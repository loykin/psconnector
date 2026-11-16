//! Recording the local pad as a macro, with checkpoints: moments where the replay waits until the screen
//! looks the way it did while recording (so loading times and Remote Play delay do not shift what follows).
//!
//! A recording is saved as an ordinary macro folder, so it shows up in the macro list and runs with the same
//! time limit, rest mode and schedule as any other macro:
//!
//! ```text
//! macros/<name>/macro.json        {"name": "...", "recording": "recording.json", "repeat": 0}
//! macros/<name>/recording.json    {"version": 1, "events": [...], "checkpoints": [...]}
//! macros/<name>/checkpoints/NN.png       what the replay compares against (small)
//! macros/<name>/checkpoints/NN-view.png  the same moment at a readable size, for people
//! ```
//!
//! Events: `{"t": secs, "b": "cross", "a": "press|release"}`, `{"t": secs, "s": "left|right", "x": .., "y": ..}`
//! and `{"t": secs, "cp": index}`. Checkpoints: `{"t", "image", "region": [x0, y0, x1, y1], "tol", "changed",
//! "timeout"}` (region as fractions of the frame; the screen matches when its mean difference, 0..255, is at
//! most `tol` and at most `changed` percent of it changed a lot). For a checkpoint in a moving 3D scene,
//! raise both; to watch only part of the screen (a menu, a HUD element), narrow `region`. Only the checkpoints marked while recording
//! are used: the screen when recording starts is not one, because in a 3D game a lap rarely ends exactly
//! where it began (and a banner shown at that moment would never come back). Put checkpoints on screens that
//! hold still (a menu, a loaded area, a dialog). Replay: `replay.rs`.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use psconnector::{Button, Frame};
use serde_json::{Value, json};

/// Size of the picture checkpoints are compared at: small enough to ignore noise and small camera
/// movements, large enough to tell screens apart.
pub(crate) const THUMB: (usize, usize) = (80, 45);
/// Size of the picture saved for people to look at.
const VIEW: (usize, usize) = (480, 270);
/// Default for how different a screen may be on average and still match (mean absolute difference, 0..255).
/// A still PS5 screen with an animated background stays under 2; the two most alike different screens
/// measured (the same scene with another panel on top) were 8.1 apart.
pub(crate) const DEFAULT_TOL: f64 = 8.0;
/// Default for the share of the picture (percent) that may have changed a lot and still match. Catches a
/// dialog or panel that the average alone would miss: a still screen stays under 1.5%, a changed panel
/// was 5.5%.
pub(crate) const DEFAULT_CHANGED: f64 = 3.0;
/// A cell counts as changed a lot when one of its colours moved more than this.
const CHANGED_BY: u8 = 40;
/// Default for how long the replay waits for a checkpoint before giving up, in seconds.
pub(crate) const DEFAULT_TIMEOUT: f64 = 60.0;
pub(crate) const FULL: [f64; 4] = [0.0, 0.0, 1.0, 1.0];

/// Area-averaged RGB of `region` (fractions of the frame) at `w`×`h`.
pub(crate) fn downscale(f: &Frame, region: [f64; 4], (w, h): (usize, usize)) -> Vec<u8> {
    let rgb = f.rgb();
    let (fw, fh) = (f.width, f.height);
    let px = |v: f64, size: usize| ((v.clamp(0.0, 1.0) * size as f64) as usize).min(size);
    let (x0, y0) = (px(region[0], fw), px(region[1], fh));
    let (x1, y1) = (px(region[2], fw).max(x0 + 1).min(fw), px(region[3], fh).max(y0 + 1).min(fh));
    let (rw, rh) = (x1 - x0, y1 - y0);
    let mut out = vec![0u8; w * h * 3];
    for ty in 0..h {
        let (sy0, sy1) = (y0 + ty * rh / h, (y0 + (ty + 1) * rh / h).max(y0 + ty * rh / h + 1).min(y1));
        for tx in 0..w {
            let (sx0, sx1) = (x0 + tx * rw / w, (x0 + (tx + 1) * rw / w).max(x0 + tx * rw / w + 1).min(x1));
            let mut sum = [0u64; 3];
            for y in sy0..sy1 {
                let row = &rgb[(y * fw + sx0) * 3..(y * fw + sx1) * 3];
                for p in row.as_chunks::<3>().0 {
                    sum[0] += u64::from(p[0]);
                    sum[1] += u64::from(p[1]);
                    sum[2] += u64::from(p[2]);
                }
            }
            let n = ((sy1 - sy0) * (sx1 - sx0)).max(1) as u64;
            let o = (ty * w + tx) * 3;
            for c in 0..3 {
                out[o + c] = (sum[c] / n) as u8;
            }
        }
    }
    out
}

/// How far apart two checkpoint-sized pictures are.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Difference {
    /// Mean absolute difference, 0 (same) to 255
    pub(crate) mean: f64,
    /// Share of cells (percent) where a colour moved more than [`CHANGED_BY`]
    pub(crate) changed: f64,
}

impl Difference {
    pub(crate) fn within(&self, tol: f64, changed: f64) -> bool {
        self.mean <= tol && self.changed <= changed
    }
}

impl std::fmt::Display for Difference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "mean {:.1}, {:.1}% changed", self.mean, self.changed)
    }
}

/// Compares two RGB pictures of the same size.
pub(crate) fn difference(a: &[u8], b: &[u8]) -> Difference {
    if a.len() != b.len() || a.len() < 3 {
        return Difference { mean: 255.0, changed: 100.0 };
    }
    let (mut total, mut changed) = (0u64, 0usize);
    for (p, q) in a.as_chunks::<3>().0.iter().zip(b.as_chunks::<3>().0) {
        let d = [p[0].abs_diff(q[0]), p[1].abs_diff(q[1]), p[2].abs_diff(q[2])];
        total += d.iter().map(|&v| u64::from(v)).sum::<u64>();
        changed += usize::from(d.iter().any(|&v| v > CHANGED_BY));
    }
    let cells = a.len() / 3;
    Difference { mean: total as f64 / a.len() as f64, changed: changed as f64 * 100.0 / cells as f64 }
}

struct Checkpoint {
    t: f64,
    thumb: Vec<u8>,
    view: Vec<u8>,
}

/// One recording in progress, or finished and waiting to be saved.
pub(crate) struct Take {
    started: Instant,
    events: Vec<Value>,
    checkpoints: Vec<Checkpoint>,
    held: Vec<Button>,
    ended: Option<f64>,
}

impl Take {
    pub(crate) fn duration(&self) -> f64 {
        self.ended.unwrap_or_else(|| self.started.elapsed().as_secs_f64())
    }

    pub(crate) fn checkpoints(&self) -> usize {
        self.checkpoints.len()
    }

    /// Button and stick events (checkpoints not counted).
    pub(crate) fn inputs(&self) -> usize {
        self.events.iter().filter(|e| e.get("cp").is_none()).count()
    }

    fn now(&self) -> f64 {
        (self.started.elapsed().as_secs_f64() * 1000.0).round() / 1000.0
    }

    fn add_checkpoint(&mut self, f: &Frame) {
        let t = self.now();
        let index = self.checkpoints.len();
        self.checkpoints.push(Checkpoint { t, thumb: downscale(f, FULL, THUMB), view: downscale(f, FULL, VIEW) });
        self.events.push(json!({"t": t, "cp": index}));
    }
}

/// The pad thread reports what it sends here; the window starts and stops recording and adds checkpoints.
#[derive(Default)]
pub(crate) struct Recorder {
    take: Mutex<Option<Take>>,
}

impl Recorder {
    /// Starts recording. Checkpoints are only those marked with [`Recorder::checkpoint`].
    pub(crate) fn start(&self) {
        let take = Take {
            started: Instant::now(),
            events: Vec::new(),
            checkpoints: Vec::new(),
            held: Vec::new(),
            ended: None,
        };
        *self.take.lock().unwrap() = Some(take);
    }

    pub(crate) fn recording(&self) -> bool {
        self.take.lock().unwrap().is_some()
    }

    /// Duration, checkpoints and inputs so far, for the bar.
    pub(crate) fn progress(&self) -> Option<(f64, usize, usize)> {
        self.take.lock().unwrap().as_ref().map(|t| (t.duration(), t.checkpoints(), t.inputs()))
    }

    pub(crate) fn checkpoint(&self, frame: &Frame) {
        if let Some(t) = self.take.lock().unwrap().as_mut() {
            t.add_checkpoint(frame);
        }
    }

    pub(crate) fn button(&self, b: Button, down: bool) {
        if let Some(t) = self.take.lock().unwrap().as_mut() {
            let at = t.now();
            t.events.push(json!({"t": at, "b": b.name(), "a": if down { "press" } else { "release" }}));
            t.held.retain(|h| *h != b);
            if down {
                t.held.push(b);
            }
        }
    }

    pub(crate) fn stick(&self, right: bool, x: f32, y: f32) {
        if let Some(t) = self.take.lock().unwrap().as_mut() {
            let at = t.now();
            let r = |v: f32| (f64::from(v) * 1000.0).round() / 1000.0;
            t.events.push(json!({"t": at, "s": if right { "right" } else { "left" }, "x": r(x), "y": r(y)}));
        }
    }

    /// Stops recording. Whatever is still held is released at the end, so a replay never leaves it pressed.
    pub(crate) fn stop(&self) -> Option<Take> {
        let mut take = self.take.lock().unwrap().take()?;
        let at = take.now();
        for b in std::mem::take(&mut take.held) {
            take.events.push(json!({"t": at, "b": b.name(), "a": "release"}));
        }
        take.ended = Some(at);
        Some(take)
    }
}

/// A folder name from what the person typed (no path separators or characters Windows refuses).
fn folder_name(name: &str) -> String {
    let s: String =
        name.trim().chars().map(|c| if c.is_control() || r#"/\:*?"<>|"#.contains(c) { '_' } else { c }).collect();
    let s = s.trim_matches(|c| c == '.' || c == ' ').to_string();
    if s.is_empty() { "recording".into() } else { s }
}

/// Saves a recording as a macro folder inside `macros` and returns the folder.
pub(crate) fn save(take: &Take, macros: &Path, name: &str) -> Result<PathBuf> {
    let name = name.trim();
    if name.is_empty() {
        bail!("the recording needs a name");
    }
    let base = folder_name(name);
    let mut dir = macros.join(&base);
    let mut n = 2;
    while dir.exists() {
        dir = macros.join(format!("{base} {n}"));
        n += 1;
    }
    std::fs::create_dir_all(dir.join("checkpoints")).with_context(|| format!("cannot create {}", dir.display()))?;
    let mut checkpoints = Vec::new();
    for (i, c) in take.checkpoints.iter().enumerate() {
        let image = format!("checkpoints/{:02}.png", i + 1);
        write_png(&dir.join(&image), THUMB, &c.thumb)?;
        write_png(&dir.join(format!("checkpoints/{:02}-view.png", i + 1)), VIEW, &c.view)?;
        checkpoints.push(json!({
            "t": c.t, "image": image, "region": FULL, "tol": DEFAULT_TOL, "changed": DEFAULT_CHANGED, "timeout": DEFAULT_TIMEOUT,
        }));
    }
    let recording = json!({
        "version": 1,
        "duration": take.duration(),
        "events": take.events,
        "checkpoints": checkpoints,
    });
    std::fs::write(dir.join("recording.json"), serde_json::to_string_pretty(&recording)?)?;
    std::fs::write(
        dir.join("macro.json"),
        serde_json::to_string_pretty(&json!({"name": name, "recording": "recording.json", "repeat": 0}))?,
    )?;
    Ok(dir)
}

fn write_png(path: &Path, (w, h): (usize, usize), rgb: &[u8]) -> Result<()> {
    std::fs::write(path, crate::png_bytes(w, h, rgb)?).with_context(|| format!("cannot write {}", path.display()))
}

/// Reads a PNG written by [`save`] back as RGB.
pub(crate) fn read_png(path: &Path) -> Result<(usize, usize, Vec<u8>)> {
    let file = std::fs::File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let mut dec = png::Decoder::new(std::io::BufReader::new(file));
    dec.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = dec.read_info()?;
    let mut buf = vec![0; reader.output_buffer_size().context("image too large")?];
    let info = reader.next_frame(&mut buf)?;
    let (w, h) = (info.width as usize, info.height as usize);
    let rgb = match info.color_type {
        png::ColorType::Rgb => buf[..w * h * 3].to_vec(),
        png::ColorType::Rgba => buf[..w * h * 4].as_chunks::<4>().0.iter().flat_map(|p| [p[0], p[1], p[2]]).collect(),
        png::ColorType::Grayscale => buf[..w * h].iter().flat_map(|&g| [g, g, g]).collect(),
        other => bail!("{}: unsupported PNG colour type {other:?}", path.display()),
    };
    Ok((w, h, rgb))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(w: usize, h: usize, px: impl Fn(usize, usize) -> [u8; 3]) -> Frame {
        let mut rgb = Vec::with_capacity(w * h * 3);
        for y in 0..h {
            for x in 0..w {
                rgb.extend_from_slice(&px(x, y));
            }
        }
        Frame::new(w, h, psconnector::PixelFormat::Rgb8, rgb, 0)
    }

    #[test]
    fn downscale_averages_and_follows_the_region() {
        // left half black, right half white
        let f = frame(160, 90, |x, _| if x < 80 { [0; 3] } else { [255; 3] });
        let t = downscale(&f, FULL, (2, 1));
        assert_eq!(t, vec![0, 0, 0, 255, 255, 255]);
        let right = downscale(&f, [0.5, 0.0, 1.0, 1.0], (4, 2));
        assert!(right.iter().all(|&v| v == 255));
        assert_eq!(downscale(&f, FULL, THUMB).len(), THUMB.0 * THUMB.1 * 3);
    }

    #[test]
    fn small_changes_match_and_another_screen_does_not() {
        let a = frame(320, 180, |x, y| [(x % 256) as u8, (y % 256) as u8, 90]);
        let noisy = frame(320, 180, |x, y| [((x + 1) % 256) as u8, (y % 256) as u8, 93]);
        let other = frame(320, 180, |_, _| [20, 20, 20]);
        let (ta, tn, to) = (downscale(&a, FULL, THUMB), downscale(&noisy, FULL, THUMB), downscale(&other, FULL, THUMB));
        assert_eq!(difference(&ta, &ta), Difference { mean: 0.0, changed: 0.0 });
        assert!(difference(&ta, &tn).within(DEFAULT_TOL, DEFAULT_CHANGED), "{}", difference(&ta, &tn));
        assert!(!difference(&ta, &to).within(DEFAULT_TOL, DEFAULT_CHANGED), "{}", difference(&ta, &to));
        // a panel over a fifth of an otherwise identical screen: small on average, but caught
        let panel =
            frame(320, 180, |x, y| if x < 64 { [250, 250, 250] } else { [(x % 256) as u8, (y % 256) as u8, 90] });
        let d = difference(&ta, &downscale(&panel, FULL, THUMB));
        assert!(!d.within(DEFAULT_TOL * 10.0, DEFAULT_CHANGED), "{d}");
    }

    #[test]
    fn a_take_keeps_marked_checkpoints_and_releases_what_is_held() {
        let r = Recorder::default();
        r.start();
        r.checkpoint(&frame(16, 9, |_, _| [1, 2, 3]));
        r.button(Button::Cross, true);
        r.stick(false, 0.5, -1.0);
        r.button(Button::Circle, true);
        r.button(Button::Circle, false);
        let take = r.stop().unwrap();
        assert!(!r.recording());
        assert_eq!(take.checkpoints(), 1);
        assert_eq!(take.events[0]["cp"], 0);
        let last = take.events.last().unwrap();
        assert_eq!((last["b"].as_str(), last["a"].as_str()), (Some("cross"), Some("release")));

        let dir = std::env::temp_dir().join(format!("psconnector-rec-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let saved = save(&take, &dir, "My: run").unwrap();
        assert_eq!(saved.file_name().unwrap(), "My_ run");
        let rec: Value = serde_json::from_str(&std::fs::read_to_string(saved.join("recording.json")).unwrap()).unwrap();
        let (w, h, px) = read_png(&saved.join(rec["checkpoints"][0]["image"].as_str().unwrap())).unwrap();
        assert_eq!((w, h), THUMB);
        assert_eq!(&px[..3], &[1, 2, 3]);
        let def: Value = serde_json::from_str(&std::fs::read_to_string(saved.join("macro.json")).unwrap()).unwrap();
        assert_eq!(def["name"], "My: run");
        assert_eq!(def["repeat"], 0, "a recording runs until stopped unless told otherwise");
        assert!(save(&take, &dir, "My: run").unwrap().ends_with("My_ run 2"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
