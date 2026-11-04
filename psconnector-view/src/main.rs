//! psconnector-view: PS5 screen window + control command port + macro runner.
//!
//! ```text
//! cargo run --release -p psconnector-view -- [host] [--port 8650] [--macros FILE] [--run MACRO]
//! ```
//! - Command port (127.0.0.1:8650, one JSON object per line): the same commands as the Python `ps5view.py`
//!   plus `frame` and `release_all`, so existing macros run unchanged. From Python, use `python/psconnector.py`.
//! - Top bar: pick a macro from `~/.psconnector/macros.json` and ▶ Run / ■ Stop.
//!   Macros run as child processes and get the port via the `PSCONNECTOR_PORT` environment variable.
//!   Stop sends SIGINT first (KeyboardInterrupt in Python) and kills the process if it is not gone in 3 s.
//!   However a macro ends, all buttons are released and the sticks are centered.
//!
//! Commands: `{"button":"cross","action":"tap|press|release","hold":0.08}` `{"stick":"left","x":0,"y":-1}`
//! `{"release_all":true}` `{"snap":"path.png"}` `{"frame":true}` `{"play":"recording.json"}` `{"stop":true}`
//! `{"status":true}` `{"osd":"text"}` `{"standby":true}` `{"ocr":[x,y,w,h],"scale":2}` (reply `ok ["line", ...]`)
//! Macros: `{"macros":true}` `{"macro_run":"name"}` `{"macro_stop":true}` `{"macro_status":true,"lines":10}` (reply `ok {JSON}`)
//! Replies are one line: `ok` / `ok playing` / `ok idle` / `err REASON`.
//! Only `frame` is followed by raw RGB bytes after its `ok WIDTH HEIGHT BYTES` line. Press Q in the window to quit.

use std::collections::{HashSet, VecDeque};
use std::io::BufRead;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use eframe::egui;
use psconnector::{Button, Client, Frame, Options};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

mod ocr;

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
    client: Arc<Client>,
    runner: Arc<Mutex<Runner>>,
    osd: Mutex<String>,
    playing: AtomicBool,
    stop_play: AtomicBool,
    /// On Ctrl+C or SIGTERM, close the window and end the session cleanly
    quit: AtomicBool,
}

impl Shared {
    fn release_all(&self) {
        for b in Button::ALL {
            self.client.release(b);
        }
        self.client.stick(false, 0.0, 0.0);
        self.client.stick(true, 0.0, 0.0);
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

fn default_host() -> Option<String> {
    // default console address saved by ps5rmtctl
    let text = std::fs::read_to_string(home().join(".ps5rmtctl").join("config.json")).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    v["host"].as_str().map(str::to_string)
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let mut args = std::env::args().skip(1);
    let mut host = None;
    let mut port = DEFAULT_PORT;
    let mut macros_path = expand("~/.psconnector/macros.json");
    let mut autorun: Option<String> = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--port" => {
                port = args.next().and_then(|p| p.parse().ok()).ok_or_else(|| anyhow!("missing value for --port"))?
            }
            "--macros" => macros_path = expand(&args.next().ok_or_else(|| anyhow!("missing value for --macros"))?),
            "--run" => autorun = Some(args.next().ok_or_else(|| anyhow!("missing value for --run"))?),
            _ => host = Some(a),
        }
    }
    let host = host
        .or_else(default_host)
        .ok_or_else(|| anyhow!("usage: psconnector-view <PS5 address> [--port 8650] [--macros FILE] [--run MACRO]"))?;

    let rt = tokio::runtime::Runtime::new()?;
    println!("connecting to the PS5...");
    let client = Arc::new(rt.block_on(connect(&host))?);
    let runner = Arc::new(Mutex::new(Runner::new(load_macros(&macros_path), macros_path.clone(), port)));
    let shared = Arc::new(Shared {
        client: client.clone(),
        runner: runner.clone(),
        osd: Mutex::new(String::new()),
        playing: AtomicBool::new(false),
        stop_play: AtomicBool::new(false),
        quit: AtomicBool::new(false),
    });
    let sig = shared.clone();
    rt.spawn(async move {
        wait_for_quit_signal().await;
        sig.quit.store(true, Ordering::SeqCst);
    });
    let listener = rt.block_on(TcpListener::bind(("127.0.0.1", port))).context("cannot open the command port")?;
    rt.spawn(serve(listener, shared.clone()));
    println!("connected. command port {port}. press Q in the window to quit");

    if let Some(name) = autorun {
        runner.lock().unwrap().start_named(&name).map_err(|e| anyhow!("--run: {e}"))?;
    }
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_title("PS5").with_inner_size([1280.0, 760.0]),
        ..Default::default()
    };
    let frames = client.frames();
    let app_shared = shared.clone();
    let result = eframe::run_native(
        "psconnector-view",
        options,
        Box::new(move |cc| {
            setup_fonts(&cc.egui_ctx);
            Ok(Box::new(Viewer { shared: app_shared, frames, texture: None, shown: None, runner }))
        }),
    );
    shared.stop_play.store(true, Ordering::SeqCst);
    shared.release_all();
    rt.block_on(client.close());
    println!("closed");
    result.map_err(|e| anyhow!("window error: {e}"))
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

/// The console reports "another session connected" for a few seconds while it cleans up the previous
/// session, so retry a few times.
async fn connect(host: &str) -> Result<Client> {
    let mut k = 0;
    loop {
        match Client::connect(host, Options::default()).await {
            Err(psconnector::Error::Busy) if k < 7 => {
                k += 1;
                println!("previous session still active, retrying in 5 s");
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
            r => return Ok(r?),
        }
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

#[derive(Clone)]
struct MacroDef {
    name: String,
    cmd: String,
    cwd: Option<PathBuf>,
    env: Vec<(String, String)>,
}

/// `[{"name": "...", "cmd": "python3 -u farm.py 0", "cwd": "~/...", "env": {"K": "V"}}]`
fn load_macros(path: &PathBuf) -> Result<Vec<MacroDef>, String> {
    let text = std::fs::read_to_string(path).map_err(|_| format!("no macro list: {}", path.display()))?;
    let v: Value = serde_json::from_str(&text).map_err(|e| format!("macro list JSON error: {e}"))?;
    let list = v.as_array().ok_or("the macro list must be a JSON array")?;
    Ok(list
        .iter()
        .filter_map(|m| {
            Some(MacroDef {
                name: m["name"].as_str()?.to_string(),
                cmd: m["cmd"].as_str()?.to_string(),
                cwd: m["cwd"].as_str().map(expand),
                env: m["env"]
                    .as_object()
                    .map(|o| {
                        o.iter()
                            .map(|(k, v)| (k.clone(), v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string())))
                            .collect()
                    })
                    .unwrap_or_default(),
            })
        })
        .collect())
}

struct Running {
    name: String,
    child: Child,
    started: Instant,
    stop_sent: Option<Instant>,
}

struct Runner {
    macros: Result<Vec<MacroDef>, String>,
    path: PathBuf,
    port: u16,
    selected: usize,
    running: Option<Running>,
    log: Arc<Mutex<VecDeque<String>>>,
    last_exit: Option<String>,
}

impl Runner {
    fn new(macros: Result<Vec<MacroDef>, String>, path: PathBuf, port: u16) -> Self {
        Self {
            macros,
            path,
            port,
            selected: 0,
            running: None,
            log: Arc::new(Mutex::new(VecDeque::new())),
            last_exit: None,
        }
    }

    fn reload(&mut self) {
        if self.running.is_none() {
            self.macros = load_macros(&self.path);
            self.selected = 0;
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
        let Ok(list) = &self.macros else { return };
        let Some(m) = list.get(self.selected).cloned() else { return };
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
                Self::push_log(&self.log, format!("▶ {} started", m.name));
                self.last_exit = None;
                self.running = Some(Running { name: m.name, child, started: Instant::now(), stop_sent: None });
            }
            Err(e) => Self::push_log(&self.log, format!("failed to start: {e}")),
        }
    }

    /// Runs the first macro whose name equals or contains `name`
    fn start_named(&mut self, name: &str) -> Result<(), String> {
        if self.running.is_some() {
            return Err("another macro is already running".into());
        }
        let list = self.macros.as_ref().map_err(|e| e.clone())?;
        let i = list
            .iter()
            .position(|m| m.name == name)
            .or_else(|| list.iter().position(|m| m.name.contains(name)))
            .ok_or_else(|| format!("no macro named '{name}'"))?;
        self.selected = i;
        self.start();
        if self.running.is_none() {
            return Err(self.log.lock().unwrap().back().cloned().unwrap_or_else(|| "failed to start".into()));
        }
        Ok(())
    }

    /// Status as JSON (for the command port and MCP)
    fn status_json(&self, lines: usize) -> Value {
        let log: Vec<String> = {
            let l = self.log.lock().unwrap();
            l.iter().skip(l.len().saturating_sub(lines)).cloned().collect()
        };
        serde_json::json!({
            "running": self.running.as_ref().map(|r| r.name.clone()),
            "elapsed_sec": self.running.as_ref().map(|r| r.started.elapsed().as_secs()),
            "stopping": self.running.as_ref().is_some_and(|r| r.stop_sent.is_some()),
            "last_exit": self.last_exit,
            "log": log,
        })
    }

    fn list_json(&self) -> Value {
        match &self.macros {
            Ok(list) => serde_json::json!(
                list.iter().map(|m| serde_json::json!({"name": m.name, "cmd": m.cmd})).collect::<Vec<_>>()
            ),
            Err(e) => serde_json::json!({"error": e}),
        }
    }

    fn stop(&mut self) {
        if let Some(r) = self.running.as_mut().filter(|r| r.stop_sent.is_none()) {
            interrupt(&mut r.child);
            r.stop_sent = Some(Instant::now());
            Self::push_log(&self.log, "■ stop signal sent".into());
        }
    }

    /// Called every frame: checks for exit and kills the process 3 s after a stop signal. True when it ended.
    fn poll(&mut self) -> bool {
        let Some(r) = self.running.as_mut() else { return false };
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
                Self::push_log(&self.log, format!("{} {how}", r.name));
                self.last_exit = Some(how);
                self.running = None;
                true
            }
            _ => false,
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
    frames: tokio::sync::watch::Receiver<Option<Arc<Frame>>>,
    texture: Option<egui::TextureHandle>,
    shown: Option<Arc<Frame>>,
    runner: Arc<Mutex<Runner>>,
}

impl Viewer {
    fn bar(&mut self, ui: &mut egui::Ui) {
        let runner = self.runner.clone();
        let mut r = runner.lock().unwrap();
        let r = &mut *r;
        ui.horizontal(|ui| {
            let names: Vec<String> = match &r.macros {
                Ok(list) => list.iter().map(|m| m.name.clone()).collect(),
                Err(_) => Vec::new(),
            };
            let idle = r.running.is_none();
            ui.add_enabled_ui(idle && !names.is_empty(), |ui| {
                egui::ComboBox::from_id_salt("macro")
                    .selected_text(names.get(r.selected).cloned().unwrap_or_else(|| "no macros".into()))
                    .width(200.0)
                    .show_ui(ui, |ui| {
                        for (i, n) in names.iter().enumerate() {
                            ui.selectable_value(&mut r.selected, i, n);
                        }
                    });
            });
            if idle {
                if ui.add_enabled(!names.is_empty(), egui::Button::new("▶ Run")).clicked() {
                    r.start();
                }
                if ui.button("Reload list").clicked() {
                    r.reload();
                }
            } else if ui.button("■ Stop").clicked() {
                r.stop();
            }
            let status = match (&r.running, &r.macros) {
                (Some(run), _) => {
                    let s = run.started.elapsed().as_secs();
                    let stopping = if run.stop_sent.is_some() { " (stopping)" } else { "" };
                    format!("{} running {:02}:{:02}:{:02}{stopping}", run.name, s / 3600, s / 60 % 60, s % 60)
                }
                (None, Err(e)) => e.clone(),
                (None, Ok(_)) => r.last_exit.clone().map(|e| format!("stopped ({e})")).unwrap_or_else(|| "idle".into()),
            };
            ui.label(status);
            let last = r.log.lock().unwrap().back().cloned().unwrap_or_default();
            ui.add(egui::Label::new(egui::RichText::new(last).weak()).truncate());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Rest mode").on_hover_text("Put the PS5 into rest mode (this also disconnects)").clicked()
                {
                    r.stop();
                    self.shared.client.standby();
                }
            });
        });
    }
}

impl eframe::App for Viewer {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let quit = self.shared.quit.load(Ordering::SeqCst);
        if quit || ctx.input(|i| i.key_pressed(egui::Key::Q) && !i.modifiers.any()) {
            self.runner.lock().unwrap().shutdown();
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        let ended = self.runner.lock().unwrap().poll();
        if ended {
            // however the macro ended, release any input left pressed
            self.shared.release_all();
            self.shared.stop_play.store(true, Ordering::SeqCst);
        }

        egui::Panel::top("bar").show(ui, |ui| self.bar(ui));

        let latest = self.frames.borrow().clone();
        if let Some(f) = latest
            && !self.shown.as_ref().is_some_and(|s| Arc::ptr_eq(s, &f))
        {
            let img = egui::ColorImage::from_rgb([f.width, f.height], &f.rgb);
            match &mut self.texture {
                Some(t) => t.set(img, egui::TextureOptions::LINEAR),
                None => self.texture = Some(ctx.load_texture("ps5", img, egui::TextureOptions::LINEAR)),
            }
            self.shown = Some(f);
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
        if !self.shared.client.is_ready() {
            text = format!("disconnected: {:?}\n{text}", self.shared.client.phase());
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
    let c = &sh.client;
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
        let region = match r.as_slice() {
            [x, y, w, h] => [*x, *y, *w, *h],
            [] => [0, 0, f.width as u32, f.height as u32],
            _ => bail!("ocr region must be [x, y, w, h]"),
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
        sh.runner.lock().unwrap().start_named(name).map_err(|e| anyhow!(e))?;
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
    let c = &sh.client;
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
