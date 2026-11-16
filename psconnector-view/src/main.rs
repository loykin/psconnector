//! psconnector-view: PS5 screen window + control command port + macro runner.
//!
//! ```text
//! cargo run --release -p psconnector-view -- [host] [--port 8650] [--macros FILE]
//!     [--quality low|720p|1080p] [--bitrate KBPS] [--no-pad] [--no-audio] [--mute] [--run MACRO [--for MIN] [--after none|standby] [--at HH:MM|+MIN]]
//! - A game controller connected to this computer (DualSense, ...) drives the PS5 directly (off with `--no-pad`).
//! - The console's sound plays on the default output device (🔊 in the bar mutes it; `--mute`, `--no-audio`).
//! ```
//! - Command port (127.0.0.1:8650, one JSON object per line): the same commands as the Python `ps5view.py`
//!   plus `frame` and `release_all`, so existing macros run unchanged. From Python, use `python/psconnector.py`.
//! - Top bar: pick a macro and ▶ Run / ■ Stop. Each folder with a `macro.json` in the `macros` folder next to
//!   the program is a macro (`--macros PATH` reads other folders, or an older list file, instead).
//!   Macros run as child processes and get the port via the `PSCONNECTOR_PORT` environment variable.
//!   Stop sends SIGINT first (KeyboardInterrupt in Python) and kills the process if it is not gone in 3 s.
//!   However a macro ends, all buttons are released and the sticks are centered.
//!   Each run can have a time limit and an after-action (rest mode), and can be scheduled for a start time.
//!   At the limit, a macro that polls `should_stop` gets 2 minutes to finish; others are stopped right away.
//!
//! Commands: `{"button":"cross","action":"tap|press|release","hold":0.08}` `{"stick":"left","x":0,"y":-1}`
//! `{"release_all":true}` `{"snap":"path.png"}` `{"screen_snap":"path.png"}` `{"frame":true}`
//! `{"play":"recording.json"}` `{"stop":true}`
//! `{"status":true}` `{"osd":"text"}` `{"standby":true}` `{"ocr":[x,y,w,h],"scale":2}` (reply `ok ["line", ...]`)
//! Macros: `{"macros":true}` `{"macro_run":"name","limit_min":60,"after":"standby","at":"23:00"}` (all but the name
//! optional) `{"macro_stop":true}` (also cancels a schedule) `{"macro_status":true,"lines":10}` (reply `ok {JSON}`)
//! `{"should_stop":true}` (reply `ok yes` once the time limit is up, else `ok no`)
//! `{"quality":"low|720p|1080p"}` reconnect at that picture quality (`""` only reports it; reply `ok {JSON}`)
//! OCR with `"rel":true` takes corners `[x0,y0,x1,y1]` as fractions of the frame.
//! Replies are one line: `ok` / `ok playing` / `ok idle` / `err REASON`.
//! Only `frame` is followed by raw RGB bytes after its `ok WIDTH HEIGHT BYTES` line. Press Q in the window to quit.

use std::collections::{HashSet, VecDeque};
use std::io::BufRead;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, anyhow, bail};
use eframe::egui;
use psconnector::{Audio, Button, Client, Frame, Options, VideoCodec, VideoRange};
use quality::Quality;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

mod commands;
mod gpu_video;
mod login_window;
mod macros;
#[cfg(target_os = "macos")]
mod metal_video;
mod native_frames;
mod ocr;
mod pad;
mod picture;
mod quality;
mod session;
mod settings;
mod setup;
mod sound;
mod theme;
mod viewer;

#[allow(unused_imports)]
use commands::*;
#[allow(unused_imports)]
use macros::*;
#[allow(unused_imports)]
use session::*;
#[allow(unused_imports)]
use viewer::*;

const DEFAULT_PORT: u16 = 8650;
/// System font with Korean glyphs (falls back to egui's default font if missing)
#[cfg(target_os = "macos")]
const FONT: &str = "/System/Library/Fonts/AppleSDGothicNeo.ttc";
#[cfg(target_os = "windows")]
const FONT: &str = "C:\\Windows\\Fonts\\malgun.ttf";
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const FONT: &str = "/usr/share/fonts/truetype/nanum/NanumGothic.ttf";
const LOG_LINES: usize = 200;

/// Home folder (`%USERPROFILE%` on Windows).
fn home() -> PathBuf {
    std::env::home_dir().unwrap_or_default()
}

fn expand(p: &str) -> PathBuf {
    match p.strip_prefix("~/").or_else(|| p.strip_prefix("~\\")) {
        Some(rest) => home().join(rest),
        None => PathBuf::from(p),
    }
}

fn main() -> Result<()> {
    if std::env::args().nth(1).as_deref() == Some("--login-window") {
        login_window::run();
    }
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let mut args = std::env::args().skip(1);
    let mut host = None;
    let mut port = DEFAULT_PORT;
    let mut macro_sources: Vec<PathBuf> = Vec::new();
    let mut autorun: Option<String> = None;
    // --for / --after / --at for --run, in the same shape as the macro_run command
    let mut run_opts = serde_json::Map::new();
    let mut run_at: Option<String> = None;
    let mut use_pad = true;
    let mut use_audio = true;
    let mut start_muted = false;
    let mut quality: Option<Quality> = None;
    let mut bitrate_kbps: Option<u32> = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--port" => {
                port = args.next().and_then(|p| p.parse().ok()).ok_or_else(|| anyhow!("missing value for --port"))?
            }
            "--macros" => {
                macro_sources.push(expand(&args.next().ok_or_else(|| anyhow!("missing value for --macros"))?))
            }
            "--run" => autorun = Some(args.next().ok_or_else(|| anyhow!("missing value for --run"))?),
            "--for" => {
                let min: f64 =
                    args.next().and_then(|v| v.parse().ok()).ok_or_else(|| anyhow!("--for needs minutes"))?;
                run_opts.insert("limit_min".into(), min.into());
            }
            "--after" => {
                run_opts
                    .insert("after".into(), args.next().ok_or_else(|| anyhow!("--after needs none|standby"))?.into());
            }
            "--no-pad" => use_pad = false,
            "--no-audio" => use_audio = false,
            "--mute" => start_muted = true,
            "--quality" => {
                let v = args.next().unwrap_or_default();
                quality = Some(Quality::parse(&v).ok_or_else(|| anyhow!("--quality must be low, 720p or 1080p"))?);
            }
            "--bitrate" => {
                bitrate_kbps =
                    Some(args.next().and_then(|v| v.parse().ok()).ok_or_else(|| anyhow!("--bitrate needs kbit/s"))?)
            }
            "--at" => run_at = Some(args.next().ok_or_else(|| anyhow!("--at needs HH:MM or +MINUTES"))?),
            _ => host = Some(a),
        }
    }
    if autorun.is_none() && (!run_opts.is_empty() || run_at.is_some()) {
        bail!("--for, --after and --at go with --run MACRO");
    }
    if macro_sources.is_empty() {
        macro_sources = default_macro_sources();
    }
    let quality = quality.or_else(quality::saved).unwrap_or(Quality::Hd720);
    // Picture/range/codec are deliberately session-scoped. Most games should always start untouched;
    // game-specific combinations live in named presets and are selected only when needed.
    let video_codec = VideoCodec::H264;
    let video_range = VideoRange::Auto;
    for legacy_global in ["picture", "video_codec", "video_range"] {
        settings::remove(legacy_global);
    }
    // turned off when signing out of an account that came from pyremoteplay
    psconnector::profile::set_read_pyremoteplay(
        settings::get("read_pyremoteplay").and_then(|v| v.as_bool()) != Some(false),
    );
    let launch = Launch {
        port,
        macro_sources,
        autorun,
        run_opts,
        run_at,
        use_pad,
        use_audio,
        start_muted,
        quality,
        bitrate_kbps,
        video_codec,
        video_range,
    };
    let rt = tokio::runtime::Runtime::new()?;
    rt.spawn(async {
        wait_for_quit_signal().await;
        QUIT.store(true, Ordering::SeqCst);
    });
    // a console given on the command line, or else the one used last time, is connected to right away
    let host = host.or_else(settings::last_host);
    let options = eframe::NativeOptions {
        viewport: {
            let v = egui::ViewportBuilder::default().with_title("PsConnector").with_inner_size([1280.0, 760.0]);
            match app_icon() {
                Some(icon) => v.with_icon(icon),
                None => v,
            }
        },
        ..Default::default()
    };
    let session: Arc<Mutex<Option<Arc<Shared>>>> = Arc::new(Mutex::new(None));
    // sound output and the local pad live for the whole program, across connections
    let muted = Arc::new(AtomicBool::new(launch.start_muted));
    let (audio_tx, audio_rx) = std::sync::mpsc::channel();
    let sound = launch.use_audio && sound::spawn(audio_rx, muted.clone());
    let outputs = Outputs { audio: sound.then_some(audio_tx), muted, pad: Arc::new(Mutex::new(None)) };
    if launch.use_pad {
        let current = session.clone();
        pad::spawn(move || current.lock().unwrap().as_ref().map(|s| s.client()), outputs.pad.clone());
    }
    let app_session = session.clone();
    let handle = rt.handle().clone();
    let result = eframe::run_native(
        "psconnector-view",
        options,
        Box::new(move |cc| {
            setup_fonts(&cc.egui_ctx);
            theme::apply(&cc.egui_ctx);
            let setup = setup::Setup::new(handle.clone(), launch.options(launch.quality), host);
            Ok(Box::new(App {
                rt: handle,
                launch,
                outputs,
                session: app_session,
                gpu_video: cc.wgpu_render_state.as_ref().map(gpu_video::GpuVideo::new),
                state: State::Setup(Box::new(setup)),
            }))
        }),
    );
    let shared = session.lock().unwrap().take();
    if let Some(shared) = shared {
        shared.stop_play.store(true, Ordering::SeqCst);
        shared.release_all();
        rt.block_on(shared.client().close());
        println!("closed");
    }
    result.map_err(|e| anyhow!("window error: {e}"))
}

/// Set by Ctrl+C / SIGTERM: close the window and end the session cleanly.
static QUIT: AtomicBool = AtomicBool::new(false);

/// Command-line choices, kept until a console is connected.
struct Launch {
    port: u16,
    macro_sources: Vec<PathBuf>,
    autorun: Option<String>,
    run_opts: serde_json::Map<String, Value>,
    run_at: Option<String>,
    use_pad: bool,
    use_audio: bool,
    start_muted: bool,
    quality: Quality,
    bitrate_kbps: Option<u32>,
    video_codec: VideoCodec,
    video_range: VideoRange,
}

impl Launch {
    fn options(&self, q: Quality) -> Options {
        Options {
            audio: self.use_audio,
            stream: q.stream(self.bitrate_kbps),
            video_range: self.video_range,
            video_codec: self.video_codec,
            ..Options::default()
        }
    }
}

fn video_range_name(range: VideoRange) -> &'static str {
    match range {
        VideoRange::Auto => "auto",
        VideoRange::Limited => "limited",
        VideoRange::Full => "full",
    }
}

fn video_codec_name(codec: VideoCodec) -> &'static str {
    match codec {
        VideoCodec::H264 => "h264",
        VideoCodec::H265 => "h265",
        VideoCodec::H265Hdr => "h265-hdr",
    }
}

fn parse_video_codec(name: &str) -> Option<VideoCodec> {
    match name.to_ascii_lowercase().as_str() {
        "h264" | "avc" => Some(VideoCodec::H264),
        "h265" | "hevc" => Some(VideoCodec::H265),
        "h265-hdr" | "hevc-hdr" | "hdr" => Some(VideoCodec::H265Hdr),
        _ => None,
    }
}

fn parse_video_range(name: &str) -> Option<VideoRange> {
    match name.to_ascii_lowercase().as_str() {
        "auto" => Some(VideoRange::Auto),
        "limited" => Some(VideoRange::Limited),
        "full" => Some(VideoRange::Full),
        _ => None,
    }
}

/// Sound output and local pad, shared by every connection
struct Outputs {
    audio: Option<std::sync::mpsc::Sender<Audio>>,
    muted: Arc<AtomicBool>,
    pad: Arc<Mutex<Option<String>>>,
}

enum State {
    /// Start screen: find, sign in, pair, wake, connect
    Setup(Box<setup::Setup>),
    Viewer(Box<Viewer>),
}

struct App {
    rt: tokio::runtime::Handle,
    launch: Launch,
    outputs: Outputs,
    /// The running session, also closed by main() after the window is gone
    session: Arc<Mutex<Option<Arc<Shared>>>>,
    gpu_video: Option<gpu_video::GpuVideo>,
    state: State,
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        match &mut self.state {
            State::Viewer(v) => {
                v.ui(ui, frame);
                if v.shared.leave.load(Ordering::SeqCst) {
                    end_session(&self.rt, &v.shared);
                    *self.session.lock().unwrap() = None;
                    let opts = self.launch.options(*v.shared.quality.lock().unwrap());
                    self.state = State::Setup(Box::new(setup::Setup::new(self.rt.clone(), opts, None)));
                }
            }
            State::Setup(s) => {
                if QUIT.load(Ordering::SeqCst) || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                if let Some((host, client)) = s.ui(ui) {
                    match start_session(&self.rt, &self.launch, &self.outputs, host, client) {
                        Ok((shared, runner)) => {
                            let _ = shared.ctx.set(ctx.clone());
                            spawn_frame_repaint(shared.clone());
                            *self.session.lock().unwrap() = Some(shared.clone());
                            self.state = State::Viewer(Box::new(Viewer {
                                shared,
                                gpu_video: self.gpu_video.clone(),
                                texture: None,
                                shown: None,
                                runner,
                                fps: FpsMeter::default(),
                                lut: None,
                                picture_open: false,
                                preset_name: String::new(),
                                upload: (0, Duration::ZERO, None),
                            }));
                        }
                        Err(e) => {
                            log::error!("{e:#}");
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                    }
                }
            }
        }
    }
}

/// The window and Dock / taskbar icon (drawn by `assets/appicon.swift`).
fn app_icon() -> Option<egui::IconData> {
    let decoder = png::Decoder::new(std::io::Cursor::new(include_bytes!("../assets/icon.png")));
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    (info.color_type == png::ColorType::Rgba && info.bit_depth == png::BitDepth::Eight).then(|| {
        buf.truncate(info.buffer_size());
        egui::IconData { rgba: buf, width: info.width, height: info.height }
    })
}

fn setup_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    if let Ok(data) = std::fs::read(FONT) {
        fonts.font_data.insert("korean".into(), Arc::new(egui::FontData::from_owned(data)));
        for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            fonts.families.entry(family).or_default().insert(0, "korean".into());
        }
    }
    theme::add_icon_font(&mut fonts);
    ctx.set_fonts(fonts);
}
