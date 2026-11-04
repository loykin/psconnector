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

/// Sends the last few events together, so a lost packet's event is still in the next one.
pub struct EventHistory {
    events: VecDeque<[u8; 3]>,
    pub sequence: u16,
}

impl EventHistory {
    const MAX: usize = 5;

    pub fn new() -> Self {
        Self { events: VecDeque::new(), sequence: 0 }
    }

    /// Puts a new event first and returns the data to send (newest → oldest).
    pub fn push(&mut self, button: Button, pressed: bool) -> Vec<u8> {
        self.events.push_front(button.event(pressed));
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
