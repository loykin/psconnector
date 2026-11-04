//! Video: Takion video packets → H.264 frames → RGB.
//!
//! Video packet header (big-endian):
//! ```text
//! [0] low 4 bits = type (2 = video), bit 4 = has NALU info
//! [1..3] packet index in the stream   [3..5] frame index
//! [5..9] unit info (bit 21+ unit index, bit 10+ total units - 1, bit 0+ FEC unit count)
//! [14..18] key_pos   [18..] data (video skips 3 bytes, plus 3 more when NALU info is present)
//! ```
//! Data is decrypted with the receive key stream. Dropping the first 2 bytes of each source unit, joining
//! them, and prepending the video header (SPS/PPS) from STREAMINFO gives an Annex-B H.264 frame.
//! FEC units are not used yet: frames with missing units are dropped and a key frame is requested.

use std::collections::BTreeMap;
use std::sync::Arc;

use log::{debug, warn};
use openh264::decoder::Decoder;
use openh264::formats::YUVSource;
use tokio::sync::watch;

/// One decoded frame (8-bit RGB, row-major).
#[derive(Clone)]
pub struct Frame {
    pub width: usize,
    pub height: usize,
    pub rgb: Vec<u8>,
    /// Frame index assigned by the console
    pub index: u16,
}

pub struct VideoPacket<'a> {
    pub frame: u16,
    pub unit: u16,
    pub total: u16,
    pub fec: u16,
    pub key_pos: u64,
    pub data: &'a [u8],
}

impl VideoPacket<'_> {
    pub fn src_units(&self) -> u16 {
        self.total - self.fec
    }
}

pub fn parse(buf: &[u8]) -> Option<VideoPacket<'_>> {
    if buf.len() < 21 || buf[0] & 0x0F != 0x02 {
        return None;
    }
    let has_nalu = (buf[0] >> 4) & 1 != 0;
    let frame = u16::from_be_bytes([buf[3], buf[4]]);
    let dword2 = u32::from_be_bytes(buf[5..9].try_into().unwrap());
    let key_pos = u32::from_be_bytes(buf[14..18].try_into().unwrap()) as u64;
    let offset = 18 + 3 + if has_nalu { 3 } else { 0 };
    let total = (((dword2 >> 10) & 0x7FF) + 1) as u16;
    let fec = (dword2 & 0x3FF) as u16;
    (buf.len() >= offset && fec < total).then(|| VideoPacket {
        frame,
        unit: ((dword2 >> 21) & 0x7FF) as u16,
        total,
        fec,
        key_pos,
        data: &buf[offset..],
    })
}

struct Pending {
    src: u16,
    units: Vec<Option<Vec<u8>>>,
    have: u16,
}

/// Collects units into frames. Units may arrive out of order.
pub struct Assembler {
    header: Vec<u8>,
    pending: BTreeMap<u16, Pending>,
    last_done: Option<u16>,
}

pub enum Assembled {
    /// A complete H.264 frame
    Frame(u16, Vec<u8>),
    /// Frames in start..=end were lost. A key frame should be requested.
    Lost(u16, u16),
}

impl Assembler {
    pub fn new(header: Vec<u8>) -> Self {
        Self { header, pending: BTreeMap::new(), last_done: None }
    }

    /// Adds one decrypted unit.
    pub fn push(&mut self, frame: u16, unit: u16, src: u16, data: Vec<u8>, out: &mut Vec<Assembled>) {
        if unit >= src {
            return; // FEC units are not used yet
        }
        if let Some(done) = self.last_done {
            // already finished or too old (u16 wrap-around aware)
            if frame.wrapping_sub(done) == 0 || frame.wrapping_sub(done) > 0x8000 {
                return;
            }
        }
        let p = self.pending.entry(frame).or_insert_with(|| Pending { src, units: vec![None; src as usize], have: 0 });
        let slot = &mut p.units[unit as usize];
        if slot.is_none() {
            *slot = Some(data);
            p.have += 1;
        }
        if p.have < p.src {
            return;
        }
        let p = self.pending.remove(&frame).unwrap();
        // unfinished frames older than this one are treated as lost
        let older: Vec<u16> = self.pending.keys().copied().filter(|&f| frame.wrapping_sub(f) < 0x8000).collect();
        for f in &older {
            self.pending.remove(f);
        }
        let expected = self.last_done.map(|d| d.wrapping_add(1)).unwrap_or(frame);
        if expected != frame {
            out.push(Assembled::Lost(expected, frame.wrapping_sub(1)));
        }
        self.last_done = Some(frame);
        let mut h264 = self.header.clone();
        for unit in p.units.into_iter().flatten() {
            // the first 2 bytes of a unit are the padding size (unit size minus data size)
            h264.extend_from_slice(unit.get(2..).unwrap_or_default());
        }
        out.push(Assembled::Frame(frame, h264));
    }
}

/// Decoder thread. Decodes incoming frames and publishes the latest picture on a watch channel.
/// Frames are decoded in order even when behind (H.264 needs the earlier frames).
pub fn spawn_decoder(
    rx: std::sync::mpsc::Receiver<(u16, Vec<u8>)>,
    out: watch::Sender<Option<Arc<Frame>>>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("psconnector-video".into())
        .spawn(move || {
            let mut dec = match Decoder::new() {
                Ok(d) => d,
                Err(e) => {
                    warn!("cannot create the H.264 decoder: {e}");
                    return;
                }
            };
            let mut rgb = Vec::new();
            while let Ok((index, h264)) = rx.recv() {
                match dec.decode(&h264) {
                    Ok(Some(yuv)) => {
                        let (w, h) = yuv.dimensions();
                        rgb.resize(w * h * 3, 0);
                        yuv.write_rgb8(&mut rgb);
                        let _ = out.send(Some(Arc::new(Frame { width: w, height: h, rgb: rgb.clone(), index })));
                    }
                    Ok(None) => {}
                    Err(e) => debug!("failed to decode frame {index}: {e}"),
                }
            }
        })
        .expect("video thread")
}
