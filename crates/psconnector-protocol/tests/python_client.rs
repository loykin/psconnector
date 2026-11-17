//! The lines `python/psconnector.py` sends (as `json.dumps` writes them) are read as the commands it means.

use psconnector::Button;
use psconnector_protocol::{ButtonAction, Command, OcrRegion};

#[test]
fn python_client_lines() {
    let cases = [
        (
            r#"{"button": "cross", "action": "tap", "hold": 0.08}"#,
            Command::Button { button: Button::Cross, action: ButtonAction::Tap { hold: 0.08 } },
        ),
        (r#"{"button": "r2", "action": "press"}"#, Command::Button { button: Button::R2, action: ButtonAction::Press }),
        (r#"{"stick": "left", "x": 0.0, "y": -1.0}"#, Command::Stick { right: false, x: 0.0, y: -1.0 }),
        (
            r#"{"ocr": [10.0, 20.0, 300.0, 40.0], "scale": 2.0}"#,
            Command::Ocr { region: OcrRegion::Pixels([10, 20, 300, 40]), scale: 2.0 },
        ),
        (r#"{"ocr": [], "scale": 1.0}"#, Command::Ocr { region: OcrRegion::Whole, scale: 1.0 }),
        (r#"{"osd": "lap 3"}"#, Command::Osd("lap 3".into())),
        (r#"{"play": "rec.json"}"#, Command::Play("rec.json".into())),
        (r#"{"quality": ""}"#, Command::Quality(None)),
        (r#"{"quality": "low"}"#, Command::Quality(Some("low".into()))),
        (r#"{"release_all": true}"#, Command::ReleaseAll),
        (r#"{"should_stop": true}"#, Command::ShouldStop),
        (r#"{"snap": "a.png"}"#, Command::Snap("a.png".into())),
        (r#"{"standby": true}"#, Command::Standby),
        (r#"{"status": true}"#, Command::Status),
        (r#"{"stop": true}"#, Command::Stop),
        (r#"{"frame": true}"#, Command::Frame),
    ];
    for (line, want) in cases {
        assert_eq!(Command::parse_line(line), Ok(want), "{line}");
    }
}
