//! The macro runner: macro folders, run time limits, schedules and the after-action.
//! Recording the local pad as a macro is in [`record`], playing it back in [`replay`].

pub(crate) mod record;
pub(crate) mod replay;

use std::collections::VecDeque;
use std::io::BufRead;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use psconnector_protocol::RunOptions;
use serde_json::Value;

use crate::cli::{expand, home};

/// Lines of macro output kept for the bar and `macro_status`
const LOG_LINES: usize = 200;

/// After a time limit is reached, how long a macro that polls `should_stop` gets to finish on its own
/// before it is interrupted.
pub(crate) const GRACE: Duration = Duration::from_secs(120);

/// What to do when a macro ends by itself or by its time limit (not on a manual ■ Stop).
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub(crate) enum After {
    #[default]
    Nothing,
    Standby,
}

impl After {
    pub(crate) fn parse(s: &str) -> Option<Self> {
        match s {
            "none" | "nothing" | "stop" => Some(Self::Nothing),
            "standby" | "rest" => Some(Self::Standby),
            _ => None,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Nothing => "none",
            Self::Standby => "standby",
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Nothing => "then nothing",
            Self::Standby => "then rest mode",
        }
    }
}

/// How long to run and what to do afterwards.
#[derive(Clone, Copy, Default, Debug)]
pub(crate) struct Plan {
    /// 0 = no limit
    pub(crate) limit_min: u32,
    pub(crate) after: After,
    /// Recordings only: how many times one run plays it (0 = until stopped or the time limit)
    pub(crate) repeat: u32,
}

impl Plan {
    /// Reads `limit_min`, `after` and `repeat` from a JSON object, keeping `self` for missing fields.
    pub(crate) fn merged(self, v: &Value) -> Result<Self, String> {
        self.with(&RunOptions::from_json(v).map_err(|e| e.0)?)
    }

    /// `self` with the fields `o` gives replaced.
    pub(crate) fn with(self, o: &RunOptions) -> Result<Self, String> {
        let after = match o.after.as_deref() {
            None => self.after,
            Some(s) => After::parse(s).ok_or_else(|| format!("after must be none or standby, not '{s}'"))?,
        };
        Ok(Self {
            limit_min: o.limit_min.map_or(self.limit_min, |m| m.round() as u32),
            after,
            repeat: o.repeat.map_or(self.repeat, |r| r as u32),
        })
    }
}

#[derive(Clone)]
pub(crate) struct MacroDef {
    pub(crate) name: String,
    pub(crate) cmd: String,
    pub(crate) cwd: Option<PathBuf>,
    pub(crate) env: Vec<(String, String)>,
    /// A recording made with Record (`"recording"` instead of `"cmd"`), replayed by this program
    pub(crate) recording: Option<PathBuf>,
    /// Default limit and after-action from the list
    pub(crate) plan: Plan,
}

/// Where macros are found when no `--macros` is given: the `macros` folder next to the program, so the whole
/// folder can be moved (or unzipped anywhere on Windows) as one. A development build in `target/release/`
/// uses the repository's `macros` folder instead.
pub(crate) fn default_macro_sources() -> Vec<PathBuf> {
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
pub(crate) fn load_macros(sources: &[PathBuf]) -> Result<Vec<MacroDef>, String> {
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
pub(crate) fn macro_def(m: &Value, folder_name: Option<&str>, folder: Option<&PathBuf>) -> Result<MacroDef, String> {
    let recording = m["recording"].as_str().map(|r| {
        let p = expand(r);
        match folder {
            Some(f) if p.is_relative() => f.join(p),
            _ => p,
        }
    });
    let cmd = match (m["cmd"].as_str(), &recording) {
        (Some(c), _) => c.to_string(),
        (None, Some(r)) => format!("(recording {})", r.display()),
        (None, None) => return Err("\"cmd\" is missing".into()),
    };
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
        recording,
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
pub(crate) fn parse_start_time(text: &str) -> Result<(SystemTime, String), String> {
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

pub(crate) fn hms(d: Duration) -> String {
    let s = d.as_secs();
    format!("{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}

pub(crate) struct Scheduled {
    pub(crate) index: usize,
    pub(crate) plan: Plan,
    pub(crate) at: SystemTime,
    pub(crate) label: String,
}

pub(crate) struct Running {
    pub(crate) name: String,
    pub(crate) child: Child,
    pub(crate) started: Instant,
    pub(crate) plan: Plan,
    pub(crate) deadline: Option<Instant>,
    /// When the time limit was reached (`should_stop` answers yes from then on)
    pub(crate) wind_down: Option<Instant>,
    /// The macro has asked `should_stop` at least once, so it gets the grace period
    pub(crate) cooperative: bool,
    pub(crate) manual_stop: bool,
    pub(crate) stop_sent: Option<Instant>,
}

/// How a macro run ended, for the caller to act on.
pub(crate) struct Ended {
    pub(crate) standby: bool,
}

pub(crate) struct Runner {
    pub(crate) macros: Result<Vec<MacroDef>, String>,
    /// Macro folders and list files, re-read by Reload list
    pub(crate) sources: Vec<PathBuf>,
    pub(crate) port: u16,
    pub(crate) selected: usize,
    /// Limit and after-action set in the bar for the next run
    pub(crate) plan: Plan,
    /// Start-time text field in the bar
    pub(crate) at_text: String,
    pub(crate) scheduled: Option<Scheduled>,
    pub(crate) running: Option<Running>,
    pub(crate) log: Arc<Mutex<VecDeque<String>>>,
    pub(crate) last_exit: Option<String>,
}

impl Runner {
    pub(crate) fn new(macros: Result<Vec<MacroDef>, String>, sources: Vec<PathBuf>, port: u16) -> Self {
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
    pub(crate) fn select(&mut self, i: usize) {
        self.selected = i;
        if let Ok(list) = &self.macros
            && let Some(m) = list.get(i)
        {
            self.plan = m.plan;
        }
    }

    pub(crate) fn reload(&mut self) {
        if self.running.is_none() && self.scheduled.is_none() {
            self.macros = load_macros(&self.sources);
            self.select(0);
        }
    }

    /// Deletes the selected macro if it is a recording made with Record (other macros may be whole
    /// repositories linked in, so they are left to the person). Returns what happened, for the bar.
    pub(crate) fn delete_selected(&mut self) -> Result<String, String> {
        if self.running.is_some() || self.scheduled.is_some() {
            return Err("stop or cancel the macro first".into());
        }
        let m = self.macros.as_ref().ok().and_then(|l| l.get(self.selected)).cloned().ok_or("no macro selected")?;
        let rec = m.recording.as_ref().ok_or("only recordings made with Record can be deleted here")?;
        let dir = rec.parent().ok_or("recording has no folder")?;
        let msg = remove_recording(dir, &self.sources, trash_dir().as_deref())?;
        let index = self.selected;
        self.reload();
        let count = self.macros.as_ref().map(Vec::len).unwrap_or(0);
        self.select(index.min(count.saturating_sub(1)));
        log::info!("{}: {msg}", m.name);
        Ok(format!("{}: {msg}", m.name))
    }

    pub(crate) fn push_log(log: &Arc<Mutex<VecDeque<String>>>, line: String) {
        let mut l = log.lock().unwrap();
        l.push_back(line);
        while l.len() > LOG_LINES {
            l.pop_front();
        }
    }

    pub(crate) fn start(&mut self) {
        let (i, plan) = (self.selected, self.plan);
        self.start_with(i, plan);
    }

    pub(crate) fn start_with(&mut self, index: usize, plan: Plan) {
        let Ok(list) = &self.macros else { return };
        let Some(m) = list.get(index).cloned() else { return };
        let mut cmd = match &m.recording {
            // a recording is replayed by this same program, as its own process like any macro
            Some(rec) => {
                let mut c = Command::new(std::env::current_exe().unwrap_or_else(|_| "psconnector-view".into()));
                c.arg("--replay").arg(rec).arg("--repeat").arg(plan.repeat.to_string());
                c
            }
            None => shell_command(&m.cmd),
        };
        cmd.env(psconnector_protocol::PORT_ENV, self.port.to_string())
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
                let laps = match (&m.recording, plan.repeat) {
                    (None, _) => String::new(),
                    (Some(_), 0) => ", until stopped".into(),
                    (Some(_), n) => format!(", {n}×"),
                };
                let note = match limit {
                    Some(_) => format!(" for {} min, {}", plan.limit_min, plan.after.label()),
                    None if plan.after == After::Standby => format!(", {}", plan.after.label()),
                    None => String::new(),
                };
                Self::push_log(&self.log, format!("▶ {} started{laps}{note}", m.name));
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
    pub(crate) fn find(&self, name: &str) -> Result<usize, String> {
        let list = self.macros.as_ref().map_err(|e| e.clone())?;
        list.iter()
            .position(|m| m.name == name)
            .or_else(|| list.iter().position(|m| m.name.contains(name)))
            .ok_or_else(|| format!("no macro named '{name}'"))
    }

    /// Runs a macro by name now, or schedules it when `at` is given. `opts` may override
    /// `limit_min` and `after` from the list.
    pub(crate) fn run_named(&mut self, name: &str, opts: &RunOptions, at: Option<&str>) -> Result<String, String> {
        if self.running.is_some() {
            return Err("another macro is already running".into());
        }
        let i = self.find(name)?;
        self.select(i);
        let plan = self.plan.with(opts)?;
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

    pub(crate) fn schedule(&mut self, index: usize, plan: Plan, at: &str) -> Result<String, String> {
        let (when, label) = parse_start_time(at)?;
        let name = self.macros.as_ref().ok().and_then(|l| l.get(index)).map(|m| m.name.clone()).unwrap_or_default();
        Self::push_log(&self.log, format!("⏰ {name} scheduled for {label}"));
        self.scheduled = Some(Scheduled { index, plan, at: when, label: label.clone() });
        Ok(format!("scheduled for {label}"))
    }

    /// Schedules the macro selected in the bar at the time typed in the bar.
    pub(crate) fn schedule_selected(&mut self) {
        let (i, plan, at) = (self.selected, self.plan, self.at_text.clone());
        if let Err(e) = self.schedule(i, plan, &at) {
            Self::push_log(&self.log, format!("schedule: {e}"));
        }
    }

    pub(crate) fn cancel_schedule(&mut self) {
        if self.scheduled.take().is_some() {
            Self::push_log(&self.log, "⏰ schedule cancelled".into());
        }
    }

    /// Asked by the macro through the command port: true once its time limit is up.
    pub(crate) fn should_stop(&mut self) -> bool {
        match self.running.as_mut() {
            Some(r) => {
                r.cooperative = true;
                r.wind_down.is_some() || r.stop_sent.is_some()
            }
            None => false,
        }
    }

    /// Status as JSON (for the command port and MCP)
    pub(crate) fn status_json(&self, lines: usize) -> Value {
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

    pub(crate) fn list_json(&self) -> Value {
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
    pub(crate) fn stop(&mut self) {
        self.cancel_schedule();
        if let Some(r) = self.running.as_mut() {
            r.manual_stop = true;
        }
        self.interrupt_running();
    }

    pub(crate) fn interrupt_running(&mut self) {
        if let Some(r) = self.running.as_mut().filter(|r| r.stop_sent.is_none()) {
            interrupt(&mut r.child);
            r.stop_sent = Some(Instant::now());
            Self::push_log(&self.log, "■ stop signal sent".into());
        }
    }

    /// Called every frame: starts a scheduled macro when its time comes, enforces the time limit,
    /// kills the process 3 s after a stop signal and reports when it ended.
    pub(crate) fn poll(&mut self) -> Option<Ended> {
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

    pub(crate) fn shutdown(&mut self) {
        self.stop();
        let t0 = Instant::now();
        while self.running.is_some() && t0.elapsed() < Duration::from_secs(4) {
            self.poll();
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// The system Trash, where deleted recordings go so they can be restored (macOS; elsewhere they are removed).
fn trash_dir() -> Option<PathBuf> {
    if cfg!(target_os = "macos") { Some(home().join(".Trash")).filter(|p| p.is_dir()) } else { None }
}

/// Moves a recording's folder to `trash` (or deletes it without one). Refuses anything that is not a real
/// folder directly inside one of the macro folders, so a linked repository is never touched.
fn remove_recording(
    dir: &std::path::Path,
    sources: &[PathBuf],
    trash: Option<&std::path::Path>,
) -> Result<String, String> {
    let meta = std::fs::symlink_metadata(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    if !meta.is_dir() {
        return Err(format!("{} is a link, not a recording folder; remove it yourself", dir.display()));
    }
    let parent = dir.parent().and_then(|p| p.canonicalize().ok());
    let inside = sources.iter().filter_map(|s| s.canonicalize().ok()).any(|s| Some(&s) == parent.as_ref());
    if !inside {
        return Err(format!("{} is not in a macro folder; remove it yourself", dir.display()));
    }
    match trash {
        Some(trash) => {
            let name = dir.file_name().ok_or("bad folder name")?;
            let mut to = trash.join(name);
            let mut n = 2;
            while to.exists() {
                to = trash.join(format!("{} {n}", name.to_string_lossy()));
                n += 1;
            }
            std::fs::rename(dir, &to).map_err(|e| format!("cannot move to the Trash: {e}"))?;
            Ok("moved to the Trash".into())
        }
        None => {
            std::fs::remove_dir_all(dir).map_err(|e| format!("cannot delete: {e}"))?;
            Ok("deleted".into())
        }
    }
}

/// Runs the macro command line through the system shell.
#[cfg(unix)]
pub(crate) fn shell_command(line: &str) -> Command {
    // `exec` replaces the shell with the macro so the stop signal reaches the macro, not the shell
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c").arg(format!("exec {line}"));
    cmd
}

#[cfg(windows)]
pub(crate) fn shell_command(line: &str) -> Command {
    use std::os::windows::process::CommandExt;
    let mut cmd = Command::new("cmd");
    cmd.arg("/C").raw_arg(line);
    cmd
}

/// Asks the macro to clean up and exit: SIGINT on Unix (KeyboardInterrupt in Python),
/// an immediate kill elsewhere.
#[cfg(unix)]
pub(crate) fn interrupt(child: &mut Child) {
    // SAFETY: only sends a signal to the pid of a child process we spawned
    unsafe { libc::kill(child.id() as i32, libc::SIGINT) };
}

#[cfg(not(unix))]
pub(crate) fn interrupt(child: &mut Child) {
    let _ = child.kill();
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn runner_with(cmd: &str, after: &str) -> Runner {
        let list = vec![MacroDef {
            name: "test macro".into(),
            cmd: cmd.into(),
            recording: None,
            cwd: None,
            env: Vec::new(),
            plan: Plan::default().merged(&serde_json::json!({"limit_min": 5, "after": after})).unwrap(),
        }];
        Runner::new(Ok(list), Vec::new(), 0)
    }

    #[test]
    fn only_recording_folders_inside_the_macro_folder_are_removed() {
        let base = std::env::temp_dir().join(format!("psconnector-del-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (macros, trash, repo) = (base.join("macros"), base.join("trash"), base.join("repo"));
        for d in [macros.join("Run"), trash.clone(), repo.clone()] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::os::unix::fs::symlink(&repo, macros.join("linked")).unwrap();
        let sources = vec![macros.clone()];
        let err = remove_recording(&macros.join("linked"), &sources, Some(&trash)).unwrap_err();
        assert!(err.contains("link"), "{err}");
        assert!(repo.is_dir(), "a linked repository is never touched");
        assert!(remove_recording(&repo, &sources, Some(&trash)).unwrap_err().contains("not in a macro folder"));
        assert_eq!(remove_recording(&macros.join("Run"), &sources, Some(&trash)).unwrap(), "moved to the Trash");
        assert!(!macros.join("Run").exists() && trash.join("Run").is_dir());
        std::fs::create_dir_all(macros.join("Run")).unwrap();
        remove_recording(&macros.join("Run"), &sources, Some(&trash)).unwrap();
        assert!(trash.join("Run 2").is_dir(), "a second one with the same name is kept apart");
        let _ = std::fs::remove_dir_all(&base);
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
