//! A game controller connected to this computer (DualSense, Xbox pad, ...) drives the PS5, like the
//! official Remote Play app. Runs on its own thread; input goes to the same send queue as macro input.
//!
//! Sticks are only sent when they move, so an idle pad does not override a macro's stick commands.
//! While Record is on, everything sent is also handed to the recorder.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use gilrs::{Axis, Button as Pad, EventType, Gilrs};
use psconnector::{Button, Client};

/// Stick values inside this radius count as centered (pads rest slightly off zero).
const DEAD_ZONE: f32 = 0.08;
/// Smallest stick change worth sending.
const STEP: f32 = 0.01;
/// How often the time from the pad's report to sending it is logged (only when there was input)
const REPORT_EVERY: Duration = Duration::from_secs(10);

/// Time from the OS reporting a pad event to it being handed to the stream, in microseconds.
#[derive(Default)]
struct Delay {
    samples: Vec<u64>,
    since: Option<std::time::Instant>,
}

impl Delay {
    fn add(&mut self, at: std::time::SystemTime) {
        if let Ok(d) = std::time::SystemTime::now().duration_since(at) {
            self.samples.push(d.as_micros() as u64);
        }
        let since = *self.since.get_or_insert_with(std::time::Instant::now);
        if since.elapsed() >= REPORT_EVERY && !self.samples.is_empty() {
            self.samples.sort_unstable();
            let n = self.samples.len();
            let pct = |p: usize| self.samples[(n * p / 100).min(n - 1)] as f64 / 1000.0;
            log::info!(
                "pad: {n} events in 10 s, report-to-send p50 {:.2} ms, p99 {:.2} ms, max {:.2} ms",
                pct(50),
                pct(99),
                *self.samples.last().unwrap() as f64 / 1000.0
            );
            self.samples.clear();
            self.since = Some(std::time::Instant::now());
        }
    }
}

fn map(b: Pad) -> Option<Button> {
    Some(match b {
        Pad::South => Button::Cross,
        Pad::East => Button::Circle,
        Pad::West => Button::Square,
        Pad::North => Button::Triangle,
        Pad::LeftTrigger => Button::L1,
        Pad::RightTrigger => Button::R1,
        Pad::LeftTrigger2 => Button::L2,
        Pad::RightTrigger2 => Button::R2,
        Pad::Select => Button::Share,
        Pad::Start => Button::Options,
        Pad::Mode => Button::Ps,
        Pad::LeftThumb => Button::L3,
        Pad::RightThumb => Button::R3,
        Pad::DPadUp => Button::Up,
        Pad::DPadDown => Button::Down,
        Pad::DPadLeft => Button::Left,
        Pad::DPadRight => Button::Right,
        _ => return None,
    })
}

/// PS5 buttons for pad buttons gilrs leaves unnamed, by their raw code. gilrs reports the DualSense
/// touchpad click (and the Edge's extra buttons) as `Unknown`, so without this the map, menus and anything
/// else on the touchpad cannot be reached from a local pad.
fn unnamed(raw: u32) -> Option<Button> {
    // macOS (IOKit): HID usage page 9 (buttons) << 16 | usage; the DualSense touchpad click is usage 14
    const MACOS_TOUCHPAD: u32 = 0x0009_000E;
    (cfg!(target_os = "macos") && raw == MACOS_TOUCHPAD).then_some(Button::Touchpad)
}

/// Current stick position from the pad, and what was last sent to the PS5.
#[derive(Default)]
struct Sticks {
    raw: [f32; 4],
    sent: [f32; 4],
}

impl Sticks {
    fn set(&mut self, axis: Axis, v: f32) -> Option<bool> {
        // gilrs Y points up, the PS5 expects Y down
        let (i, v) = match axis {
            Axis::LeftStickX => (0, v),
            Axis::LeftStickY => (1, -v),
            Axis::RightStickX => (2, v),
            Axis::RightStickY => (3, -v),
            _ => return None,
        };
        self.raw[i] = v;
        Some(i >= 2)
    }

    /// Dead-zoned position of one stick if it changed enough since the last send.
    fn changed(&mut self, right: bool) -> Option<(f32, f32)> {
        let i = if right { 2 } else { 0 };
        let (x, y) = (self.raw[i], self.raw[i + 1]);
        let (x, y) = if x.hypot(y) < DEAD_ZONE { (0.0, 0.0) } else { (x, y) };
        let [sx, sy] = [self.sent[i], self.sent[i + 1]];
        let centered_now = x == 0.0 && y == 0.0 && (sx != 0.0 || sy != 0.0);
        if (x - sx).abs() < STEP && (y - sy).abs() < STEP && !centered_now {
            return None;
        }
        self.sent[i] = x;
        self.sent[i + 1] = y;
        Some((x, y))
    }
}

/// Starts the controller thread. `name` holds the connected pad's name for the window.
/// `client` gives the current connection (it changes when the viewer reconnects; None = not connected,
/// input is dropped). Runs for the whole program.
pub fn spawn(
    client: impl Fn() -> Option<Arc<Client>> + Send + 'static,
    name: Arc<Mutex<Option<String>>>,
    recorder: Arc<crate::record::Recorder>,
) {
    std::thread::spawn(move || {
        let mut gilrs = match Gilrs::new() {
            Ok(g) => g,
            Err(e) => {
                log::warn!("game controller input unavailable: {e}");
                return;
            }
        };
        // pads show up a moment after start (as Connected events), not right away
        let update_name = |g: &Gilrs| {
            // "DualSense Edge Wireless Controller" → "DualSense Edge", to keep the bar short
            let first =
                g.gamepads().next().map(|(_, p)| p.os_name().trim_end_matches(" Wireless Controller").to_string());
            if let Some(n) = &first {
                log::info!("game controller: {n}");
            }
            *name.lock().unwrap() = first;
        };
        let mut sticks = Sticks::default();
        let mut logged = std::collections::HashSet::new();
        let mut delay = Delay::default();
        loop {
            // wakes as soon as the OS reports input; nothing else runs on this thread, so no timeout
            let Some(ev) = gilrs.next_event_blocking(None) else { continue };
            if matches!(
                ev.event,
                EventType::ButtonPressed(..) | EventType::ButtonReleased(..) | EventType::AxisChanged(..)
            ) {
                delay.add(ev.time);
            }
            match ev.event {
                // buttons gilrs has no name for: the touchpad click is one of them
                EventType::ButtonPressed(Pad::Unknown, code) | EventType::ButtonReleased(Pad::Unknown, code) => {
                    let down = matches!(ev.event, EventType::ButtonPressed(..));
                    match (unnamed(code.into_u32()), client()) {
                        (Some(b), Some(c)) => {
                            if down {
                                c.press(b);
                            } else {
                                c.release(b);
                            }
                            recorder.button(b, down);
                        }
                        (None, _) if down && logged.insert(code) => {
                            log::debug!("pad: unmapped button {code} (raw {:#x})", code.into_u32());
                        }
                        _ => {}
                    }
                }
                EventType::ButtonPressed(b, _) => {
                    if let (Some(b), Some(c)) = (map(b), client()) {
                        c.press(b);
                        recorder.button(b, true);
                    }
                }
                EventType::ButtonReleased(b, _) => {
                    if let (Some(b), Some(c)) = (map(b), client()) {
                        c.release(b);
                        recorder.button(b, false);
                    }
                }
                EventType::AxisChanged(axis, v, _) => {
                    if let Some(right) = sticks.set(axis, v)
                        && let Some((x, y)) = sticks.changed(right)
                        && let Some(c) = client()
                    {
                        c.stick(right, x, y);
                        recorder.stick(right, x, y);
                    }
                }
                EventType::Connected => update_name(&gilrs),
                EventType::Disconnected => {
                    // let go of everything the pad might have been holding
                    if let Some(c) = client() {
                        for b in Button::ALL {
                            c.release(b);
                        }
                        for right in [false, true] {
                            c.stick(right, 0.0, 0.0);
                        }
                    }
                    sticks = Sticks::default();
                    update_name(&gilrs);
                }
                _ => {}
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(target_os = "macos")]
    fn touchpad_click_reaches_the_ps5() {
        assert_eq!(unnamed(0x0009_000E), Some(Button::Touchpad));
        assert_eq!(unnamed(0x0009_0007), None, "L2's digital click stays with the analog trigger");
    }

    #[test]
    fn idle_pad_sends_nothing_and_centering_is_sent_once() {
        let mut s = Sticks::default();
        s.set(Axis::LeftStickX, 0.03);
        assert_eq!(s.changed(false), None, "drift inside the dead zone is ignored");
        s.set(Axis::LeftStickY, 1.0);
        assert_eq!(s.changed(false), Some((0.03, -1.0)), "up on the pad is negative Y on the PS5");
        assert_eq!(s.changed(false), None);
        s.set(Axis::LeftStickX, 0.0);
        s.set(Axis::LeftStickY, 0.02);
        assert_eq!(s.changed(false), Some((0.0, 0.0)));
        assert_eq!(s.changed(false), None);
    }
}
