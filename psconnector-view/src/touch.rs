//! Fingers on a DualSense's touchpad drive the PS5's touchpad (swipes, the map in some games). gilrs only
//! reports the touchpad click, so the finger positions are read from the pad's own HID reports on a
//! thread of their own (shared, so gilrs keeps reading the same pad).
//!
//! DualSense input report: USB report 0x01 and Bluetooth report 0x31 carry the same layout (from byte 1 and
//! byte 2). Two touch points start 32 bytes in, 4 bytes each: bit 7 of the first byte is set while no finger
//! is down, its low 7 bits are the finger's id; then x and y as 12 bits each (1920 × 1080).

use std::sync::Arc;
use std::time::{Duration, Instant};

use psconnector::Client;

const SONY: u16 = 0x054C;
/// DualSense and DualSense Edge
const PADS: [u16; 2] = [0x0CE6, 0x0DF2];
/// Moves are sent at most this often per finger (the pad reports up to 250 times a second).
const MOVE_EVERY: Duration = Duration::from_millis(8);

/// One finger as the pad reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Finger {
    pub(crate) id: u8,
    pub(crate) x: u16,
    pub(crate) y: u16,
}

/// The two touch points in one input report, or `None` for a report without them.
pub(crate) fn parse(report: &[u8]) -> Option<[Option<Finger>; 2]> {
    let base = match report.first()? {
        0x01 if report.len() >= 64 => 1, // USB
        0x31 if report.len() >= 78 => 2, // Bluetooth, full report
        _ => return None,                // Bluetooth simplified report: no touch data
    };
    let point = |i: usize| {
        let p = report.get(base + 32 + i * 4..base + 36 + i * 4)?;
        (p[0] & 0x80 == 0).then(|| Finger {
            id: p[0] & 0x7F,
            x: u16::from(p[1]) | (u16::from(p[2] & 0x0F) << 8),
            y: u16::from(p[2] >> 4) | (u16::from(p[3]) << 4),
        })
    };
    Some([point(0), point(1)])
}

/// What changed between two reports, as touch events to send: (id, down, x, y).
#[derive(Default)]
pub(crate) struct Tracker {
    /// where each finger was last seen
    fingers: [Option<Finger>; 2],
    /// what was last sent for it, and when
    sent: [Option<(Finger, Instant)>; 2],
}

impl Tracker {
    pub(crate) fn update(&mut self, now: [Option<Finger>; 2], at: Instant) -> Vec<(u8, bool, u16, u16)> {
        let mut out = Vec::new();
        for (i, now) in now.into_iter().enumerate() {
            let before = self.fingers[i];
            self.fingers[i] = now;
            match (before, now) {
                (Some(b), Some(n)) if b.id == n.id => {
                    // a move: sent when it moved since the last send, but not more often than MOVE_EVERY
                    let Some((sent, sent_at)) = self.sent[i] else { continue };
                    if (sent.x, sent.y) != (n.x, n.y) && at.duration_since(sent_at) >= MOVE_EVERY {
                        out.push((n.id, true, n.x, n.y));
                        self.sent[i] = Some((n, at));
                    }
                }
                (b, n) => {
                    if let Some(b) = b {
                        // the lift happens where the finger really was, even if that move was held back
                        if self.sent[i].is_some_and(|(s, _)| (s.x, s.y) != (b.x, b.y)) {
                            out.push((b.id, true, b.x, b.y));
                        }
                        out.push((b.id, false, b.x, b.y));
                        self.sent[i] = None;
                    }
                    if let Some(n) = n {
                        out.push((n.id, true, n.x, n.y));
                        self.sent[i] = Some((n, at));
                    }
                }
            }
        }
        out
    }

    /// Lifts every finger still down (the pad went away).
    pub(crate) fn lift_all(&mut self) -> Vec<(u8, bool, u16, u16)> {
        self.update([None, None], Instant::now())
    }
}

/// Starts the touchpad thread. Runs for the whole program; a pad that is connected later is picked up.
pub fn spawn(client: impl Fn() -> Option<Arc<Client>> + Send + 'static, recorder: Arc<crate::record::Recorder>) {
    std::thread::Builder::new()
        .name("touchpad".into())
        .spawn(move || {
            let mut api = match hidapi::HidApi::new() {
                Ok(a) => a,
                Err(e) => {
                    log::warn!("touchpad input unavailable: {e}");
                    return;
                }
            };
            #[cfg(target_os = "macos")]
            api.set_open_exclusive(false);
            loop {
                let dev = api.refresh_devices().ok().and_then(|_| {
                    let info = api.device_list().find(|d| d.vendor_id() == SONY && PADS.contains(&d.product_id()))?;
                    info.open_device(&api).ok()
                });
                let Some(dev) = dev else {
                    std::thread::sleep(Duration::from_secs(2));
                    continue;
                };
                // Over Bluetooth the pad sends a short report until its calibration is read once
                let mut feature = [0u8; 41];
                feature[0] = 0x05;
                let _ = dev.get_feature_report(&mut feature);
                log::info!("touchpad: reading finger positions");
                let mut tracker = Tracker::default();
                let mut buf = [0u8; 128];
                let (mut seen, mut touched) = (false, false);
                let send = |events: Vec<(u8, bool, u16, u16)>| {
                    if events.is_empty() {
                        return;
                    }
                    let c = client();
                    for (id, down, x, y) in events {
                        if let Some(c) = &c {
                            c.touch(id, down, x, y);
                        }
                        recorder.touch(id, down, x, y);
                    }
                };
                loop {
                    match dev.read_timeout(&mut buf, 500) {
                        Ok(0) => continue,
                        Ok(n) => {
                            let parsed = parse(&buf[..n]);
                            if !seen {
                                seen = true;
                                let kind = if parsed.is_some() { "with touch data" } else { "without touch data" };
                                log::info!("touchpad: report {:#04x}, {n} bytes, {kind}", buf[0]);
                            }
                            if let Some(points) = parsed {
                                let events = tracker.update(points, Instant::now());
                                if !events.is_empty() && !touched {
                                    touched = true;
                                    log::info!("touchpad: first finger {:?}", events[0]);
                                }
                                send(events);
                            }
                        }
                        Err(e) => {
                            log::info!("touchpad: pad gone ({e})");
                            send(tracker.lift_all());
                            break;
                        }
                    }
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        })
        .expect("touchpad thread");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usb_report(points: [Option<(u8, u16, u16)>; 2]) -> Vec<u8> {
        let mut r = vec![0u8; 64];
        r[0] = 0x01;
        for (i, p) in points.into_iter().enumerate() {
            let o = 1 + 32 + i * 4;
            match p {
                Some((id, x, y)) => {
                    r[o] = id & 0x7F;
                    r[o + 1] = x as u8;
                    r[o + 2] = ((x >> 8) as u8 & 0x0F) | ((y as u8 & 0x0F) << 4);
                    r[o + 3] = (y >> 4) as u8;
                }
                None => r[o] = 0x80,
            }
        }
        r
    }

    #[test]
    fn reads_both_fingers_from_usb_and_bluetooth_reports() {
        let r = usb_report([Some((5, 1919, 1079)), None]);
        assert_eq!(parse(&r), Some([Some(Finger { id: 5, x: 1919, y: 1079 }), None]));
        let mut bt = vec![0x31, 0x00];
        bt.extend_from_slice(&r[1..]);
        bt.resize(78, 0);
        assert_eq!(parse(&bt), parse(&r));
        assert_eq!(parse(&[0x01, 0x80, 0x80, 0x80, 0x80, 0, 0, 0, 0, 0]), None, "short Bluetooth report");
    }

    #[test]
    fn a_swipe_is_down_moves_and_up() {
        let mut t = Tracker::default();
        let t0 = Instant::now();
        let f = |x| [Some(Finger { id: 7, x, y: 500 }), None];
        assert_eq!(t.update(f(100), t0), vec![(7, true, 100, 500)]);
        assert_eq!(t.update(f(100), t0 + Duration::from_millis(20)), vec![], "no move, nothing sent");
        assert_eq!(t.update(f(150), t0 + Duration::from_millis(40)), vec![(7, true, 150, 500)]);
        assert_eq!(t.update(f(160), t0 + Duration::from_millis(42)), vec![], "moves are rate limited");
        assert_eq!(
            t.update([None, None], t0 + Duration::from_millis(44)),
            vec![(7, true, 160, 500), (7, false, 160, 500)],
            "the held-back last position is sent before the lift"
        );
        // a new finger right away in the same slot: the old one lifts first
        t.update(f(10), t0 + Duration::from_millis(50));
        let next = [Some(Finger { id: 8, x: 20, y: 30 }), None];
        assert_eq!(t.update(next, t0 + Duration::from_millis(51)), vec![(7, false, 10, 500), (8, true, 20, 30)]);
        assert_eq!(t.lift_all(), vec![(8, false, 20, 30)]);
    }
}
