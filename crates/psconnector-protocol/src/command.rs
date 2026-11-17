//! Commands: one JSON object per line. The key that names a command is looked for in a fixed order, so an
//! object with several command keys means the first one in [`Command::parse`].

use psconnector::{Button, TOUCHPAD_SIZE, VideoCodec, VideoRange};
use serde_json::{Map, Value, json};

/// A command line that could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BadCommand(pub String);

impl std::fmt::Display for BadCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for BadCommand {}

fn bad<T>(msg: impl Into<String>) -> Result<T, BadCommand> {
    Err(BadCommand(msg.into()))
}

/// Default hold for `tap`, in seconds.
pub const TAP_HOLD: f64 = 0.08;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ButtonAction {
    /// Press, wait `hold` seconds, release; the reply comes after the release.
    Tap {
        hold: f64,
    },
    Press,
    Release,
}

/// `ocr` region.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum OcrRegion {
    /// `[x, y, w, h]` in frame pixels
    Pixels([u32; 4]),
    /// `[x0, y0, x1, y1]` as fractions of the frame (`"rel": true`), so it does not depend on the quality
    Fractions([f64; 4]),
    /// `[]`: the whole frame
    Whole,
}

/// `input_bench`: repeatable, normally invisible input load for measuring the input queue.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct InputBench {
    pub seconds: f64,
    pub interval_ms: u64,
    pub burst: usize,
    /// Stick deflection; the default stays below the console's dead zone
    pub amplitude: f32,
}

/// Overrides for a macro run's time limit, after-action and repeat count. Also read from `macro.json`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RunOptions {
    /// Minutes, 0 = no limit
    pub limit_min: Option<f64>,
    /// `none` or `standby` (checked by the macro runner)
    pub after: Option<String>,
    /// Recordings: plays per run, 0 = until stopped
    pub repeat: Option<u64>,
}

impl RunOptions {
    /// Reads `limit_min`, `after` and `repeat` from an object; missing (or null) fields stay `None`.
    pub fn from_json(v: &Value) -> Result<Self, BadCommand> {
        let limit_min = match &v["limit_min"] {
            Value::Null => None,
            n => match n.as_f64().filter(|n| *n >= 0.0) {
                Some(n) => Some(n),
                None => return bad("limit_min must be a number of minutes"),
            },
        };
        let repeat = match &v["repeat"] {
            Value::Null => None,
            n => match n.as_u64() {
                Some(n) => Some(n),
                None => return bad("repeat must be a whole number (0 = until stopped)"),
            },
        };
        Ok(Self { limit_min, after: v["after"].as_str().map(str::to_string), repeat })
    }

    fn write(&self, m: &mut Map<String, Value>) {
        if let Some(v) = self.limit_min {
            m.insert("limit_min".into(), v.into());
        }
        if let Some(v) = &self.after {
            m.insert("after".into(), v.clone().into());
        }
        if let Some(v) = self.repeat {
            m.insert("repeat".into(), v.into());
        }
    }
}

/// `picture`: change the display adjustment.
#[derive(Clone, Debug, PartialEq)]
pub enum PictureChange {
    /// Only these values (`{"shadows": 0.5, ...}`)
    Values(Map<String, Value>),
    /// A named preset (built-in or saved)
    Preset(String),
    /// Change nothing, only report
    Report,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    /// `{"button":"cross","action":"tap|press|release","hold":0.08}`
    Button { button: Button, action: ButtonAction },
    /// `{"stick":"left|right","x":0,"y":-1}` (up is negative)
    Stick { right: bool, x: f32, y: f32 },
    /// `{"touch":0,"down":true,"x":960,"y":540}`: a finger on the touchpad ([`TOUCHPAD_SIZE`])
    Touch { id: u8, down: bool, x: u16, y: u16 },
    /// `{"swipe":[x0,y0,x1,y1],"ms":300}`: one finger moved across the touchpad; the reply comes when done
    Swipe { from: (f64, f64), to: (f64, f64), ms: f64 },
    /// `{"release_all":true}`
    ReleaseAll,
    /// `{"frame":true}`: reply `ok W H BYTES` and then the RGB bytes
    Frame,
    /// `{"thumb":[x0,y0,x1,y1]}` (fractions; `true` = whole frame): the screen at checkpoint size, like `frame`
    Thumb { region: Option<[f64; 4]> },
    /// `{"ocr":[x,y,w,h],"scale":2}`: reply `ok ["line", ...]`
    Ocr { region: OcrRegion, scale: f32 },
    /// `{"snap":"path.png"}`: the decoded frame
    Snap(String),
    /// `{"screen_snap":"path.png"}`: the window as drawn, picture adjustment included
    ScreenSnap(String),
    /// `{"picture_snap":"path.png"}`: the decoded frame with the picture adjustment applied
    PictureSnap(String),
    /// `{"play":"recording.json"}`: plays an old-style recording (a list of timed events)
    Play(String),
    /// `{"stop":true}`: stops `play` and `input_bench`
    Stop,
    /// `{"input_bench":{"seconds":12,"interval_ms":10,"burst":4,"amplitude":0.01}}`
    InputBench(InputBench),
    /// `{"status":true}`: reply `ok playing` or `ok idle`
    Status,
    /// `{"osd":"text"}`: text in the top-left of the window
    Osd(String),
    /// `{"standby":true}`: rest mode
    Standby,
    /// `{"macros":true,"reload":true}`: reply `ok [JSON]`
    Macros { reload: bool },
    /// `{"macro_run":"name","limit_min":60,"after":"standby","at":"23:00"}` (all but the name optional)
    MacroRun { name: String, options: RunOptions, at: Option<String> },
    /// `{"quality":"low|720p|1080p"}` reconnects at that quality; `""` (`None`) only reports. Reply `ok {JSON}`
    Quality(Option<String>),
    /// `{"video_codec":"h264|h265|h265-hdr"}`; `""` (`None`) only reports
    VideoCodec(Option<VideoCodec>),
    /// `{"video_range":"auto|limited|full"}`; `""` (`None`) only reports
    VideoRange(Option<VideoRange>),
    /// `{"picture_save":"name"}`: saves the current picture adjustment and range as a preset
    PictureSave(String),
    /// `{"picture_delete":"name"}`
    PictureDelete(String),
    /// `{"picture":{...}}` / `{"picture":"preset"}` / `{"picture":""}`: reply `ok {current values}`
    Picture(PictureChange),
    /// `{"should_stop":true}`: reply `ok yes` once the macro's time limit is up, else `ok no`
    ShouldStop,
    /// `{"macro_stop":true}`: also cancels a schedule
    MacroStop,
    /// `{"macro_status":true,"lines":10}`: reply `ok {JSON}`
    MacroStatus { lines: usize },
}

/// A touchpad point from coordinates that may be off the pad or fractional.
pub fn touch_point(x: f64, y: f64) -> (u16, u16) {
    let (w, h) = TOUCHPAD_SIZE;
    let c = |v: f64, max: u16| v.round().clamp(0.0, f64::from(max - 1)) as u16;
    (c(x, w), c(y, h))
}

fn numbers(v: &Value) -> Vec<f64> {
    v.as_array().map(|a| a.iter().filter_map(Value::as_f64).collect()).unwrap_or_default()
}

impl Command {
    /// Reads one command line.
    pub fn parse_line(line: &str) -> Result<Self, BadCommand> {
        let m: Value = serde_json::from_str(line).map_err(|_| BadCommand("not JSON".into()))?;
        Self::parse(&m)
    }

    pub fn parse(m: &Value) -> Result<Self, BadCommand> {
        let string = |key: &str| m[key].as_str().map(str::to_string);
        let has = |key: &str| m.get(key).is_some();
        if let Some(name) = m["button"].as_str() {
            let Some(button) = Button::parse(name) else { return bad(format!("unknown button: {name}")) };
            let action = match m["action"].as_str().unwrap_or("tap") {
                "press" => ButtonAction::Press,
                "release" => ButtonAction::Release,
                "tap" => ButtonAction::Tap { hold: m["hold"].as_f64().unwrap_or(TAP_HOLD).max(0.0) },
                other => return bad(format!("unknown action: {other}")),
            };
            return Ok(Self::Button { button, action });
        }
        if let Some(which) = m["stick"].as_str() {
            let (x, y) = (m["x"].as_f64().unwrap_or(0.0) as f32, m["y"].as_f64().unwrap_or(0.0) as f32);
            return Ok(Self::Stick { right: which == "right", x, y });
        }
        if let Some(id) = m["touch"].as_u64() {
            let (x, y) = touch_point(m["x"].as_f64().unwrap_or(0.0), m["y"].as_f64().unwrap_or(0.0));
            return Ok(Self::Touch { id: id as u8, down: m["down"].as_bool().unwrap_or(true), x, y });
        }
        if m["swipe"].is_array() {
            let [x0, y0, x1, y1] = numbers(&m["swipe"])[..] else { return bad("swipe needs [x0, y0, x1, y1]") };
            let ms = m["ms"].as_f64().unwrap_or(300.0).clamp(16.0, 10_000.0);
            return Ok(Self::Swipe { from: (x0, y0), to: (x1, y1), ms });
        }
        if has("release_all") {
            return Ok(Self::ReleaseAll);
        }
        if has("frame") {
            return Ok(Self::Frame);
        }
        if has("thumb") {
            let region = match numbers(&m["thumb"])[..] {
                [x0, y0, x1, y1] => Some([x0, y0, x1, y1]),
                [] => None,
                _ => return bad("thumb region must be [x0, y0, x1, y1] fractions"),
            };
            return Ok(Self::Thumb { region });
        }
        if has("ocr") {
            let v = numbers(&m["ocr"]);
            let region = if m["rel"].as_bool() == Some(true) {
                let [x0, y0, x1, y1] = v[..] else { return bad("ocr with rel needs [x0, y0, x1, y1] fractions") };
                OcrRegion::Fractions([x0, y0, x1, y1])
            } else {
                match v.iter().map(|v| v.max(0.0) as u32).collect::<Vec<_>>()[..] {
                    [x, y, w, h] => OcrRegion::Pixels([x, y, w, h]),
                    [] => OcrRegion::Whole,
                    _ => return bad("ocr region must be [x, y, w, h]"),
                }
            };
            return Ok(Self::Ocr { region, scale: m["scale"].as_f64().unwrap_or(1.0) as f32 });
        }
        if let Some(path) = string("snap") {
            return Ok(Self::Snap(path));
        }
        if let Some(path) = string("screen_snap") {
            return Ok(Self::ScreenSnap(path));
        }
        if let Some(path) = string("picture_snap") {
            return Ok(Self::PictureSnap(path));
        }
        if let Some(path) = string("play") {
            return Ok(Self::Play(path));
        }
        if has("stop") {
            return Ok(Self::Stop);
        }
        if let Some(o) = m.get("input_bench") {
            return Ok(Self::InputBench(InputBench {
                seconds: o["seconds"].as_f64().unwrap_or(12.0).clamp(2.0, 30.0),
                interval_ms: o["interval_ms"].as_u64().unwrap_or(10).clamp(1, 100),
                burst: o["burst"].as_u64().unwrap_or(4).clamp(1, 20) as usize,
                amplitude: o["amplitude"].as_f64().unwrap_or(0.01).clamp(0.0, 1.0) as f32,
            }));
        }
        if has("status") {
            return Ok(Self::Status);
        }
        if let Some(text) = m.get("osd") {
            return Ok(Self::Osd(text.as_str().map(str::to_string).unwrap_or_else(|| text.to_string())));
        }
        if has("standby") {
            return Ok(Self::Standby);
        }
        if has("macros") {
            return Ok(Self::Macros { reload: m["reload"].as_bool() == Some(true) });
        }
        if let Some(name) = string("macro_run") {
            return Ok(Self::MacroRun { name, options: RunOptions::from_json(m)?, at: string("at") });
        }
        if let Some(q) = m["quality"].as_str() {
            return Ok(Self::Quality((!q.is_empty()).then(|| q.to_string())));
        }
        if let Some(name) = m["video_codec"].as_str() {
            if name.is_empty() {
                return Ok(Self::VideoCodec(None));
            }
            let Some(codec) = VideoCodec::parse(name) else { return bad("video_codec must be h264, h265 or h265-hdr") };
            return Ok(Self::VideoCodec(Some(codec)));
        }
        if let Some(name) = m["video_range"].as_str() {
            if name.is_empty() {
                return Ok(Self::VideoRange(None));
            }
            let Some(range) = VideoRange::parse(name) else { return bad("video_range must be auto, limited or full") };
            return Ok(Self::VideoRange(Some(range)));
        }
        if let Some(name) = string("picture_save") {
            return Ok(Self::PictureSave(name));
        }
        if let Some(name) = string("picture_delete") {
            return Ok(Self::PictureDelete(name));
        }
        if let Some(v) = m.get("picture") {
            return Ok(Self::Picture(match v {
                Value::Object(values) => PictureChange::Values(values.clone()),
                Value::String(name) if !name.is_empty() => PictureChange::Preset(name.clone()),
                _ => PictureChange::Report,
            }));
        }
        if has("should_stop") {
            return Ok(Self::ShouldStop);
        }
        if has("macro_stop") {
            return Ok(Self::MacroStop);
        }
        if has("macro_status") {
            return Ok(Self::MacroStatus { lines: m["lines"].as_u64().unwrap_or(10) as usize });
        }
        bad("unknown command")
    }

    /// The JSON object [`parse`](Self::parse) reads back as this command.
    pub fn to_json(&self) -> Value {
        match self {
            Self::Button { button, action } => match action {
                ButtonAction::Tap { hold } => json!({"button": button.name(), "action": "tap", "hold": hold}),
                ButtonAction::Press => json!({"button": button.name(), "action": "press"}),
                ButtonAction::Release => json!({"button": button.name(), "action": "release"}),
            },
            Self::Stick { right, x, y } => {
                json!({"stick": if *right { "right" } else { "left" }, "x": x, "y": y})
            }
            Self::Touch { id, down, x, y } => json!({"touch": id, "down": down, "x": x, "y": y}),
            Self::Swipe { from, to, ms } => json!({"swipe": [from.0, from.1, to.0, to.1], "ms": ms}),
            Self::ReleaseAll => json!({"release_all": true}),
            Self::Frame => json!({"frame": true}),
            Self::Thumb { region: Some(r) } => json!({"thumb": r}),
            Self::Thumb { region: None } => json!({"thumb": true}),
            Self::Ocr { region, scale } => match region {
                OcrRegion::Pixels(r) => json!({"ocr": r, "scale": scale}),
                OcrRegion::Fractions(r) => json!({"ocr": r, "rel": true, "scale": scale}),
                OcrRegion::Whole => json!({"ocr": [], "scale": scale}),
            },
            Self::Snap(p) => json!({"snap": p}),
            Self::ScreenSnap(p) => json!({"screen_snap": p}),
            Self::PictureSnap(p) => json!({"picture_snap": p}),
            Self::Play(p) => json!({"play": p}),
            Self::Stop => json!({"stop": true}),
            Self::InputBench(b) => json!({"input_bench": {
                "seconds": b.seconds, "interval_ms": b.interval_ms, "burst": b.burst, "amplitude": b.amplitude,
            }}),
            Self::Status => json!({"status": true}),
            Self::Osd(text) => json!({"osd": text}),
            Self::Standby => json!({"standby": true}),
            Self::Macros { reload } => json!({"macros": true, "reload": reload}),
            Self::MacroRun { name, options, at } => {
                let mut m = Map::new();
                m.insert("macro_run".into(), name.clone().into());
                options.write(&mut m);
                if let Some(at) = at {
                    m.insert("at".into(), at.clone().into());
                }
                Value::Object(m)
            }
            Self::Quality(q) => json!({"quality": q.as_deref().unwrap_or("")}),
            Self::VideoCodec(c) => json!({"video_codec": c.map(VideoCodec::name).unwrap_or("")}),
            Self::VideoRange(r) => json!({"video_range": r.map(VideoRange::name).unwrap_or("")}),
            Self::PictureSave(name) => json!({"picture_save": name}),
            Self::PictureDelete(name) => json!({"picture_delete": name}),
            Self::Picture(change) => match change {
                PictureChange::Values(v) => json!({"picture": v}),
                PictureChange::Preset(name) => json!({"picture": name}),
                PictureChange::Report => json!({"picture": ""}),
            },
            Self::ShouldStop => json!({"should_stop": true}),
            Self::MacroStop => json!({"macro_stop": true}),
            Self::MacroStatus { lines } => json!({"macro_status": true, "lines": lines}),
        }
    }

    /// The command as one line, newline included.
    pub fn to_line(&self) -> String {
        format!("{}\n", self.to_json())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(v: Value) -> Result<Command, BadCommand> {
        Command::parse(&v)
    }

    #[test]
    fn every_command_reads_back_as_itself() {
        let all = [
            Command::Button { button: Button::Cross, action: ButtonAction::Tap { hold: 0.08 } },
            Command::Button { button: Button::R2, action: ButtonAction::Press },
            Command::Button { button: Button::Ps, action: ButtonAction::Release },
            Command::Stick { right: true, x: 0.5, y: -1.0 },
            Command::Stick { right: false, x: 0.0, y: 0.25 },
            Command::Touch { id: 3, down: false, x: 960, y: 540 },
            Command::Swipe { from: (100.0, 200.0), to: (1800.0, 200.0), ms: 300.0 },
            Command::ReleaseAll,
            Command::Frame,
            Command::Thumb { region: Some([0.0, 0.5, 1.0, 1.0]) },
            Command::Thumb { region: None },
            Command::Ocr { region: OcrRegion::Pixels([10, 20, 300, 40]), scale: 2.0 },
            Command::Ocr { region: OcrRegion::Fractions([0.1, 0.2, 0.3, 0.4]), scale: 1.0 },
            Command::Ocr { region: OcrRegion::Whole, scale: 1.0 },
            Command::Snap("a.png".into()),
            Command::ScreenSnap("b.png".into()),
            Command::PictureSnap("c.png".into()),
            Command::Play("rec.json".into()),
            Command::Stop,
            Command::InputBench(InputBench { seconds: 5.0, interval_ms: 20, burst: 2, amplitude: 0.5 }),
            Command::Status,
            Command::Osd("hello".into()),
            Command::Standby,
            Command::Macros { reload: true },
            Command::Macros { reload: false },
            Command::MacroRun {
                name: "lap".into(),
                options: RunOptions { limit_min: Some(60.0), after: Some("standby".into()), repeat: Some(3) },
                at: Some("23:00".into()),
            },
            Command::MacroRun { name: "lap".into(), options: RunOptions::default(), at: None },
            Command::Quality(Some("1080p".into())),
            Command::Quality(None),
            Command::VideoCodec(Some(VideoCodec::H265Hdr)),
            Command::VideoCodec(None),
            Command::VideoRange(Some(VideoRange::Full)),
            Command::VideoRange(None),
            Command::PictureSave("Dark game".into()),
            Command::PictureDelete("Dark game".into()),
            Command::Picture(PictureChange::Values(json!({"shadows": 0.5}).as_object().unwrap().clone())),
            Command::Picture(PictureChange::Preset("Dark game".into())),
            Command::Picture(PictureChange::Report),
            Command::ShouldStop,
            Command::MacroStop,
            Command::MacroStatus { lines: 25 },
        ];
        for c in all {
            assert_eq!(Command::parse_line(&c.to_line()).as_ref(), Ok(&c), "{}", c.to_json());
        }
    }

    #[test]
    fn defaults_and_limits_match_the_documented_ones() {
        assert_eq!(
            parse(json!({"button": "CROSS"})),
            Ok(Command::Button { button: Button::Cross, action: ButtonAction::Tap { hold: TAP_HOLD } })
        );
        assert_eq!(parse(json!({"stick": "up"})), Ok(Command::Stick { right: false, x: 0.0, y: 0.0 }));
        assert_eq!(
            parse(json!({"touch": 1, "x": -5, "y": 5000})),
            Ok(Command::Touch { id: 1, down: true, x: 0, y: 1079 })
        );
        assert_eq!(
            parse(json!({"swipe": [0, 0, 1, 1], "ms": 1})),
            Ok(Command::Swipe { from: (0.0, 0.0), to: (1.0, 1.0), ms: 16.0 })
        );
        assert_eq!(parse(json!({"thumb": []})), Ok(Command::Thumb { region: None }));
        assert_eq!(parse(json!({"osd": 5})), Ok(Command::Osd("5".into())));
        assert_eq!(parse(json!({"macro_status": true})), Ok(Command::MacroStatus { lines: 10 }));
        assert_eq!(
            parse(json!({"input_bench": {}})),
            Ok(Command::InputBench(InputBench { seconds: 12.0, interval_ms: 10, burst: 4, amplitude: 0.01 }))
        );
        // any value names the flag commands, as before
        assert_eq!(parse(json!({"release_all": false})), Ok(Command::ReleaseAll));
    }

    #[test]
    fn the_first_command_key_wins() {
        assert!(matches!(parse(json!({"stick": "left", "button": "cross"})), Ok(Command::Button { .. })));
        assert!(matches!(parse(json!({"macro_stop": true, "status": true})), Ok(Command::Status)));
    }

    #[test]
    fn bad_commands_say_why() {
        let err = |v: Value| parse(v).unwrap_err().0;
        assert_eq!(err(json!({"button": "jump"})), "unknown button: jump");
        assert_eq!(err(json!({"button": "cross", "action": "hold"})), "unknown action: hold");
        assert_eq!(err(json!({"swipe": [1, 2]})), "swipe needs [x0, y0, x1, y1]");
        assert_eq!(err(json!({"ocr": [1, 2], "rel": true})), "ocr with rel needs [x0, y0, x1, y1] fractions");
        assert_eq!(err(json!({"video_codec": "vp9"})), "video_codec must be h264, h265 or h265-hdr");
        assert_eq!(err(json!({"macro_run": "x", "limit_min": "soon"})), "limit_min must be a number of minutes");
        assert_eq!(err(json!({"jump": true})), "unknown command");
        assert_eq!(Command::parse_line("{").unwrap_err().0, "not JSON");
    }
}
