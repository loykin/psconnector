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
//! `{"release_all":true}` `{"snap":"path.png"}` `{"frame":true}` `{"play":"recording.json"}` `{"stop":true}`
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
use psconnector::{Audio, Button, Client, Frame, Options};
use quality::Quality;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

mod login_window;
mod ocr;
mod pad;
mod quality;
mod settings;
mod setup;
mod sound;
mod theme;

const DEFAULT_PORT: u16 = 8650;
/// System font with Korean glyphs (falls back to egui's default font if missing)
#[cfg(target_os = "macos")]
const FONT: &str = "/System/Library/Fonts/AppleSDGothicNeo.ttc";
#[cfg(target_os = "windows")]
const FONT: &str = "C:\\Windows\\Fonts\\malgun.ttf";
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const FONT: &str = "/usr/share/fonts/truetype/nanum/NanumGothic.ttf";
const LOG_LINES: usize = 200;

struct Shared {
    /// Current connection; replaced when the viewer reconnects (quality change)
    client: std::sync::RwLock<Arc<Client>>,
    host: String,
    quality: Mutex<Quality>,
    bitrate_kbps: Option<u32>,
    use_audio: bool,
    /// Where each connection's audio is forwarded to (None when sound is off)
    audio_out: Option<std::sync::mpsc::Sender<Audio>>,
    /// Set while reconnecting; shown in the window
    reconnecting: Mutex<Option<String>>,
    rt: tokio::runtime::Handle,
    runner: Arc<Mutex<Runner>>,
    osd: Mutex<String>,
    /// Name of the game controller connected to this computer, if any
    pad: Arc<Mutex<Option<String>>>,
    /// Sound is playing (false with --no-audio or without an output device)
    sound: bool,
    muted: Arc<AtomicBool>,
    playing: AtomicBool,
    stop_play: AtomicBool,
    /// The window's egui context, to wake it up from other threads
    ctx: std::sync::OnceLock<egui::Context>,
    /// Disconnect was pressed: go back to the start screen
    leave: AtomicBool,
    /// The session is over; its threads and tasks stop
    ended: AtomicBool,
    serve_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Shared {
    fn client(&self) -> Arc<Client> {
        self.client.read().unwrap().clone()
    }

    fn release_all(&self) {
        let c = self.client();
        for b in Button::ALL {
            c.release(b);
        }
        c.stick(false, 0.0, 0.0);
        c.stick(true, 0.0, 0.0);
    }

    fn options(&self, q: Quality) -> Options {
        Options { audio: self.use_audio, stream: q.stream(self.bitrate_kbps), ..Options::default() }
    }

    /// Sends this connection's decoded audio on to the sound output.
    fn attach_audio(&self, c: &Client) {
        if let (Some(out), Some(rx)) = (self.audio_out.clone(), c.take_audio()) {
            std::thread::spawn(move || {
                while let Ok(a) = rx.recv() {
                    if out.send(a).is_err() {
                        break;
                    }
                }
            });
        }
    }

    /// Reconnects at another quality (no-op if already there).
    fn set_quality(self: &Arc<Self>, q: Quality) -> Result<(), String> {
        if *self.quality.lock().unwrap() == q {
            return Ok(());
        }
        {
            let mut r = self.reconnecting.lock().unwrap();
            if r.as_deref().is_some_and(|n| n.starts_with("reconnecting")) {
                return Err("already reconnecting".into());
            }
            *r = Some(format!("reconnecting at {}...", q.label()));
        }
        let sh = self.clone();
        self.rt.spawn(async move {
            sh.release_all();
            let old_q = *sh.quality.lock().unwrap();
            sh.client().close().await;
            // the console needs a moment to drop the old session; connect() retries while it is busy
            let result = match connect(&sh.host, sh.options(q)).await {
                Ok(c) => Ok((c, q)),
                Err(e) => {
                    log::warn!("reconnect at {} failed: {e:#}; going back to {}", q.label(), old_q.label());
                    connect(&sh.host, sh.options(old_q)).await.map(|c| (c, old_q))
                }
            };
            match result {
                Ok((c, got)) => {
                    sh.attach_audio(&c);
                    *sh.client.write().unwrap() = Arc::new(c);
                    *sh.quality.lock().unwrap() = got;
                    quality::save(got);
                    log::info!("connected at {}", got.label());
                    *sh.reconnecting.lock().unwrap() = None;
                }
                Err(e) => *sh.reconnecting.lock().unwrap() = Some(format!("reconnect failed: {e:#}")),
            }
        });
        Ok(())
    }
}

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
    };
    let rt = tokio::runtime::Runtime::new()?;
    rt.spawn(async {
        wait_for_quit_signal().await;
        QUIT.store(true, Ordering::SeqCst);
    });
    // a console given on the command line, or else the one used last time, is connected to right away
    let host = host.or_else(settings::last_host);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_title("PS5").with_inner_size([1280.0, 760.0]),
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
}

impl Launch {
    fn options(&self, q: Quality) -> Options {
        Options { audio: self.use_audio, stream: q.stream(self.bitrate_kbps), ..Options::default() }
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
                            *self.session.lock().unwrap() = Some(shared.clone());
                            self.state = State::Viewer(Box::new(Viewer {
                                shared,
                                texture: None,
                                shown: None,
                                runner,
                                fps: FpsMeter::default(),
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

/// Everything that runs once a console is connected: sound, pad, command port, macros.
fn start_session(
    rt: &tokio::runtime::Handle,
    l: &Launch,
    out: &Outputs,
    host: String,
    client: Client,
) -> Result<(Arc<Shared>, Arc<Mutex<Runner>>)> {
    let client = Arc::new(client);
    let runner = Arc::new(Mutex::new(Runner::new(load_macros(&l.macro_sources), l.macro_sources.clone(), l.port)));
    let shared = Arc::new(Shared {
        client: std::sync::RwLock::new(client.clone()),
        host,
        quality: Mutex::new(l.quality),
        bitrate_kbps: l.bitrate_kbps,
        use_audio: l.use_audio,
        audio_out: out.audio.clone(),
        reconnecting: Mutex::new(None),
        rt: rt.clone(),
        runner: runner.clone(),
        osd: Mutex::new(String::new()),
        pad: out.pad.clone(),
        sound: out.audio.is_some(),
        muted: out.muted.clone(),
        playing: AtomicBool::new(false),
        stop_play: AtomicBool::new(false),
        ctx: std::sync::OnceLock::new(),
        leave: AtomicBool::new(false),
        ended: AtomicBool::new(false),
        serve_task: Mutex::new(None),
    });
    shared.attach_audio(&client);
    let listener = rt
        .block_on(TcpListener::bind(("127.0.0.1", l.port)))
        .with_context(|| format!("cannot open the command port {}", l.port))?;
    *shared.serve_task.lock().unwrap() = Some(rt.spawn(serve(listener, shared.clone())));
    spawn_supervisor(shared.clone(), rt.clone());
    println!("connected. command port {}. press Q in the window to quit", l.port);
    if let Some(name) = &l.autorun {
        let msg = runner.lock().unwrap().run_named(name, &Value::Object(l.run_opts.clone()), l.run_at.as_deref());
        match msg {
            Ok(m) => println!("--run {name}: {m}"),
            Err(e) => println!("--run {name}: {e}"),
        }
    }
    Ok((shared, runner))
}

/// Ends a session (Disconnect): stops its macro, frees the command port, and closes the connection.
fn end_session(rt: &tokio::runtime::Handle, sh: &Arc<Shared>) {
    sh.runner.lock().unwrap().shutdown();
    sh.stop_play.store(true, Ordering::SeqCst);
    sh.release_all();
    sh.ended.store(true, Ordering::SeqCst);
    if let Some(t) = sh.serve_task.lock().unwrap().take() {
        t.abort();
    }
    let c = sh.client();
    rt.spawn(async move { c.close().await });
    println!("disconnected");
}

/// Runs macros' schedules and time limits, and handles a quit signal, off the window thread:
/// macOS stops repainting a hidden or minimized window, and these must not wait for it.
fn spawn_supervisor(sh: Arc<Shared>, rt: tokio::runtime::Handle) {
    std::thread::Builder::new()
        .name("supervisor".into())
        .spawn(move || {
            let mut quit_since: Option<Instant> = None;
            while !sh.ended.load(Ordering::SeqCst) {
                let ended = sh.runner.lock().unwrap().poll();
                if let Some(ended) = ended {
                    // however the macro ended, release any input left pressed
                    sh.release_all();
                    sh.stop_play.store(true, Ordering::SeqCst);
                    if ended.standby {
                        sh.client().standby();
                    }
                }
                if QUIT.load(Ordering::SeqCst)
                    && let Some(ctx) = sh.ctx.get()
                {
                    // the window closes on its next frame; make sure there is one
                    ctx.request_repaint();
                    // a window macOS keeps asleep may never draw it: close the session here instead
                    if quit_since.get_or_insert_with(Instant::now).elapsed() > Duration::from_secs(3) {
                        sh.runner.lock().unwrap().shutdown();
                        sh.release_all();
                        rt.block_on(sh.client().close());
                        println!("closed");
                        std::process::exit(0);
                    }
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        })
        .expect("supervisor thread");
}

/// Waits for Ctrl+C (and SIGTERM on Unix).
async fn wait_for_quit_signal() {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("SIGTERM");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}

/// The console keeps reporting "another session connected" (and sometimes refuses the connection) for a
/// while after the previous session ends — anywhere from a few seconds to about 40 — so keep retrying.
async fn connect(host: &str, opts: Options) -> Result<Client> {
    let t0 = Instant::now();
    loop {
        match Client::connect(host, opts.clone()).await {
            Ok(c) => return Ok(c),
            Err(e) if t0.elapsed() < Duration::from_secs(90) && is_transient(&e) => {
                println!("previous session still active ({e}), retrying in 3 s");
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
            Err(e) => return Err(e.into()),
        }
    }
}

fn is_transient(e: &psconnector::Error) -> bool {
    use std::io::ErrorKind::{ConnectionRefused, ConnectionReset};
    match e {
        psconnector::Error::Busy => true,
        psconnector::Error::Io(io) => matches!(io.kind(), ConnectionRefused | ConnectionReset),
        _ => false,
    }
}

fn setup_fonts(ctx: &egui::Context) {
    let Ok(data) = std::fs::read(FONT) else { return };
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert("korean".into(), Arc::new(egui::FontData::from_owned(data)));
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts.families.entry(family).or_default().insert(0, "korean".into());
    }
    ctx.set_fonts(fonts);
}

// ------------------------------------------------------------ macro runner

/// After a time limit is reached, how long a macro that polls `should_stop` gets to finish on its own
/// before it is interrupted.
const GRACE: Duration = Duration::from_secs(120);

/// What to do when a macro ends by itself or by its time limit (not on a manual ■ Stop).
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
enum After {
    #[default]
    Nothing,
    Standby,
}

impl After {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "none" | "nothing" | "stop" => Some(Self::Nothing),
            "standby" | "rest" => Some(Self::Standby),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Nothing => "none",
            Self::Standby => "standby",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Nothing => "then nothing",
            Self::Standby => "then rest mode",
        }
    }
}

/// How long to run and what to do afterwards.
#[derive(Clone, Copy, Default, Debug)]
struct Plan {
    /// 0 = no limit
    limit_min: u32,
    after: After,
}

impl Plan {
    /// Reads `limit_min` and `after` from a JSON object, keeping `self` for missing fields.
    fn merged(self, v: &Value) -> Result<Self, String> {
        let limit_min = match &v["limit_min"] {
            Value::Null => self.limit_min,
            n => n.as_f64().filter(|n| *n >= 0.0).ok_or("limit_min must be a number of minutes")?.round() as u32,
        };
        let after = match v["after"].as_str() {
            None => self.after,
            Some(s) => After::parse(s).ok_or_else(|| format!("after must be none or standby, not '{s}'"))?,
        };
        Ok(Self { limit_min, after })
    }
}

#[derive(Clone)]
struct MacroDef {
    name: String,
    cmd: String,
    cwd: Option<PathBuf>,
    env: Vec<(String, String)>,
    /// Default limit and after-action from the list
    plan: Plan,
}

/// Where macros are found when no `--macros` is given: the `macros` folder next to the program, so the whole
/// folder can be moved (or unzipped anywhere on Windows) as one. A development build in `target/release/`
/// uses the repository's `macros` folder instead.
fn default_macro_sources() -> Vec<PathBuf> {
    let exe = std::env::current_exe().ok().and_then(|p| p.canonicalize().ok());
    let Some(dir) = exe.as_deref().and_then(|p| p.parent()) else { return vec![PathBuf::from("macros")] };
    let beside = dir.join("macros");
    let in_cargo_target = dir.parent().is_some_and(|p| p.file_name().is_some_and(|n| n == "target"));
    match dir.parent().and_then(|p| p.parent()) {
        Some(repo) if in_cargo_target && !beside.is_dir() => vec![repo.join("macros")],
        _ => vec![beside],
    }
}

/// Reads macros from each source: a folder holds one macro per subfolder (each with a `macro.json`),
/// a file is the older list form (`[{"name", "cmd", "cwd", ...}]`). Broken entries are skipped with a warning.
///
/// `macro.json`: `{"name": "...", "cmd": "python3 -u main.py", "env": {"K": "V"}, "limit_min": 60, "after": "standby"}`.
/// Only `cmd` is required. The macro runs in its own folder (relative paths in `cmd` work), and the
/// name defaults to the folder name, so a macro folder can be copied or moved as it is.
fn load_macros(sources: &[PathBuf]) -> Result<Vec<MacroDef>, String> {
    let mut list = Vec::new();
    for src in sources {
        if src.is_dir() {
            let mut dirs: Vec<PathBuf> = std::fs::read_dir(src)
                .map_err(|e| format!("cannot read {}: {e}", src.display()))?
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.join("macro.json").is_file()) // follows symlinks to repositories
                .collect();
            dirs.sort();
            for dir in dirs {
                let file = dir.join("macro.json");
                let parsed = std::fs::read_to_string(&file)
                    .map_err(|e| e.to_string())
                    .and_then(|t| serde_json::from_str::<Value>(&t).map_err(|e| e.to_string()))
                    .and_then(|v| {
                        let folder = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                        macro_def(&v, Some(&folder), Some(&dir))
                    });
                match parsed {
                    Ok(m) => list.push(m),
                    Err(e) => log::warn!("skipping {}: {e}", file.display()),
                }
            }
        } else if src.is_file() {
            let text = std::fs::read_to_string(src).map_err(|e| format!("cannot read {}: {e}", src.display()))?;
            let v: Value = serde_json::from_str(&text).map_err(|e| format!("{}: JSON error: {e}", src.display()))?;
            let entries = v.as_array().ok_or_else(|| format!("{} must be a JSON array", src.display()))?;
            for m in entries {
                match macro_def(m, None, None) {
                    Ok(m) => list.push(m),
                    Err(e) => log::warn!("skipping an entry in {}: {e}", src.display()),
                }
            }
        }
    }
    if list.is_empty() {
        let at = sources.first().map(|p| p.display().to_string()).unwrap_or_default();
        return Err(format!("no macros: add a folder with a macro.json to {at}"));
    }
    Ok(list)
}

/// One macro from its JSON. `folder` gives the default name and working folder (folder macros).
fn macro_def(m: &Value, folder_name: Option<&str>, folder: Option<&PathBuf>) -> Result<MacroDef, String> {
    let cmd = m["cmd"].as_str().ok_or("\"cmd\" is missing")?.to_string();
    let name = m["name"].as_str().or(folder_name).ok_or("\"name\" is missing")?.to_string();
    let cwd = match m["cwd"].as_str() {
        Some(c) => {
            let p = expand(c);
            Some(match folder {
                Some(f) if p.is_relative() => f.join(p),
                _ => p,
            })
        }
        None => folder.cloned(),
    };
    Ok(MacroDef {
        cmd,
        cwd,
        env: m["env"]
            .as_object()
            .map(|o| {
                o.iter()
                    .map(|(k, v)| (k.clone(), v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string())))
                    .collect()
            })
            .unwrap_or_default(),
        plan: Plan::default().merged(m).map_err(|e| format!("macro '{name}': {e}"))?,
        name,
    })
}

/// Next local time matching `HH:MM` (today or tomorrow), or `+MIN` minutes from now.
fn parse_start_time(text: &str) -> Result<(SystemTime, String), String> {
    let text = text.trim();
    let wait = if let Some(min) = text.strip_prefix('+') {
        let min: u64 = min.trim().parse().map_err(|_| "use HH:MM or +MINUTES")?;
        Duration::from_secs(min * 60)
    } else {
        let t = chrono::NaiveTime::parse_from_str(text, "%H:%M").map_err(|_| "use HH:MM or +MINUTES")?;
        let now = chrono::Local::now().naive_local();
        let mut at = now.date().and_time(t);
        if at <= now {
            at += chrono::TimeDelta::days(1);
        }
        (at - now).to_std().unwrap_or_default()
    };
    let at = SystemTime::now() + wait;
    let label = chrono::DateTime::<chrono::Local>::from(at).format("%m-%d %H:%M").to_string();
    Ok((at, label))
}

fn hms(d: Duration) -> String {
    let s = d.as_secs();
    format!("{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}

struct Scheduled {
    index: usize,
    plan: Plan,
    at: SystemTime,
    label: String,
}

struct Running {
    name: String,
    child: Child,
    started: Instant,
    plan: Plan,
    deadline: Option<Instant>,
    /// When the time limit was reached (`should_stop` answers yes from then on)
    wind_down: Option<Instant>,
    /// The macro has asked `should_stop` at least once, so it gets the grace period
    cooperative: bool,
    manual_stop: bool,
    stop_sent: Option<Instant>,
}

/// How a macro run ended, for the caller to act on.
struct Ended {
    standby: bool,
}

struct Runner {
    macros: Result<Vec<MacroDef>, String>,
    /// Macro folders and list files, re-read by Reload list
    sources: Vec<PathBuf>,
    port: u16,
    selected: usize,
    /// Limit and after-action set in the bar for the next run
    plan: Plan,
    /// Start-time text field in the bar
    at_text: String,
    scheduled: Option<Scheduled>,
    running: Option<Running>,
    log: Arc<Mutex<VecDeque<String>>>,
    last_exit: Option<String>,
}

impl Runner {
    fn new(macros: Result<Vec<MacroDef>, String>, sources: Vec<PathBuf>, port: u16) -> Self {
        let mut r = Self {
            macros,
            sources,
            port,
            selected: 0,
            plan: Plan::default(),
            at_text: String::new(),
            scheduled: None,
            running: None,
            log: Arc::new(Mutex::new(VecDeque::new())),
            last_exit: None,
        };
        r.select(0);
        r
    }

    /// Selects a macro and loads its default limit and after-action into the bar.
    fn select(&mut self, i: usize) {
        self.selected = i;
        if let Ok(list) = &self.macros
            && let Some(m) = list.get(i)
        {
            self.plan = m.plan;
        }
    }

    fn reload(&mut self) {
        if self.running.is_none() && self.scheduled.is_none() {
            self.macros = load_macros(&self.sources);
            self.select(0);
        }
    }

    fn push_log(log: &Arc<Mutex<VecDeque<String>>>, line: String) {
        let mut l = log.lock().unwrap();
        l.push_back(line);
        while l.len() > LOG_LINES {
            l.pop_front();
        }
    }

    fn start(&mut self) {
        let (i, plan) = (self.selected, self.plan);
        self.start_with(i, plan);
    }

    fn start_with(&mut self, index: usize, plan: Plan) {
        let Ok(list) = &self.macros else { return };
        let Some(m) = list.get(index).cloned() else { return };
        let mut cmd = shell_command(&m.cmd);
        cmd.env("PSCONNECTOR_PORT", self.port.to_string())
            .env("PYTHONUNBUFFERED", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(cwd) = &m.cwd {
            cmd.current_dir(cwd);
        }
        for (k, v) in &m.env {
            cmd.env(k, v);
        }
        self.log.lock().unwrap().clear();
        match cmd.spawn() {
            Ok(mut child) => {
                for out in [
                    child.stdout.take().map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
                    child.stderr.take().map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
                ]
                .into_iter()
                .flatten()
                {
                    let log = self.log.clone();
                    std::thread::spawn(move || {
                        for line in std::io::BufReader::new(out).lines().map_while(|l| l.ok()) {
                            Self::push_log(&log, line);
                        }
                    });
                }
                let limit = (plan.limit_min > 0).then(|| Duration::from_secs(u64::from(plan.limit_min) * 60));
                let note = match limit {
                    Some(_) => format!(" for {} min, {}", plan.limit_min, plan.after.label()),
                    None if plan.after == After::Standby => format!(", {}", plan.after.label()),
                    None => String::new(),
                };
                Self::push_log(&self.log, format!("▶ {} started{note}", m.name));
                self.last_exit = None;
                let started = Instant::now();
                self.running = Some(Running {
                    name: m.name,
                    child,
                    started,
                    plan,
                    deadline: limit.map(|d| started + d),
                    wind_down: None,
                    cooperative: false,
                    manual_stop: false,
                    stop_sent: None,
                });
            }
            Err(e) => Self::push_log(&self.log, format!("failed to start: {e}")),
        }
    }

    /// Index of the first macro whose name equals or contains `name`
    fn find(&self, name: &str) -> Result<usize, String> {
        let list = self.macros.as_ref().map_err(|e| e.clone())?;
        list.iter()
            .position(|m| m.name == name)
            .or_else(|| list.iter().position(|m| m.name.contains(name)))
            .ok_or_else(|| format!("no macro named '{name}'"))
    }

    /// Runs a macro by name now, or schedules it when `at` is given. `opts` may override
    /// `limit_min` and `after` from the list.
    fn run_named(&mut self, name: &str, opts: &Value, at: Option<&str>) -> Result<String, String> {
        if self.running.is_some() {
            return Err("another macro is already running".into());
        }
        let i = self.find(name)?;
        self.select(i);
        let plan = self.plan.merged(opts)?;
        self.plan = plan;
        if let Some(at) = at {
            return self.schedule(i, plan, at);
        }
        self.scheduled = None;
        self.start_with(i, plan);
        if self.running.is_none() {
            return Err(self.log.lock().unwrap().back().cloned().unwrap_or_else(|| "failed to start".into()));
        }
        Ok("started".into())
    }

    fn schedule(&mut self, index: usize, plan: Plan, at: &str) -> Result<String, String> {
        let (when, label) = parse_start_time(at)?;
        let name = self.macros.as_ref().ok().and_then(|l| l.get(index)).map(|m| m.name.clone()).unwrap_or_default();
        Self::push_log(&self.log, format!("⏰ {name} scheduled for {label}"));
        self.scheduled = Some(Scheduled { index, plan, at: when, label: label.clone() });
        Ok(format!("scheduled for {label}"))
    }

    /// Schedules the macro selected in the bar at the time typed in the bar.
    fn schedule_selected(&mut self) {
        let (i, plan, at) = (self.selected, self.plan, self.at_text.clone());
        if let Err(e) = self.schedule(i, plan, &at) {
            Self::push_log(&self.log, format!("schedule: {e}"));
        }
    }

    fn cancel_schedule(&mut self) {
        if self.scheduled.take().is_some() {
            Self::push_log(&self.log, "⏰ schedule cancelled".into());
        }
    }

    /// Asked by the macro through the command port: true once its time limit is up.
    fn should_stop(&mut self) -> bool {
        match self.running.as_mut() {
            Some(r) => {
                r.cooperative = true;
                r.wind_down.is_some() || r.stop_sent.is_some()
            }
            None => false,
        }
    }

    /// Status as JSON (for the command port and MCP)
    fn status_json(&self, lines: usize) -> Value {
        let log: Vec<String> = {
            let l = self.log.lock().unwrap();
            l.iter().skip(l.len().saturating_sub(lines)).cloned().collect()
        };
        let r = self.running.as_ref();
        serde_json::json!({
            "running": r.map(|r| r.name.clone()),
            "elapsed_sec": r.map(|r| r.started.elapsed().as_secs()),
            "limit_min": r.map(|r| r.plan.limit_min).filter(|m| *m > 0),
            "left_sec": r.and_then(|r| r.deadline).map(|d| d.saturating_duration_since(Instant::now()).as_secs()),
            "after": r.map(|r| r.plan.after.as_str()),
            "winding_down": r.is_some_and(|r| r.wind_down.is_some()),
            "stopping": r.is_some_and(|r| r.stop_sent.is_some()),
            "scheduled": self.scheduled.as_ref().map(|s| serde_json::json!({
                "name": self.macros.as_ref().ok().and_then(|l| l.get(s.index)).map(|m| m.name.clone()),
                "at": s.label,
                "in_sec": s.at.duration_since(SystemTime::now()).unwrap_or_default().as_secs(),
                "limit_min": s.plan.limit_min,
                "after": s.plan.after.as_str(),
            })),
            "last_exit": self.last_exit,
            "log": log,
        })
    }

    fn list_json(&self) -> Value {
        match &self.macros {
            Ok(list) => serde_json::json!(
                list.iter()
                    .map(|m| serde_json::json!({
                        "name": m.name, "cmd": m.cmd, "limit_min": m.plan.limit_min, "after": m.plan.after.as_str()
                    }))
                    .collect::<Vec<_>>()
            ),
            Err(e) => serde_json::json!({"error": e}),
        }
    }

    /// Manual stop (■ Stop, macro_stop): also cancels a pending schedule. The after-action is skipped.
    fn stop(&mut self) {
        self.cancel_schedule();
        if let Some(r) = self.running.as_mut() {
            r.manual_stop = true;
        }
        self.interrupt_running();
    }

    fn interrupt_running(&mut self) {
        if let Some(r) = self.running.as_mut().filter(|r| r.stop_sent.is_none()) {
            interrupt(&mut r.child);
            r.stop_sent = Some(Instant::now());
            Self::push_log(&self.log, "■ stop signal sent".into());
        }
    }

    /// Called every frame: starts a scheduled macro when its time comes, enforces the time limit,
    /// kills the process 3 s after a stop signal and reports when it ended.
    fn poll(&mut self) -> Option<Ended> {
        if self.running.is_none() {
            if self.scheduled.as_ref().is_some_and(|s| SystemTime::now() >= s.at) {
                let s = self.scheduled.take()?;
                self.start_with(s.index, s.plan);
            }
            return None;
        }
        let r = self.running.as_mut()?;
        if r.wind_down.is_none() && r.deadline.is_some_and(|d| Instant::now() >= d) {
            r.wind_down = Some(Instant::now());
            let how = if r.cooperative { "asking the macro to finish" } else { "stopping" };
            Self::push_log(&self.log, format!("⏱ time limit reached, {how}"));
        }
        // a macro that polls should_stop gets GRACE to wrap up; others are interrupted right away
        if r.stop_sent.is_none() && r.wind_down.is_some_and(|w| !r.cooperative || w.elapsed() > GRACE) {
            self.interrupt_running();
        }
        let r = self.running.as_mut()?;
        if let Some(t) = r.stop_sent
            && t.elapsed() > Duration::from_secs(3)
        {
            let _ = r.child.kill();
        }
        match r.child.try_wait() {
            Ok(Some(status)) => {
                let how = match status.code() {
                    Some(0) => "exited normally".to_string(),
                    Some(c) => format!("exit code {c}"),
                    None => "killed by signal".to_string(),
                };
                let how = if r.wind_down.is_some() { format!("{how} (time limit)") } else { how };
                let standby = r.plan.after == After::Standby && !r.manual_stop;
                Self::push_log(&self.log, format!("{} {how}{}", r.name, if standby { ", → rest mode" } else { "" }));
                self.last_exit = Some(how);
                self.running = None;
                Some(Ended { standby })
            }
            _ => None,
        }
    }

    fn shutdown(&mut self) {
        self.stop();
        let t0 = Instant::now();
        while self.running.is_some() && t0.elapsed() < Duration::from_secs(4) {
            self.poll();
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// Runs the macro command line through the system shell.
#[cfg(unix)]
fn shell_command(line: &str) -> Command {
    // `exec` replaces the shell with the macro so the stop signal reaches the macro, not the shell
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c").arg(format!("exec {line}"));
    cmd
}

#[cfg(windows)]
fn shell_command(line: &str) -> Command {
    use std::os::windows::process::CommandExt;
    let mut cmd = Command::new("cmd");
    cmd.arg("/C").raw_arg(line);
    cmd
}

/// Asks the macro to clean up and exit: SIGINT on Unix (KeyboardInterrupt in Python),
/// an immediate kill elsewhere.
#[cfg(unix)]
fn interrupt(child: &mut Child) {
    // SAFETY: only sends a signal to the pid of a child process we spawned
    unsafe { libc::kill(child.id() as i32, libc::SIGINT) };
}

#[cfg(not(unix))]
fn interrupt(child: &mut Child) {
    let _ = child.kill();
}

// ------------------------------------------------------------ window

struct Viewer {
    shared: Arc<Shared>,
    texture: Option<egui::TextureHandle>,
    shown: Option<Arc<Frame>>,
    runner: Arc<Mutex<Runner>>,
    fps: FpsMeter,
}

/// Frames shown per second, over the last second.
#[derive(Default)]
struct FpsMeter {
    since: Option<Instant>,
    count: u32,
    fps: u32,
}

impl FpsMeter {
    fn frame(&mut self) {
        self.count += 1;
        let since = *self.since.get_or_insert_with(Instant::now);
        if since.elapsed() >= Duration::from_secs(1) {
            self.fps = self.count;
            self.count = 0;
            self.since = Some(Instant::now());
        }
    }
}

impl Viewer {
    fn bar(&mut self, ui: &mut egui::Ui) {
        // the bar holds many controls: tighter than the rest of the window
        ui.spacing_mut().button_padding = egui::vec2(8.0, 3.0);
        ui.spacing_mut().item_spacing.x = 6.0;
        ui.add_space(2.0);
        let runner = self.runner.clone();
        let mut r = runner.lock().unwrap();
        let r = &mut *r;
        ui.horizontal(|ui| {
            let names: Vec<String> = match &r.macros {
                Ok(list) => list.iter().map(|m| m.name.clone()).collect(),
                Err(_) => Vec::new(),
            };
            let idle = r.running.is_none();
            let waiting = r.scheduled.is_some();
            ui.add_enabled_ui(idle && !waiting && !names.is_empty(), |ui| {
                let mut selected = r.selected;
                egui::ComboBox::from_id_salt("macro")
                    .selected_text(names.get(selected).cloned().unwrap_or_else(|| "no macros".into()))
                    .width(180.0)
                    .show_ui(ui, |ui| {
                        for (i, n) in names.iter().enumerate() {
                            ui.selectable_value(&mut selected, i, n);
                        }
                    });
                if selected != r.selected {
                    r.select(selected);
                }
                ui.add(
                    egui::DragValue::new(&mut r.plan.limit_min)
                        .range(0..=24 * 60)
                        .custom_formatter(|v, _| if v < 1.0 { "no limit".into() } else { format!("{v:.0} min") }),
                )
                .on_hover_text(
                    "Run time limit (drag or type, 0 = no limit). When it is reached, a macro that checks \
                     ps.should_stop() gets 2 minutes to finish; others are stopped right away.",
                );
                egui::ComboBox::from_id_salt("after")
                    .selected_text(r.plan.after.label())
                    .width(110.0)
                    .show_ui(ui, |ui| {
                        for a in [After::Nothing, After::Standby] {
                            ui.selectable_value(&mut r.plan.after, a, a.label());
                        }
                    })
                    .response
                    .on_hover_text("What to do when the macro ends by itself or at its time limit (not on ■ Stop)");
            });
            if !idle {
                if theme::destructive(ui, true, "■ Stop").clicked() {
                    r.stop();
                }
            } else if waiting {
                if ui.button("Cancel schedule").clicked() {
                    r.cancel_schedule();
                }
            } else {
                if theme::primary(ui, !names.is_empty(), "▶ Run").clicked() {
                    r.start();
                }
                ui.add(egui::TextEdit::singleline(&mut r.at_text).hint_text("HH:MM").desired_width(60.0))
                    .on_hover_text("Start time: HH:MM (today, or tomorrow if already past) or +MINUTES");
                if ui
                    .add_enabled(!names.is_empty() && !r.at_text.trim().is_empty(), egui::Button::new("⏰ Schedule"))
                    .clicked()
                {
                    r.schedule_selected();
                }
                if theme::ghost(ui, true, "Reload list").clicked() {
                    r.reload();
                }
            }
            let status = match (&r.running, &r.scheduled, &r.macros) {
                (Some(run), _, _) => {
                    let mut s = format!("{} running {}", run.name, hms(run.started.elapsed()));
                    if let Some(d) = run.deadline {
                        s += &format!(" · {} left", hms(d.saturating_duration_since(Instant::now())));
                    }
                    if run.plan.after == After::Standby {
                        s += " · then rest mode";
                    }
                    if run.stop_sent.is_some() {
                        s += " (stopping)";
                    } else if run.wind_down.is_some() {
                        s += " (finishing)";
                    }
                    s
                }
                (None, Some(sc), _) => {
                    let name = names.get(sc.index).cloned().unwrap_or_default();
                    let wait = sc.at.duration_since(SystemTime::now()).unwrap_or_default();
                    format!("{name} starts at {} (in {})", sc.label, hms(wait))
                }
                (None, None, Err(e)) => e.clone(),
                (None, None, Ok(_)) => {
                    r.last_exit.clone().map(|e| format!("stopped ({e})")).unwrap_or_else(|| "idle".into())
                }
            };
            ui.label(status);
            let last = r.log.lock().unwrap().back().cloned().unwrap_or_default();
            ui.add(egui::Label::new(egui::RichText::new(last).weak()).truncate());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if theme::ghost(ui, true, "Rest mode")
                    .on_hover_text("Put the PS5 into rest mode (this also disconnects)")
                    .clicked()
                {
                    r.stop();
                    self.shared.client().standby();
                    self.shared.leave.store(true, Ordering::SeqCst);
                }
                if theme::ghost(ui, true, "Disconnect")
                    .on_hover_text("Close the connection and go back to the start screen")
                    .clicked()
                {
                    self.shared.leave.store(true, Ordering::SeqCst);
                }
                if self.shared.sound {
                    let muted = self.shared.muted.load(Ordering::Relaxed);
                    if ui.button(if muted { "🔇" } else { "🔊" }).on_hover_text("Mute / unmute the PS5 sound").clicked()
                    {
                        self.shared.muted.store(!muted, Ordering::Relaxed);
                    }
                }
                let current = *self.shared.quality.lock().unwrap();
                let mut chosen = current;
                let busy =
                    self.shared.reconnecting.lock().unwrap().as_deref().is_some_and(|n| n.starts_with("reconnecting"));
                ui.add_enabled_ui(!busy, |ui| {
                    egui::ComboBox::from_id_salt("quality")
                        .selected_text(current.label())
                        .show_ui(ui, |ui| {
                            for q in Quality::ALL {
                                ui.selectable_value(&mut chosen, q, q.label());
                            }
                        })
                        .response
                        .on_hover_text(
                            "Picture quality. Changing it reconnects (a few seconds). Remembered for next time.",
                        );
                });
                if chosen != current {
                    let _ = self.shared.set_quality(chosen);
                }
                if let Some(f) = &self.shown {
                    ui.label(egui::RichText::new(format!("{}x{} {} fps", f.width, f.height, self.fps.fps)).weak());
                }
                let pad = self.shared.pad.lock().unwrap().clone();
                match pad {
                    Some(name) => ui.label(format!("🎮 {name}")),
                    None => ui.label(egui::RichText::new("🎮 no controller").weak()),
                };
            });
        });
    }
}

impl Viewer {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let quit = QUIT.load(Ordering::SeqCst);
        if quit || ctx.input(|i| i.key_pressed(egui::Key::Q) && !i.modifiers.any()) {
            self.runner.lock().unwrap().shutdown();
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }

        egui::Panel::top("bar").show(ui, |ui| self.bar(ui));

        let latest = self.shared.client().frame();
        if let Some(f) = latest
            && !self.shown.as_ref().is_some_and(|s| Arc::ptr_eq(s, &f))
        {
            let img = egui::ColorImage::from_rgb([f.width, f.height], &f.rgb);
            match &mut self.texture {
                Some(t) => t.set(img, egui::TextureOptions::LINEAR),
                None => self.texture = Some(ctx.load_texture("ps5", img, egui::TextureOptions::LINEAR)),
            }
            self.shown = Some(f);
            self.fps.frame();
        }
        // draw into the remaining area at 16:9
        let area = ui.available_rect_before_wrap();
        let painter = ui.painter_at(area);
        painter.rect_filled(area, 0.0, egui::Color32::BLACK);
        let size = if area.width() / area.height() > 16.0 / 9.0 {
            egui::vec2(area.height() * 16.0 / 9.0, area.height())
        } else {
            egui::vec2(area.width(), area.width() * 9.0 / 16.0)
        };
        let rect = egui::Rect::from_center_size(area.center(), size);
        if let Some(t) = &self.texture {
            painter.image(
                t.id(),
                rect,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );
        }
        let mut text = self.shared.osd.lock().unwrap().clone();
        let note = self.shared.reconnecting.lock().unwrap().clone();
        if let Some(note) = note {
            text = format!("{note}\n{text}");
        } else if !self.shared.client().is_ready() {
            text = format!("disconnected: {:?}\n{text}", self.shared.client().phase());
        }
        if !text.is_empty() {
            let font = egui::FontId::proportional(20.0);
            let galley = painter.layout_no_wrap(text, font, egui::Color32::from_rgb(255, 220, 120));
            let pos = rect.min + egui::vec2(12.0, 10.0);
            painter.rect_filled(
                egui::Rect::from_min_size(pos, galley.size()).expand(6.0),
                4.0,
                egui::Color32::from_black_alpha(200),
            );
            painter.galley(pos, galley, egui::Color32::WHITE);
        }
        ctx.request_repaint_after(Duration::from_millis(15));
    }
}

// ------------------------------------------------------------ command port

enum Reply {
    Text(String),
    Bytes(Vec<u8>),
}

async fn serve(listener: TcpListener, shared: Arc<Shared>) {
    loop {
        let Ok((sock, _)) = listener.accept().await else { continue };
        let shared = shared.clone();
        tokio::spawn(async move {
            let (r, mut w) = sock.into_split();
            let mut lines = BufReader::new(r).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let out = match handle(&line, &shared).await {
                    Ok(Reply::Text(s)) => format!("{s}\n").into_bytes(),
                    Ok(Reply::Bytes(b)) => b,
                    Err(e) => format!("err {e}\n").into_bytes(),
                };
                if w.write_all(&out).await.is_err() {
                    break;
                }
            }
        });
    }
}

fn button(name: &str) -> Result<Button> {
    Button::parse(name).ok_or_else(|| anyhow!("unknown button: {name}"))
}

async fn handle(line: &str, sh: &Arc<Shared>) -> Result<Reply> {
    let ok = || Ok(Reply::Text("ok".into()));
    let m: Value = serde_json::from_str(line).context("not JSON")?;
    let c = &sh.client();
    if let Some(name) = m["button"].as_str() {
        let b = button(name)?;
        match m["action"].as_str().unwrap_or("tap") {
            "press" => c.press(b),
            "release" => c.release(b),
            "tap" => c.tap(b, Duration::from_secs_f64(m["hold"].as_f64().unwrap_or(0.08))).await,
            other => bail!("unknown action: {other}"),
        }
    } else if let Some(which) = m["stick"].as_str() {
        let (x, y) = (m["x"].as_f64().unwrap_or(0.0) as f32, m["y"].as_f64().unwrap_or(0.0) as f32);
        c.stick(which == "right", x, y);
    } else if m.get("release_all").is_some() {
        sh.release_all();
    } else if m.get("frame").is_some() {
        let f = c.frame().ok_or_else(|| anyhow!("no frame yet"))?;
        let mut out = format!("ok {} {} {}\n", f.width, f.height, f.rgb.len()).into_bytes();
        out.extend_from_slice(&f.rgb);
        return Ok(Reply::Bytes(out));
    } else if let Some(region) = m.get("ocr") {
        // {"ocr": [x, y, w, h], "scale": 2.0} → ok ["line", ...]
        let f = c.frame().ok_or_else(|| anyhow!("no frame yet"))?;
        let r: Vec<u32> = region
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_f64()).map(|v| v.max(0.0) as u32).collect())
            .unwrap_or_default();
        let region = if m["rel"].as_bool() == Some(true) {
            // "rel": corners [x0, y0, x1, y1] as fractions of the frame, so the region does not depend on quality
            let v: Vec<f64> =
                region.as_array().map(|a| a.iter().filter_map(Value::as_f64).collect()).unwrap_or_default();
            let [x0, y0, x1, y1] = v.as_slice() else { bail!("ocr with rel needs [x0, y0, x1, y1] fractions") };
            let px = |v: f64, size: usize| (v.clamp(0.0, 1.0) * size as f64) as u32;
            let (x0, y0, x1, y1) = (px(*x0, f.width), px(*y0, f.height), px(*x1, f.width), px(*y1, f.height));
            [x0, y0, x1.saturating_sub(x0).max(1), y1.saturating_sub(y0).max(1)]
        } else {
            match r.as_slice() {
                [x, y, w, h] => [*x, *y, *w, *h],
                [] => [0, 0, f.width as u32, f.height as u32],
                _ => bail!("ocr region must be [x, y, w, h]"),
            }
        };
        let scale = m["scale"].as_f64().unwrap_or(1.0) as f32;
        let lines = tokio::task::spawn_blocking(move || {
            let (w, h, rgb) = ocr::crop_scale(&f, region, scale);
            ocr::read_text(&png_bytes(w, h, &rgb)?).map_err(|e| anyhow!(e))
        })
        .await??;
        return Ok(Reply::Text(format!("ok {}", serde_json::to_string(&lines)?)));
    } else if let Some(path) = m["snap"].as_str() {
        let f = c.frame().ok_or_else(|| anyhow!("no frame yet"))?;
        let path = path.to_string();
        tokio::task::spawn_blocking(move || save_png(&path, &f)).await??;
    } else if let Some(path) = m["play"].as_str() {
        if sh.playing.swap(true, Ordering::SeqCst) {
            bail!("already playing");
        }
        sh.stop_play.store(false, Ordering::SeqCst);
        let loaded = std::fs::read_to_string(path)
            .map_err(anyhow::Error::from)
            .and_then(|t| serde_json::from_str::<Vec<Value>>(&t).map_err(anyhow::Error::from));
        match loaded {
            Ok(events) => {
                tokio::spawn(play(events, sh.clone()));
            }
            Err(e) => {
                sh.playing.store(false, Ordering::SeqCst);
                bail!("cannot read the recording: {e}");
            }
        }
    } else if m.get("stop").is_some() {
        sh.stop_play.store(true, Ordering::SeqCst);
    } else if m.get("status").is_some() {
        return Ok(Reply::Text(if sh.playing.load(Ordering::SeqCst) { "ok playing" } else { "ok idle" }.into()));
    } else if let Some(text) = m.get("osd") {
        *sh.osd.lock().unwrap() = text.as_str().map(str::to_string).unwrap_or_else(|| text.to_string());
    } else if m.get("standby").is_some() {
        c.standby();
    } else if m.get("macros").is_some() {
        let r = sh.runner.lock().unwrap();
        if m["reload"].as_bool() == Some(true) {
            drop(r);
            sh.runner.lock().unwrap().reload();
            return Ok(Reply::Text(format!("ok {}", sh.runner.lock().unwrap().list_json())));
        }
        return Ok(Reply::Text(format!("ok {}", r.list_json())));
    } else if let Some(name) = m["macro_run"].as_str() {
        // optional "limit_min", "after": "none|standby", "at": "HH:MM|+MIN"
        let msg = sh.runner.lock().unwrap().run_named(name, &m, m["at"].as_str()).map_err(|e| anyhow!(e))?;
        return Ok(Reply::Text(format!("ok {msg}")));
    } else if let Some(q) = m["quality"].as_str() {
        // {"quality": "low|720p|1080p"} reconnects at that quality; {"quality": ""} just reports it
        if !q.is_empty() {
            let q = Quality::parse(q).ok_or_else(|| anyhow!("quality must be low, 720p or 1080p"))?;
            sh.set_quality(q).map_err(|e| anyhow!(e))?;
        }
        let f = c.frame();
        return Ok(Reply::Text(format!(
            "ok {}",
            serde_json::json!({
                "quality": sh.quality.lock().unwrap().name(),
                "reconnecting": sh.reconnecting.lock().unwrap().clone(),
                "width": f.as_ref().map(|f| f.width),
                "height": f.as_ref().map(|f| f.height),
            })
        )));
    } else if m.get("should_stop").is_some() {
        let yes = sh.runner.lock().unwrap().should_stop();
        return Ok(Reply::Text(if yes { "ok yes" } else { "ok no" }.into()));
    } else if m.get("macro_stop").is_some() {
        sh.runner.lock().unwrap().stop();
    } else if m.get("macro_status").is_some() {
        let lines = m["lines"].as_u64().unwrap_or(10) as usize;
        return Ok(Reply::Text(format!("ok {}", sh.runner.lock().unwrap().status_json(lines))));
    } else {
        bail!("unknown command");
    }
    ok()
}

/// Plays a recording: `[{"t":secs,"b":button,"a":"press|release"} or {"t":secs,"s":"left|right","x":..,"y":..}]`
async fn play(events: Vec<Value>, sh: Arc<Shared>) {
    let c = sh.client();
    let t0 = tokio::time::Instant::now();
    let mut held: HashSet<Button> = HashSet::new();
    for ev in events {
        if sh.stop_play.load(Ordering::SeqCst) {
            break;
        }
        let t = ev["t"].as_f64().unwrap_or(0.0);
        tokio::time::sleep_until(t0 + Duration::from_secs_f64(t.max(0.0))).await;
        if let Some(name) = ev["b"].as_str() {
            let Ok(b) = button(name) else { continue };
            if ev["a"].as_str() == Some("press") {
                c.press(b);
                held.insert(b);
            } else {
                c.release(b);
                held.remove(&b);
            }
        } else if let Some(which) = ev["s"].as_str() {
            c.stick(which == "right", ev["x"].as_f64().unwrap_or(0.0) as f32, ev["y"].as_f64().unwrap_or(0.0) as f32);
        }
    }
    for b in held {
        c.release(b);
    }
    c.stick(false, 0.0, 0.0);
    c.stick(true, 0.0, 0.0);
    sh.playing.store(false, Ordering::SeqCst);
}

fn png_bytes(w: usize, h: usize, rgb: &[u8]) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut buf, w as u32, h as u32);
        enc.set_color(png::ColorType::Rgb);
        enc.set_compression(png::Compression::Fast);
        enc.write_header()?.write_image_data(rgb)?;
    }
    Ok(buf)
}

fn save_png(path: &str, f: &Frame) -> Result<()> {
    let file = std::io::BufWriter::new(std::fs::File::create(path)?);
    let mut enc = png::Encoder::new(file, f.width as u32, f.height as u32);
    enc.set_color(png::ColorType::Rgb);
    enc.set_compression(png::Compression::Fast);
    enc.write_header()?.write_image_data(&f.rgb)?;
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn runner_with(cmd: &str, after: &str) -> Runner {
        let list = vec![MacroDef {
            name: "test macro".into(),
            cmd: cmd.into(),
            cwd: None,
            env: Vec::new(),
            plan: Plan::default().merged(&serde_json::json!({"limit_min": 5, "after": after})).unwrap(),
        }];
        Runner::new(Ok(list), Vec::new(), 0)
    }

    fn wait_end(r: &mut Runner) -> Ended {
        let t0 = Instant::now();
        loop {
            if let Some(e) = r.poll() {
                return e;
            }
            assert!(t0.elapsed() < Duration::from_secs(10), "macro did not end");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn macros_from_folders() {
        let root = std::env::temp_dir().join(format!("psconnector-macros-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for (dir, json) in [
            ("b-farm", r#"{"cmd": ".venv/bin/python -u farm.py", "limit_min": 30, "after": "standby"}"#),
            ("a-walk", r#"{"name": "Menu walk", "cmd": "python3 walk.py", "cwd": "sub"}"#),
            ("broken", r#"{"name": "no cmd"}"#),
        ] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
            std::fs::write(root.join(dir).join("macro.json"), json).unwrap();
        }
        std::fs::create_dir_all(root.join("not-a-macro")).unwrap();
        let list = load_macros(std::slice::from_ref(&root)).unwrap();
        let names: Vec<_> = list.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["Menu walk", "b-farm"], "sorted by folder, name defaults to the folder");
        assert_eq!(list[0].cwd.as_deref(), Some(root.join("a-walk").join("sub").as_path()));
        assert_eq!(list[1].cwd.as_deref(), Some(root.join("b-farm").as_path()));
        assert_eq!((list[1].plan.limit_min, list[1].plan.after), (30, After::Standby));
        assert!(load_macros(&[root.join("missing")]).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn plan_from_json() {
        let p = Plan::default().merged(&serde_json::json!({"limit_min": 90, "after": "standby"})).unwrap();
        assert_eq!((p.limit_min, p.after), (90, After::Standby));
        let kept = p.merged(&serde_json::json!({})).unwrap();
        assert_eq!((kept.limit_min, kept.after), (90, After::Standby));
        assert!(p.merged(&serde_json::json!({"after": "later"})).is_err());
        assert!(p.merged(&serde_json::json!({"limit_min": -1})).is_err());
    }

    #[test]
    fn start_time_parsing() {
        let (at, _) = parse_start_time("+30").unwrap();
        let wait = at.duration_since(SystemTime::now()).unwrap().as_secs();
        assert!((1795..=1800).contains(&wait));
        let (at, _) = parse_start_time("03:15").unwrap();
        assert!(at.duration_since(SystemTime::now()).unwrap() <= Duration::from_secs(24 * 3600));
        assert!(parse_start_time("3pm").is_err());
    }

    #[test]
    fn limit_stops_a_macro_that_never_asks_and_then_rests() {
        let mut r = runner_with("sleep 30", "standby");
        r.start();
        assert_eq!(r.plan.limit_min, 5);
        r.running.as_mut().unwrap().deadline = Some(Instant::now());
        let end = wait_end(&mut r);
        assert!(end.standby);
        assert!(r.last_exit.as_deref().unwrap().contains("time limit"));
    }

    #[test]
    fn cooperative_macro_gets_grace_and_is_told_to_stop() {
        let mut r = runner_with("sleep 30", "none");
        r.start();
        assert!(!r.should_stop());
        r.running.as_mut().unwrap().deadline = Some(Instant::now());
        assert!(r.poll().is_none());
        let run = r.running.as_ref().unwrap();
        assert!(run.wind_down.is_some() && run.stop_sent.is_none(), "should wait for the macro, not interrupt");
        assert!(r.should_stop());
        // grace over → interrupted
        r.running.as_mut().unwrap().wind_down = Some(Instant::now() - GRACE - Duration::from_secs(1));
        assert!(!wait_end(&mut r).standby);
    }

    #[test]
    fn manual_stop_skips_rest_mode_and_cancels_schedule() {
        let mut r = runner_with("sleep 30", "standby");
        r.start();
        r.stop();
        assert!(!wait_end(&mut r).standby);
        r.at_text = "+10".into();
        r.schedule_selected();
        assert!(r.scheduled.is_some());
        r.stop();
        assert!(r.scheduled.is_none());
    }

    #[test]
    fn scheduled_macro_starts_when_due() {
        let mut r = runner_with("true", "none");
        r.at_text = "+10".into();
        r.schedule_selected();
        assert!(r.poll().is_none() && r.running.is_none());
        r.scheduled.as_mut().unwrap().at = SystemTime::now();
        assert!(r.poll().is_none());
        assert!(r.running.is_some(), "should have started");
        assert!(!wait_end(&mut r).standby);
    }
}
