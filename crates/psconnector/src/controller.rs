//! Controller input: turns button events and stick state into PS5 feedback data.
//!
//! Only the stream task sends (`stream.rs`). This module only builds the data.

use std::collections::VecDeque;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Button {
    Up,
    Down,
    Left,
    Right,
    L1,
    R1,
    L2,
    R2,
    Cross,
    Circle,
    Square,
    Triangle,
    Options,
    Share,
    Ps,
    L3,
    R3,
    Touchpad,
}

impl Button {
    pub const ALL: [Button; 18] = [
        Button::Up,
        Button::Down,
        Button::Left,
        Button::Right,
        Button::L1,
        Button::R1,
        Button::L2,
        Button::R2,
        Button::Cross,
        Button::Circle,
        Button::Square,
        Button::Triangle,
        Button::Options,
        Button::Share,
        Button::Ps,
        Button::L3,
        Button::R3,
        Button::Touchpad,
    ];

    fn id(self) -> u8 {
        0x80 + self as u8
    }

    pub fn name(self) -> &'static str {
        match self {
            Button::Up => "up",
            Button::Down => "down",
            Button::Left => "left",
            Button::Right => "right",
            Button::L1 => "l1",
            Button::R1 => "r1",
            Button::L2 => "l2",
            Button::R2 => "r2",
            Button::Cross => "cross",
            Button::Circle => "circle",
            Button::Square => "square",
            Button::Triangle => "triangle",
            Button::Options => "options",
            Button::Share => "share",
            Button::Ps => "ps",
            Button::L3 => "l3",
            Button::R3 => "r3",
            Button::Touchpad => "touchpad",
        }
    }

    pub fn parse(s: &str) -> Option<Button> {
        let s = s.to_ascii_lowercase();
        Button::ALL.into_iter().find(|b| b.name() == s)
    }

    /// 3-byte event: `0x80, button id, state`. From options onward the id is 0x20 higher when pressed.
    fn event(self, pressed: bool) -> [u8; 3] {
        let mut id = self.id();
        if id >= 0x8C && pressed {
            id += 0x20;
        }
        [0x80, id, if pressed { 0xFF } else { 0x00 }]
    }
}

/// Width and height of the touchpad in the coordinates touch events use (the DualSense's own).
pub const TOUCHPAD_SIZE: (u16, u16) = (1920, 1080);

/// 5-byte touch event: `0xD0` (finger down or moved) / `0xC0` (lifted), the finger's id (7 bits), then x and
/// y as 12 bits each. The same layout chiaki uses.
pub fn touch_event(id: u8, down: bool, x: u16, y: u16) -> [u8; 5] {
    let (x, y) = (x.min(0xFFF), y.min(0xFFF));
    [if down { 0xD0 } else { 0xC0 }, id & 0x7F, (x >> 4) as u8, (((x & 0xF) << 4) as u8) | (y >> 8) as u8, y as u8]
}

/// Sends the last few events together, so a lost packet's event is still in the next one.
pub struct EventHistory {
    events: VecDeque<Vec<u8>>,
    pub sequence: u16,
}

impl EventHistory {
    const MAX: usize = 5;

    pub fn new() -> Self {
        Self { events: VecDeque::new(), sequence: 0 }
    }

    /// Puts a new event first and returns the data to send (newest → oldest).
    pub fn push(&mut self, button: Button, pressed: bool) -> Vec<u8> {
        self.push_raw(button.event(pressed).to_vec())
    }

    /// Puts any encoded event first (touch events are 5 bytes) and returns the data to send.
    pub fn push_raw(&mut self, event: Vec<u8>) -> Vec<u8> {
        self.events.push_front(event);
        self.events.truncate(Self::MAX);
        self.data()
    }

    pub fn data(&self) -> Vec<u8> {
        self.events.iter().flatten().copied().collect()
    }
}

impl Default for EventHistory {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Sticks {
    pub left: (f32, f32),
    pub right: (f32, f32),
}

/// 17 bytes of idle motion sensor data (same as pyremoteplay).
const MOTION_IDLE: [u8; 17] =
    [0xA0, 0xFF, 0x7F, 0xFF, 0x7F, 0xFF, 0x7F, 0xFF, 0x7F, 0x99, 0x99, 0xFF, 0x7F, 0xFE, 0xF7, 0xEF, 0x1F];

fn axis(v: f32) -> i16 {
    (v.clamp(-1.0, 1.0) * 0x7FFF as f32) as i16
}

impl Sticks {
    /// 28-byte PS5 state: 17 motion + 4 sticks (i16, big-endian) + 0 0 + 1 (report as DS4).
    pub fn state_data(&self) -> Vec<u8> {
        let mut buf = MOTION_IDLE.to_vec();
        for v in [self.left.0, self.left.1, self.right.0, self.right.1] {
            buf.extend_from_slice(&axis(v).to_be_bytes());
        }
        buf.extend_from_slice(&[0, 0, 1]);
        buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn touch_events_pack_x_and_y_into_twelve_bits() {
        assert_eq!(touch_event(3, true, 0x123, 0x456), [0xD0, 3, 0x12, 0x34, 0x56]);
        assert_eq!(touch_event(0x85, false, 1919, 1079), [0xC0, 0x05, 0x77, 0xF4, 0x37]);
        let mut h = EventHistory::new();
        h.push(Button::Cross, true);
        let data = h.push_raw(touch_event(1, true, 0, 0).to_vec());
        assert_eq!(data.len(), 8, "newest (touch) first, then the button");
        assert_eq!(data[0], 0xD0);
    }
}
