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
    /// Units in a row that looked older than the last finished frame
    stale_run: u32,
}

/// That many "old" units in a row means the console restarted its frame numbering (it does so when it
/// changes the stream, e.g. after adapting to the network): start following the new numbers.
const RESYNC_AFTER: u32 = 120;

pub enum Assembled {
    /// A complete H.264 frame
    Frame(u16, Vec<u8>),
    /// Frames in start..=end were lost. A key frame should be requested.
    Lost(u16, u16),
}

impl Assembler {
    pub fn new(header: Vec<u8>) -> Self {
        Self { header, pending: BTreeMap::new(), last_done: None, stale_run: 0 }
    }

    /// Adds one decrypted unit.
    pub fn push(&mut self, frame: u16, unit: u16, src: u16, data: Vec<u8>, out: &mut Vec<Assembled>) {
        if unit >= src {
            return; // FEC units are not used yet
        }
        if let Some(done) = self.last_done {
            // already finished or too old (u16 wrap-around aware)
            if frame.wrapping_sub(done) == 0 || frame.wrapping_sub(done) > 0x8000 {
                self.stale_run += 1;
                if self.stale_run < RESYNC_AFTER {
                    return;
                }
                log::info!("video frame numbers restarted ({done} → {frame}); following them");
                self.last_done = None;
                self.pending.clear();
            }
        }
        self.stale_run = 0;
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

/// Decoding speed, logged every 10 s.
#[derive(Default)]
struct DecodeStats {
    since: Option<std::time::Instant>,
    decoded: u32,
    shown: u32,
    skipped: u32,
    queue: std::time::Duration,
    max_queue: std::time::Duration,
    decode: std::time::Duration,
    convert: std::time::Duration,
}

impl DecodeStats {
    fn report(&mut self) {
        let since = *self.since.get_or_insert_with(std::time::Instant::now);
        if since.elapsed() < std::time::Duration::from_secs(10) {
            return;
        }
        let ms = |d: std::time::Duration, n: u32| d.as_secs_f64() * 1000.0 / f64::from(n.max(1));
        log::info!(
            "video: {} frames decoded ({:.1} ms each, queue {:.1} ms avg/{:.1} ms max), {} shown ({:.1} ms to RGB), {} skipped to catch up",
            self.decoded,
            ms(self.decode, self.decoded),
            ms(self.queue, self.decoded),
            self.max_queue.as_secs_f64() * 1000.0,
            self.shown,
            ms(self.convert, self.shown),
            self.skipped
        );
        *self = Self { since: Some(std::time::Instant::now()), ..Self::default() };
    }
}

/// Decoder thread. Decodes incoming frames and publishes the latest picture on a watch channel.
/// Frames are decoded in order even when behind (H.264 needs the earlier frames).
///
/// If the decoder keeps failing (it can get stuck after a damaged frame), it asks the stream for a key frame
/// through `need_key`, and after a while starts a fresh decoder.
pub fn spawn_decoder(
    rx: std::sync::mpsc::Receiver<(u16, std::time::Instant, Vec<u8>)>,
    out: watch::Sender<Option<Arc<Frame>>>,
    need_key: Arc<std::sync::atomic::AtomicBool>,
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
            // Frames returned by the watch channel can be reused once the UI has released its Arc. Keeping a
            // few candidates avoids both the 6 MB rgb.clone() and a fresh allocation for every 1080p frame.
            let mut reusable = Vec::new();
            let mut stats = DecodeStats::default();
            // frames in a row that gave no picture
            let mut failing = 0u32;
            let mut next = rx.recv().ok();
            while let Some((index, queued_at, h264)) = next.take() {
                let queued = queued_at.elapsed();
                let t0 = std::time::Instant::now();
                let decoded = dec.decode(&h264);
                let t_decode = t0.elapsed();
                // when more frames are already waiting, only decode this one (later frames need it) and skip
                // turning it into a picture nobody will see; this keeps the screen from falling behind
                let waiting = rx.try_recv();
                let behind = waiting.is_ok();
                match &decoded {
                    Ok(Some(_)) => failing = 0,
                    _ => failing += 1,
                }
                if failing > 0 && failing.is_multiple_of(10) {
                    need_key.store(true, std::sync::atomic::Ordering::SeqCst);
                }
                match decoded {
                    Ok(Some(yuv)) if !behind => {
                        let t1 = std::time::Instant::now();
                        let (w, h) = yuv.dimensions();
                        if rgb.capacity() < w * h * 3
                            && let Some(at) = reusable.iter().position(|f: &Arc<Frame>| Arc::strong_count(f) == 1)
                            && let Ok(frame) = Arc::try_unwrap(reusable.swap_remove(at))
                        {
                            rgb = frame.rgb;
                        }
                        rgb.resize(w * h * 3, 0);
                        yuv.write_rgb8(&mut rgb);
                        let frame = Arc::new(Frame { width: w, height: h, rgb: std::mem::take(&mut rgb), index });
                        if let Some(old) = out.send_replace(Some(frame)) {
                            reusable.push(old);
                            if reusable.len() > 8 {
                                reusable.remove(0);
                            }
                        }
                        stats.shown += 1;
                        stats.convert += t1.elapsed();
                    }
                    Ok(_) => stats.skipped += u32::from(behind),
                    Err(e) => debug!("failed to decode frame {index}: {e}"),
                }
                stats.decoded += 1;
                stats.queue += queued;
                stats.max_queue = stats.max_queue.max(queued);
                stats.decode += t_decode;
                stats.report();
                if failing >= 60 {
                    log::info!("video decoder stuck after a damaged frame; starting a fresh one");
                    match Decoder::new() {
                        Ok(d) => dec = d,
                        Err(e) => warn!("cannot restart the H.264 decoder: {e}"),
                    }
                    failing = 0;
                }
                next = match waiting {
                    Ok(f) => Some(f),
                    Err(std::sync::mpsc::TryRecvError::Empty) => rx.recv().ok(),
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => None,
                };
            }
        })
        .expect("video thread")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(a: &mut Assembler, frame: u16) -> usize {
        let mut out = Vec::new();
        a.push(frame, 0, 1, vec![0, 0, 9], &mut out);
        out.iter().filter(|x| matches!(x, Assembled::Frame(..))).count()
    }

    #[test]
    fn follows_restarted_frame_numbers() {
        let mut a = Assembler::new(vec![]);
        for f in 5000..5010 {
            assert_eq!(unit(&mut a, f), 1);
        }
        // the console starts over at 0: ignored at first, then followed
        let got: usize = (0..200).map(|f| unit(&mut a, f)).sum();
        assert!(got >= 70, "frames after the restart: {got}");
        // a stray old unit afterwards is still ignored
        assert_eq!(unit(&mut a, 150), 0);
    }
}
