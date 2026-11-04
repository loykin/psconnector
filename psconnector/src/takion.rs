//! Building and parsing Takion packets. It mimics SCTP, connecting with INIT → INIT_ACK → COOKIE → COOKIE_ACK.
//!
//! Control packet layout (big-endian):
//! ```text
//! [0] header type  [1..5] peer tag  [5..9] gmac  [9..13] key_pos
//! [13] chunk type  [14] flag  [15..17] chunk length (payload + 4)  [17..] payload
//! ```

use crate::gkcrypt::StreamCipher;

pub const A_RWND: u32 = 0x019000;
pub const STREAMS: u16 = 0x64;

pub mod header {
    pub const CONTROL: u8 = 0x00;
    pub const FEEDBACK_EVENT: u8 = 0x01;
    pub const VIDEO: u8 = 0x02;
    pub const AUDIO: u8 = 0x03;
    pub const CONGESTION: u8 = 0x05;
    pub const FEEDBACK_STATE: u8 = 0x06;
}

pub mod chunk {
    pub const DATA: u8 = 0x00;
    pub const INIT: u8 = 0x01;
    pub const INIT_ACK: u8 = 0x02;
    pub const DATA_ACK: u8 = 0x03;
    pub const COOKIE: u8 = 0x0A;
    pub const COOKIE_ACK: u8 = 0x0B;
}

/// A received control packet.
#[derive(Debug)]
pub struct Control<'a> {
    pub chunk_type: u8,
    pub payload: &'a [u8],
}

/// The AV type if this is a video or audio packet, otherwise None.
pub fn av_type(packet: &[u8]) -> Option<u8> {
    match packet.first().map(|b| b & 0x0F) {
        Some(t @ (header::VIDEO | header::AUDIO)) => Some(t),
        _ => None,
    }
}

pub fn parse_control(packet: &[u8]) -> Option<Control<'_>> {
    if packet.len() < 17 || packet[0] != header::CONTROL {
        return None;
    }
    Some(Control { chunk_type: packet[13], payload: &packet[17..] })
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(b[at..at + 4].try_into().unwrap())
}

/// INIT_ACK payload → (peer tag, cookie)
pub fn parse_init_ack(payload: &[u8]) -> Option<(u32, &[u8])> {
    (payload.len() >= 16).then(|| (u32_at(payload, 0), &payload[16..]))
}

/// DATA payload → (tsn, channel, data)
pub fn parse_data(payload: &[u8]) -> Option<(u32, u16, &[u8])> {
    (payload.len() >= 9).then(|| (u32_at(payload, 0), u16::from_be_bytes([payload[4], payload[5]]), &payload[9..]))
}

/// Builds a control packet. With a cipher, fills in gmac and key_pos and advances key_pos by advance_by
/// (or the payload length when 0). Control payloads are authenticated, not encrypted.
pub fn control(
    chunk_type: u8,
    flag: u8,
    tag_remote: u32,
    payload: &[u8],
    cipher: Option<&mut StreamCipher>,
    advance_by: usize,
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(17 + payload.len());
    buf.push(header::CONTROL);
    buf.extend_from_slice(&tag_remote.to_be_bytes());
    buf.extend_from_slice(&[0; 8]); // gmac, key_pos (both zero while computing the gmac)
    buf.push(chunk_type);
    buf.push(flag);
    buf.extend_from_slice(&((payload.len() + 4) as u16).to_be_bytes());
    buf.extend_from_slice(payload);
    if let Some(c) = cipher {
        let key_pos = c.key_pos;
        let tag = c.local.gmac(&buf, key_pos);
        buf[5..9].copy_from_slice(&tag);
        buf[9..13].copy_from_slice(&(key_pos as u32).to_be_bytes());
        c.key_pos += if advance_by == 0 { payload.len() } else { advance_by } as u64;
    }
    buf
}

pub fn init_payload(tag_local: u32, tsn: u32) -> Vec<u8> {
    let mut p = Vec::with_capacity(16);
    p.extend_from_slice(&tag_local.to_be_bytes());
    p.extend_from_slice(&A_RWND.to_be_bytes());
    p.extend_from_slice(&STREAMS.to_be_bytes());
    p.extend_from_slice(&STREAMS.to_be_bytes());
    p.extend_from_slice(&tsn.to_be_bytes());
    p
}

pub fn data_payload(tsn: u32, channel: u16, data: &[u8]) -> Vec<u8> {
    let mut p = Vec::with_capacity(9 + data.len());
    p.extend_from_slice(&tsn.to_be_bytes());
    p.extend_from_slice(&channel.to_be_bytes());
    p.extend_from_slice(&[0; 3]);
    p.extend_from_slice(data);
    p
}

pub fn data_ack_payload(tsn: u32) -> Vec<u8> {
    let mut p = Vec::with_capacity(12);
    p.extend_from_slice(&tsn.to_be_bytes());
    p.extend_from_slice(&A_RWND.to_be_bytes());
    p.extend_from_slice(&[0; 4]); // gap ack block count, duplicate tsn count
    p
}

/// Controller feedback packet (EVENT = buttons, STATE = sticks). The data is encrypted.
/// Layout: `[0] type [1..3] sequence [3] 0 [4..8] key_pos [8..12] gmac [12..] data`
/// The gmac is computed with key_pos filled in and only the gmac field zeroed.
pub fn feedback(kind: u8, sequence: u16, data: &[u8], c: &mut StreamCipher) -> Vec<u8> {
    let key_pos = c.key_pos;
    let mut buf = Vec::with_capacity(12 + data.len());
    buf.push(kind);
    buf.extend_from_slice(&sequence.to_be_bytes());
    buf.push(0);
    buf.extend_from_slice(&(key_pos as u32).to_be_bytes());
    buf.extend_from_slice(&[0; 4]);
    let mut enc = data.to_vec();
    c.local.xor(key_pos, &mut enc);
    buf.extend_from_slice(&enc);
    let tag = c.local.gmac(&buf, key_pos);
    buf[8..12].copy_from_slice(&tag);
    c.key_pos += data.len() as u64;
    buf
}

/// Congestion report (packets received, packets lost).
/// Layout: `[0] 5 [1..3] 0 [3..5] received [5..7] lost [7..11] gmac [11..15] key_pos`
pub fn congestion(received: u16, lost: u16, c: &mut StreamCipher) -> Vec<u8> {
    let key_pos = c.key_pos;
    let mut buf = vec![header::CONGESTION, 0, 0];
    buf.extend_from_slice(&received.to_be_bytes());
    buf.extend_from_slice(&lost.to_be_bytes());
    buf.extend_from_slice(&[0; 4]);
    buf.extend_from_slice(&(key_pos as u32).to_be_bytes());
    let tag = c.local.gmac(&buf, key_pos);
    buf[7..11].copy_from_slice(&tag);
    c.key_pos += buf.len() as u64;
    buf
}
