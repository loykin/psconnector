//! Plays a recording made with `record.rs`: `psconnector-view --replay recording.json [--repeat N]`
//! (N = 0 plays it until stopped).
//!
//! It runs as an ordinary macro process (the bar starts it for macro folders with `"recording"`) and drives
//! the window through the command port, so time limits, rest mode, schedules and ■ Stop work as for any
//! macro. Inputs follow at their recorded times counted from the
//! last checkpoint reached, so a slow load only delays what comes after it. A checkpoint that is not reached
//! within its timeout ends the replay (the game is somewhere the recording does not expect).

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use crate::record;

/// How often the screen is compared while waiting for a checkpoint.
const POLL: Duration = Duration::from_millis(100);
/// A checkpoint counts as reached after this many matching looks in a row (a single similar frame
/// during a fade is not enough).
const STABLE: u32 = 2;

static STOP: AtomicBool = AtomicBool::new(false);

#[cfg(unix)]
fn catch_interrupt() {
    extern "C" fn on_signal(_: libc::c_int) {
        STOP.store(true, Ordering::SeqCst);
    }
    // SAFETY: the handler only stores to an atomic, which is async-signal-safe
    unsafe {
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
    }
}

#[cfg(not(unix))]
fn catch_interrupt() {
    // Stop kills the process here; the window releases all input once a macro has ended.
}

struct Port {
    w: TcpStream,
    r: BufReader<TcpStream>,
}

impl Port {
    fn connect() -> Result<Self> {
        Self::connect_to(std::env::var("PSCONNECTOR_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8650))
    }

    fn connect_to(port: u16) -> Result<Self> {
        let w =
            TcpStream::connect(("127.0.0.1", port)).with_context(|| format!("no psconnector-view on port {port}"))?;
        w.set_nodelay(true)?;
        Ok(Self { r: BufReader::new(w.try_clone()?), w })
    }

    fn call(&mut self, msg: &Value) -> Result<String> {
        self.w.write_all(format!("{msg}\n").as_bytes())?;
        let mut line = String::new();
        self.r.read_line(&mut line)?;
        let line = line.trim().to_string();
        if !line.starts_with("ok") {
            bail!("{}", if line.is_empty() { "psconnector-view closed the port".into() } else { line });
        }
        Ok(line)
    }

    /// The current screen at checkpoint size.
    fn thumb(&mut self, region: [f64; 4]) -> Result<Vec<u8>> {
        let head = self.call(&json!({"thumb": region}))?;
        let n: usize =
            head.split_whitespace().nth(3).and_then(|v| v.parse().ok()).ok_or_else(|| anyhow!("bad reply: {head}"))?;
        let mut buf = vec![0; n];
        self.r.read_exact(&mut buf)?;
        Ok(buf)
    }
}

struct Checkpoint {
    t: f64,
    thumb: Vec<u8>,
    region: [f64; 4],
    tol: f64,
    changed: f64,
    timeout: f64,
}

/// Sleeps until `until`, waking early on Stop.
fn sleep_until(until: Instant) -> bool {
    loop {
        if STOP.load(Ordering::SeqCst) {
            return false;
        }
        let now = Instant::now();
        if now >= until {
            return true;
        }
        std::thread::sleep((until - now).min(Duration::from_millis(20)));
    }
}

/// Waits until the screen matches the checkpoint. `Ok(false)` on Stop.
fn reach(port: &mut Port, n: usize, cp: &Checkpoint) -> Result<bool> {
    let started = Instant::now();
    let (mut best, mut stable, mut last_note) = (None, 0, Instant::now());
    loop {
        if STOP.load(Ordering::SeqCst) {
            return Ok(false);
        }
        let d = record::difference(&port.thumb(cp.region)?, &cp.thumb);
        if best.is_none_or(|b: record::Difference| d.mean + d.changed < b.mean + b.changed) {
            best = Some(d);
        }
        stable = if d.within(cp.tol, cp.changed) { stable + 1 } else { 0 };
        if stable >= STABLE {
            if started.elapsed() > Duration::from_secs(1) {
                println!("checkpoint {n} reached after {:.1} s ({d})", started.elapsed().as_secs_f64());
            }
            return Ok(true);
        }
        if started.elapsed().as_secs_f64() > cp.timeout {
            bail!(
                "checkpoint {n} not reached in {:.0} s (closest: {}; needs mean {:.1} and {:.1}% changed or less): \
                 the screen is not where the recording expects, stopping",
                cp.timeout,
                best.map(|b| b.to_string()).unwrap_or_else(|| "nothing seen".into()),
                cp.tol,
                cp.changed
            );
        }
        if last_note.elapsed() > Duration::from_secs(5) {
            println!("waiting for checkpoint {n} ({d}; needs mean {:.1}, {:.1}% changed)", cp.tol, cp.changed);
            last_note = Instant::now();
        }
        std::thread::sleep(POLL);
    }
}

fn load(path: &Path) -> Result<(Vec<Value>, Vec<Checkpoint>, u32)> {
    let text = std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    let rec: Value = serde_json::from_str(&text).with_context(|| format!("{}: not valid JSON", path.display()))?;
    let dir = path.parent().unwrap_or(Path::new("."));
    let mut checkpoints = Vec::new();
    for (i, c) in rec["checkpoints"].as_array().into_iter().flatten().enumerate() {
        let image = c["image"].as_str().ok_or_else(|| anyhow!("checkpoint {} has no image", i + 1))?;
        let (w, h, thumb) = record::read_png(&dir.join(image))?;
        if (w, h) != record::THUMB {
            bail!("{image} must be {}×{}", record::THUMB.0, record::THUMB.1);
        }
        let region = match c["region"].as_array().map(|a| a.iter().filter_map(Value::as_f64).collect::<Vec<_>>()) {
            Some(v) if v.len() == 4 => [v[0], v[1], v[2], v[3]],
            _ => record::FULL,
        };
        checkpoints.push(Checkpoint {
            t: c["t"].as_f64().unwrap_or(0.0),
            thumb,
            region,
            tol: c["tol"].as_f64().unwrap_or(record::DEFAULT_TOL),
            changed: c["changed"].as_f64().unwrap_or(record::DEFAULT_CHANGED),
            timeout: c["timeout"].as_f64().unwrap_or(record::DEFAULT_TIMEOUT),
        });
    }
    let mut events = rec["events"].as_array().cloned().unwrap_or_default();
    events.sort_by(|a, b| a["t"].as_f64().unwrap_or(0.0).total_cmp(&b["t"].as_f64().unwrap_or(0.0)));
    let repeat = rec["repeat"].as_u64().unwrap_or(0) as u32;
    Ok((events, checkpoints, repeat))
}

/// One pass through the recording. `Ok(false)` on Stop.
fn lap(port: &mut Port, events: &[Value], checkpoints: &[Checkpoint]) -> Result<bool> {
    // times count from the last checkpoint reached (the start of the lap until the first one)
    let (mut from_rec, mut from_real) = (0.0, Instant::now());
    for ev in events {
        if let Some(i) = ev["cp"].as_u64() {
            let cp = checkpoints.get(i as usize).ok_or_else(|| anyhow!("event refers to missing checkpoint {i}"))?;
            if !reach(port, i as usize + 1, cp)? {
                return Ok(false);
            }
            (from_rec, from_real) = (cp.t, Instant::now());
            continue;
        }
        let t = ev["t"].as_f64().unwrap_or(0.0);
        if !sleep_until(from_real + Duration::from_secs_f64((t - from_rec).max(0.0))) {
            return Ok(false);
        }
        if let Some(b) = ev["b"].as_str() {
            port.call(&json!({"button": b, "action": ev["a"].as_str().unwrap_or("release")}))?;
        } else if let Some(s) = ev["s"].as_str() {
            port.call(&json!({"stick": s, "x": ev["x"], "y": ev["y"]}))?;
        }
    }
    Ok(true)
}

/// Entry point for `--replay`; the process exit code is 0 when it finished or was stopped.
pub(crate) fn run(path: &Path, repeat: Option<u32>) -> ! {
    catch_interrupt();
    let result = (|| -> Result<()> {
        let (events, checkpoints, saved) = load(path)?;
        // the bar passes the run's own repeat count; the file's is for running it by hand
        let repeat = repeat.unwrap_or(saved);
        let mut port = Port::connect()?;
        let laps = if repeat == 0 { "until stopped".to_string() } else { format!("{repeat}×") };
        println!(
            "replaying {} inputs, {} checkpoints, {laps}",
            events.iter().filter(|e| e.get("cp").is_none()).count(),
            checkpoints.len()
        );
        let mut n = 0u32;
        while repeat == 0 || n < repeat {
            let started = Instant::now();
            let done = lap(&mut port, &events, &checkpoints);
            port.call(&json!({"release_all": true}))?;
            if !done? {
                println!("stopped");
                return Ok(());
            }
            n += 1;
            println!("lap {n} done in {:.1} s", started.elapsed().as_secs_f64());
            if port.call(&json!({"should_stop": true}))? == "ok yes" {
                println!("time limit reached");
                break;
            }
        }
        Ok(())
    })();
    match result {
        Ok(()) => std::process::exit(0),
        Err(e) => {
            println!("{e:#}");
            std::process::exit(2);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// A stand-in for psconnector-view's command port: logs commands with their time, and shows `screen`
    /// (a checkpoint-sized picture) to `thumb`.
    type Sent = Arc<Mutex<Vec<(f64, Value)>>>;

    fn fake_port(screen: Arc<Mutex<Vec<u8>>>) -> (u16, Sent) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let log = Arc::new(Mutex::new(Vec::new()));
        let seen = log.clone();
        std::thread::spawn(move || {
            let (sock, _) = listener.accept().unwrap();
            let t0 = Instant::now();
            let mut w = sock.try_clone().unwrap();
            for line in BufReader::new(sock).lines().map_while(|l| l.ok()) {
                let m: Value = serde_json::from_str(&line).unwrap();
                if m.get("thumb").is_some() {
                    let px = screen.lock().unwrap().clone();
                    let (tw, th) = record::THUMB;
                    w.write_all(format!("ok {tw} {th} {}\n", px.len()).as_bytes()).unwrap();
                    w.write_all(&px).unwrap();
                } else {
                    seen.lock().unwrap().push((t0.elapsed().as_secs_f64(), m));
                    w.write_all(b"ok\n").unwrap();
                }
            }
        });
        (port, log)
    }

    #[test]
    fn inputs_wait_for_their_checkpoint_and_keep_their_timing_after_it() {
        let size = record::THUMB.0 * record::THUMB.1 * 3;
        let (menu, loading) = (vec![200u8; size], vec![0u8; size]);
        let screen = Arc::new(Mutex::new(loading.clone()));
        let (port, log) = fake_port(screen.clone());
        let checkpoints = vec![Checkpoint {
            t: 1.0,
            thumb: menu.clone(),
            region: record::FULL,
            tol: 8.0,
            changed: 3.0,
            timeout: 5.0,
        }];
        let events = vec![
            json!({"t": 0.1, "b": "cross", "a": "press"}),
            json!({"t": 0.15, "b": "cross", "a": "release"}),
            json!({"t": 1.0, "cp": 0}),
            json!({"t": 1.2, "s": "left", "x": 0.0, "y": -1.0}),
        ];
        // the "load" ends 0.6 s in, much later than the 0.15 s the recording needed
        let later = screen.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(600));
            *later.lock().unwrap() = menu;
        });
        let mut p = Port::connect_to(port).unwrap();
        assert!(lap(&mut p, &events, &checkpoints).unwrap());
        let log = log.lock().unwrap();
        assert_eq!(log.len(), 3);
        assert_eq!(log[0].1["button"], "cross");
        assert!((log[0].0 - 0.1).abs() < 0.05, "first input at its recorded time: {}", log[0].0);
        let (stick_at, stick) = &log[2];
        assert_eq!(stick["stick"], "left");
        // reached at 0.6 s or a little later (two matching looks), then 0.2 s as recorded
        assert!(*stick_at > 0.75 && *stick_at < 1.2, "stick sent {stick_at:.2} s in");
    }

    #[test]
    fn a_checkpoint_that_never_comes_ends_the_replay() {
        let size = record::THUMB.0 * record::THUMB.1 * 3;
        let (port, log) = fake_port(Arc::new(Mutex::new(vec![0u8; size])));
        let checkpoints = vec![Checkpoint {
            t: 0.0,
            thumb: vec![255; size],
            region: record::FULL,
            tol: 8.0,
            changed: 3.0,
            timeout: 0.4,
        }];
        let events = vec![json!({"t": 0.0, "cp": 0}), json!({"t": 0.1, "b": "cross", "a": "press"})];
        let mut p = Port::connect_to(port).unwrap();
        let err = lap(&mut p, &events, &checkpoints).unwrap_err().to_string();
        assert!(err.contains("checkpoint 1 not reached"), "{err}");
        assert!(log.lock().unwrap().is_empty(), "nothing is pressed before the start screen is seen");
    }
}
