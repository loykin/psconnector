//! The window: the start screen until a console is connected, then the viewer.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow};
use eframe::egui;
use psconnector::{Audio, Options, VideoCodec, VideoRange};
use psconnector_protocol::{RunOptions, settings};

use crate::cli::Args;
use crate::input::{pad, touch};
use crate::macros::{self, record};
use crate::quality::{self, Quality};
use crate::session::{Shared, end_session, spawn_frame_repaint, start_session};
use crate::ui::viewer::Viewer;
use crate::ui::{setup, theme};
use crate::video::gpu;
use crate::{session, sound};

/// System font with Korean glyphs (falls back to egui's default font if missing)
#[cfg(target_os = "macos")]
const FONT: &str = "/System/Library/Fonts/AppleSDGothicNeo.ttc";
#[cfg(target_os = "windows")]
const FONT: &str = "C:\\Windows\\Fonts\\malgun.ttf";
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const FONT: &str = "/usr/share/fonts/truetype/nanum/NanumGothic.ttf";

/// Opens the window and runs until it is closed.
pub(crate) fn run(args: Args) -> Result<()> {
    let mut macro_sources = args.macro_sources;
    if macro_sources.is_empty() {
        macro_sources = macros::default_macro_sources();
    }
    let quality = args.quality.or_else(quality::saved).unwrap_or(Quality::Hd720);
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
        port: args.port,
        macro_sources,
        autorun: args.autorun,
        run_opts: args.run_opts,
        run_at: args.run_at,
        use_pad: args.use_pad,
        use_audio: args.use_audio,
        start_muted: args.start_muted,
        quality,
        bitrate_kbps: args.bitrate_kbps,
        video_codec,
        video_range,
    };
    let rt = tokio::runtime::Runtime::new()?;
    rt.spawn(async {
        session::wait_for_quit_signal().await;
        QUIT.store(true, Ordering::SeqCst);
    });
    // an address given on the command line is connected to as it is; otherwise the start screen finds the
    // console used last time by its id (its address may have changed) and connects to it
    let host = args.host;
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
    let outputs = Outputs {
        audio: sound.then_some(audio_tx),
        muted,
        pad: Arc::new(Mutex::new(None)),
        recorder: Arc::new(record::Recorder::default()),
    };
    if launch.use_pad {
        let current = session.clone();
        pad::spawn(
            move || current.lock().unwrap().as_ref().map(|s| s.client()),
            outputs.pad.clone(),
            outputs.recorder.clone(),
        );
        let current = session.clone();
        touch::spawn(move || current.lock().unwrap().as_ref().map(|s| s.client()), outputs.recorder.clone());
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
                gpu_video: cc.wgpu_render_state.as_ref().map(gpu::GpuVideo::new),
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
pub(crate) static QUIT: AtomicBool = AtomicBool::new(false);

/// Command-line choices, kept until a console is connected.
pub(crate) struct Launch {
    pub(crate) port: u16,
    pub(crate) macro_sources: Vec<PathBuf>,
    pub(crate) autorun: Option<String>,
    pub(crate) run_opts: RunOptions,
    pub(crate) run_at: Option<String>,
    pub(crate) use_pad: bool,
    pub(crate) use_audio: bool,
    pub(crate) start_muted: bool,
    pub(crate) quality: Quality,
    pub(crate) bitrate_kbps: Option<u32>,
    pub(crate) video_codec: VideoCodec,
    pub(crate) video_range: VideoRange,
}

impl Launch {
    pub(crate) fn options(&self, q: Quality) -> Options {
        Options {
            audio: self.use_audio,
            stream: q.stream(self.bitrate_kbps),
            video_range: self.video_range,
            video_codec: self.video_codec,
            ..Options::default()
        }
    }
}

/// Sound output and local pad, shared by every connection
pub(crate) struct Outputs {
    pub(crate) audio: Option<std::sync::mpsc::Sender<Audio>>,
    pub(crate) muted: Arc<AtomicBool>,
    pub(crate) pad: Arc<Mutex<Option<String>>>,
    /// Records what the local pad sends (Record in the bar)
    pub(crate) recorder: Arc<record::Recorder>,
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
    gpu_video: Option<gpu::GpuVideo>,
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
                            self.state = State::Viewer(Box::new(Viewer::new(shared, self.gpu_video.clone(), runner)));
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
