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
use rayon::prelude::*;
use tokio::sync::watch;

use crate::VideoCodec;

/// One decoded frame (8-bit RGB, row-major).
#[derive(Clone)]
pub struct Frame {
    pub width: usize,
    pub height: usize,
    pub rgb: Vec<u8>,
    /// Frame index assigned by the console
    pub index: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Matrix {
    Bt601,
    Bt709,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ColorSpec {
    full_range: bool,
    matrix: Matrix,
    primaries: u8,
    transfer: u8,
    matrix_code: u8,
}

/// How decoded YUV sample levels are mapped to RGB.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum VideoRange {
    /// Follow the H.264 VUI flag.
    #[default]
    Auto,
    Limited,
    Full,
}

impl VideoRange {
    fn from_atomic(value: u8) -> Self {
        match value {
            1 => Self::Limited,
            2 => Self::Full,
            _ => Self::Auto,
        }
    }

    pub(crate) fn atomic(self) -> u8 {
        match self {
            Self::Auto => 0,
            Self::Limited => 1,
            Self::Full => 2,
        }
    }
}

impl Default for ColorSpec {
    fn default() -> Self {
        Self { full_range: false, matrix: Matrix::Bt709, primaries: 1, transfer: 1, matrix_code: 1 }
    }
}

struct Bits {
    data: Vec<u8>,
    at: usize,
}

impl Bits {
    fn from_ebsp(data: &[u8]) -> Self {
        let mut rbsp = Vec::with_capacity(data.len());
        let mut zeros = 0;
        for &byte in data {
            if zeros >= 2 && byte == 3 {
                zeros = 0;
                continue;
            }
            rbsp.push(byte);
            zeros = if byte == 0 { zeros + 1 } else { 0 };
        }
        Self { data: rbsp, at: 0 }
    }

    fn bit(&mut self) -> Option<u32> {
        let byte = *self.data.get(self.at / 8)?;
        let value = u32::from((byte >> (7 - self.at % 8)) & 1);
        self.at += 1;
        Some(value)
    }

    fn bits(&mut self, count: usize) -> Option<u32> {
        let mut value = 0;
        for _ in 0..count {
            value = (value << 1) | self.bit()?;
        }
        Some(value)
    }

    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        Some((1u32 << zeros) - 1 + if zeros == 0 { 0 } else { self.bits(zeros)? })
    }

    fn se(&mut self) -> Option<i32> {
        let value = self.ue()?;
        Some(if value & 1 == 0 { -((value / 2) as i32) } else { value.div_ceil(2) as i32 })
    }

    fn scaling_list(&mut self, size: usize) -> Option<()> {
        let (mut last, mut next) = (8i32, 8i32);
        for _ in 0..size {
            if next != 0 {
                next = (last + self.se()? + 256) % 256;
            }
            if next != 0 {
                last = next;
            }
        }
        Some(())
    }
}

fn start_code(data: &[u8], from: usize) -> Option<(usize, usize)> {
    for at in from..data.len().saturating_sub(2) {
        if data[at..].starts_with(&[0, 0, 1]) {
            return Some((at, at + 3));
        }
        if data[at..].starts_with(&[0, 0, 0, 1]) {
            return Some((at, at + 4));
        }
    }
    None
}

impl ColorSpec {
    fn from_h264(data: &[u8]) -> Option<Self> {
        let mut search = 0;
        while let Some((_prefix, nal)) = start_code(data, search) {
            let next = start_code(data, nal + 1).map_or(data.len(), |(prefix, _)| prefix);
            if data.get(nal)? & 0x1f == 7 {
                return Self::from_sps(data.get(nal + 1..next)?);
            }
            search = next;
        }
        None
    }

    fn from_sps(sps: &[u8]) -> Option<Self> {
        let mut b = Bits::from_ebsp(sps);
        let profile = b.bits(8)? as u8;
        b.bits(8)?; // constraint flags
        b.bits(8)?; // level
        b.ue()?; // SPS id
        if matches!(profile, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135) {
            let chroma = b.ue()?;
            if chroma == 3 {
                b.bit()?;
            }
            b.ue()?; // bit depth luma
            b.ue()?; // bit depth chroma
            b.bit()?;
            if b.bit()? != 0 {
                let lists = if chroma == 3 { 12 } else { 8 };
                for index in 0..lists {
                    if b.bit()? != 0 {
                        b.scaling_list(if index < 6 { 16 } else { 64 })?;
                    }
                }
            }
        }
        b.ue()?; // log2_max_frame_num_minus4
        match b.ue()? {
            0 => {
                b.ue()?;
            }
            1 => {
                b.bit()?;
                b.se()?;
                b.se()?;
                for _ in 0..b.ue()? {
                    b.se()?;
                }
            }
            _ => {}
        }
        b.ue()?; // max refs
        b.bit()?;
        b.ue()?; // width
        b.ue()?; // height
        let frame_mbs_only = b.bit()?;
        if frame_mbs_only == 0 {
            b.bit()?;
        }
        b.bit()?;
        if b.bit()? != 0 {
            for _ in 0..4 {
                b.ue()?;
            }
        }
        if b.bit()? == 0 {
            return None;
        }
        if b.bit()? != 0 {
            let aspect = b.bits(8)?;
            if aspect == 255 {
                b.bits(16)?;
                b.bits(16)?;
            }
        }
        if b.bit()? != 0 {
            b.bit()?;
        }
        if b.bit()? == 0 {
            return None;
        }
        b.bits(3)?; // video format
        let full_range = b.bit()? != 0;
        if b.bit()? == 0 {
            return Some(Self { full_range, ..Self::default() });
        }
        let primaries = b.bits(8)? as u8;
        let transfer = b.bits(8)? as u8;
        let matrix_code = b.bits(8)? as u8;
        let matrix = if matrix_code == 1 { Matrix::Bt709 } else { Matrix::Bt601 };
        Some(Self { full_range, matrix, primaries, transfer, matrix_code })
    }
}

fn write_rgb8(yuv: &(impl YUVSource + Sync), spec: ColorSpec, target: &mut [u8]) {
    let (width, height) = yuv.dimensions();
    let (ys, us, vs) = yuv.strides();
    assert_eq!(target.len(), width * height * 3);
    let (ym, rv, gu, gv, bu) = match (spec.full_range, spec.matrix) {
        (false, Matrix::Bt709) => (298, 459, -55, -136, 541),
        (false, Matrix::Bt601) => (298, 409, -100, -208, 516),
        (true, Matrix::Bt709) => (256, 403, -48, -120, 475),
        (true, Matrix::Bt601) => (256, 359, -88, -183, 454),
    };
    let y_offset = if spec.full_range { 0 } else { 16 };
    let rows_per_job = 16;
    target.par_chunks_mut(width * 3 * rows_per_job).enumerate().for_each(|(job, output)| {
        let first_row = job * rows_per_job;
        for (local_row, row) in output.chunks_mut(width * 3).enumerate() {
            let y = first_row + local_row;
            if y >= height {
                break;
            }
            for x in 0..width {
                let c = i32::from(yuv.y()[y * ys + x]) - y_offset;
                let d = i32::from(yuv.u()[(y / 2) * us + x / 2]) - 128;
                let e = i32::from(yuv.v()[(y / 2) * vs + x / 2]) - 128;
                let base = ym * c;
                let pixel = &mut row[x * 3..x * 3 + 3];
                pixel[0] = ((base + rv * e + 128) >> 8).clamp(0, 255) as u8;
                pixel[1] = ((base + gu * d + gv * e + 128) >> 8).clamp(0, 255) as u8;
                pixel[2] = ((base + bu * d + 128) >> 8).clamp(0, 255) as u8;
            }
        }
    });
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

#[derive(Clone, Copy)]
struct PlaneLevels {
    min: u8,
    max: u8,
    below_limited: u64,
    above_limited: u64,
    at_zero: u64,
    at_full: u64,
    samples: u64,
}

impl PlaneLevels {
    fn new() -> Self {
        Self { min: u8::MAX, max: 0, below_limited: 0, above_limited: 0, at_zero: 0, at_full: 0, samples: 0 }
    }

    fn add(&mut self, value: u8, low: u8, high: u8) {
        self.min = self.min.min(value);
        self.max = self.max.max(value);
        self.below_limited += u64::from(value < low);
        self.above_limited += u64::from(value > high);
        self.at_zero += u64::from(value == 0);
        self.at_full += u64::from(value == u8::MAX);
        self.samples += 1;
    }

    fn outside_percent(self) -> f64 {
        100.0 * (self.below_limited + self.above_limited) as f64 / self.samples.max(1) as f64
    }
}

#[derive(Clone, Copy)]
struct YuvLevels {
    y: PlaneLevels,
    u: PlaneLevels,
    v: PlaneLevels,
}

impl YuvLevels {
    fn from_frame(yuv: &impl YUVSource) -> Self {
        let (width, height) = yuv.dimensions();
        let (y_stride, u_stride, v_stride) = yuv.strides();
        let mut levels = Self { y: PlaneLevels::new(), u: PlaneLevels::new(), v: PlaneLevels::new() };
        for row in 0..height {
            for &value in &yuv.y()[row * y_stride..row * y_stride + width] {
                levels.y.add(value, 16, 235);
            }
        }
        for row in 0..height / 2 {
            for &value in &yuv.u()[row * u_stride..row * u_stride + width / 2] {
                levels.u.add(value, 16, 240);
            }
            for &value in &yuv.v()[row * v_stride..row * v_stride + width / 2] {
                levels.v.add(value, 16, 240);
            }
        }
        levels
    }
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
    range: Arc<std::sync::atomic::AtomicU8>,
    codec: VideoCodec,
) -> std::thread::JoinHandle<()> {
    match codec {
        VideoCodec::H264 => spawn_h264_decoder(rx, out, need_key, range),
        VideoCodec::H265 => spawn_hevc_decoder(rx, out, need_key, false),
        VideoCodec::H265Hdr => spawn_hevc_decoder(rx, out, need_key, true),
    }
}

#[cfg(target_os = "macos")]
fn hdr_sdr_lut() -> [u8; 256] {
    // VideoToolbox has already converted the HDR YUV surface to display-referred BGRA.
    // Its 8-bit result keeps the highlights but looks dim on an SDR egui surface, so lift
    // the mid/high range without moving black or hard-clipping the shoulder.
    const ANCHORS: &[(u8, u8)] = &[
        (0, 0),
        (32, 20),
        (64, 55),
        (80, 85),
        (96, 115),
        (112, 145),
        (128, 175),
        (144, 200),
        (160, 220),
        (192, 245),
        (255, 255),
    ];
    let mut lut = [0; 256];
    for pair in ANCHORS.windows(2) {
        let (x0, y0) = pair[0];
        let (x1, y1) = pair[1];
        for x in x0..=x1 {
            let t = (x - x0) as f32 / (x1 - x0) as f32;
            lut[x as usize] = (y0 as f32 + t * (y1 as f32 - y0 as f32)).round() as u8;
        }
    }
    lut
}

#[cfg(target_os = "macos")]
fn hdr_bgra_to_sdr_rgb(src: &[u8], dst: &mut [u8], lut: &[u8; 256]) {
    src.par_chunks_exact(4).zip(dst.par_chunks_exact_mut(3)).for_each(|(src, dst)| {
        let mut r = lut[src[2] as usize] as f32;
        let mut g = lut[src[1] as usize] as f32;
        let mut b = lut[src[0] as usize] as f32;
        let luma = 0.2126 * r + 0.7152 * g + 0.0722 * b;
        // The 8-bit VideoToolbox conversion loses some chroma in HDR midtones. Restore it
        // gently, fading the correction out near white so highlights remain neutral.
        let saturation = 1.0 + 0.20 * (1.0 - luma / 255.0);
        r = luma + (r - luma) * saturation;
        g = luma + (g - luma) * saturation;
        b = luma + (b - luma) * saturation;
        dst[0] = r.clamp(0.0, 255.0).round() as u8;
        dst[1] = g.clamp(0.0, 255.0).round() as u8;
        dst[2] = b.clamp(0.0, 255.0).round() as u8;
    });
}

fn spawn_h264_decoder(
    rx: std::sync::mpsc::Receiver<(u16, std::time::Instant, Vec<u8>)>,
    out: watch::Sender<Option<Arc<Frame>>>,
    need_key: Arc<std::sync::atomic::AtomicBool>,
    range: Arc<std::sync::atomic::AtomicU8>,
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
            let mut color = None;
            let mut levels_logged = false;
            // frames in a row that gave no picture
            let mut failing = 0u32;
            let mut next = rx.recv().ok();
            while let Some((index, queued_at, h264)) = next.take() {
                let detected = ColorSpec::from_h264(&h264).or_else(|| color.is_none().then(ColorSpec::default));
                if let Some(detected) = detected
                    && Some(detected) != color
                {
                    log::info!(
                        "video colour: range={}, matrix={:?} (primaries={}, transfer={}, matrix={})",
                        if detected.full_range { "full" } else { "limited" },
                        detected.matrix,
                        detected.primaries,
                        detected.transfer,
                        detected.matrix_code
                    );
                    color = Some(detected);
                }
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
                        if !levels_logged {
                            let levels = YuvLevels::from_frame(&yuv);
                            log::info!(
                                "video levels before RGB: Y={}..{} ({:.2}% outside 16..235, zero={}, 255={}), U={}..{} ({:.2}% outside 16..240), V={}..{} ({:.2}% outside 16..240)",
                                levels.y.min,
                                levels.y.max,
                                levels.y.outside_percent(),
                                levels.y.at_zero,
                                levels.y.at_full,
                                levels.u.min,
                                levels.u.max,
                                levels.u.outside_percent(),
                                levels.v.min,
                                levels.v.max,
                                levels.v.outside_percent(),
                            );
                            levels_logged = true;
                        }
                        let (w, h) = yuv.dimensions();
                        if rgb.capacity() < w * h * 3
                            && let Some(at) = reusable.iter().position(|f: &Arc<Frame>| Arc::strong_count(f) == 1)
                            && let Ok(frame) = Arc::try_unwrap(reusable.swap_remove(at))
                        {
                            rgb = frame.rgb;
                        }
                        rgb.resize(w * h * 3, 0);
                        let mut spec = color.unwrap_or_default();
                        match VideoRange::from_atomic(range.load(std::sync::atomic::Ordering::Relaxed)) {
                            VideoRange::Auto => {}
                            VideoRange::Limited => spec.full_range = false,
                            VideoRange::Full => spec.full_range = true,
                        }
                        write_rgb8(&yuv, spec, &mut rgb);
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

#[cfg(target_os = "macos")]
fn spawn_hevc_decoder(
    rx: std::sync::mpsc::Receiver<(u16, std::time::Instant, Vec<u8>)>,
    out: watch::Sender<Option<Arc<Frame>>>,
    need_key: Arc<std::sync::atomic::AtomicBool>,
    hdr: bool,
) -> std::thread::JoinHandle<()> {
    use macos_media_toolkit::videotoolbox::{DecodeError, DecoderConfig, HevcDecoder};

    std::thread::Builder::new()
        .name("psconnector-hevc".into())
        .spawn(move || {
            let mut decoder = HevcDecoder::new(&DecoderConfig::default());
            let mut rgb = Vec::new();
            let mut stats = DecodeStats::default();
            let hdr_lut = hdr.then(hdr_sdr_lut);
            let mut next = rx.recv().ok();
            let mut announced = false;
            while let Some((index, queued_at, access_unit)) = next.take() {
                let queued = queued_at.elapsed();
                let t0 = std::time::Instant::now();
                let decoded = decoder.decode(&access_unit);
                let t_decode = t0.elapsed();
                let waiting = rx.try_recv();
                let behind = waiting.is_ok();
                match decoded {
                    Ok(decoded) if !behind => {
                        let t1 = std::time::Instant::now();
                        let frame = decoded.frame;
                        let width = frame.width as usize;
                        let height = frame.height as usize;
                        rgb.resize(width * height * 3, 0);
                        if let Some(lut) = &hdr_lut {
                            hdr_bgra_to_sdr_rgb(&frame.bgra, &mut rgb, lut);
                        } else {
                            frame.bgra.par_chunks_exact(4).zip(rgb.par_chunks_exact_mut(3)).for_each(|(src, dst)| {
                                dst[0] = src[2];
                                dst[1] = src[1];
                                dst[2] = src[0];
                            });
                        }
                        if !announced {
                            log::info!(
                                "HEVC decoded by VideoToolbox: {}x{} BGRA ({})",
                                width,
                                height,
                                if hdr { "HDR mapped to SDR display" } else { "SDR" }
                            );
                            announced = true;
                        }
                        out.send_replace(Some(Arc::new(Frame { width, height, rgb: std::mem::take(&mut rgb), index })));
                        stats.shown += 1;
                        stats.convert += t1.elapsed();
                    }
                    Ok(_) => stats.skipped += u32::from(behind),
                    Err(DecodeError::MissingKeyframe) => {
                        need_key.store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                    Err(DecodeError::Frame(e) | DecodeError::Fatal(e)) => {
                        warn!("HEVC decode failed: {e}");
                        decoder.reset();
                        need_key.store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                }
                stats.decoded += 1;
                stats.queue += queued;
                stats.max_queue = stats.max_queue.max(queued);
                stats.decode += t_decode;
                stats.report();
                next = match waiting {
                    Ok(f) => Some(f),
                    Err(std::sync::mpsc::TryRecvError::Empty) => rx.recv().ok(),
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => None,
                };
            }
        })
        .expect("HEVC video thread")
}

#[cfg(not(target_os = "macos"))]
fn spawn_hevc_decoder(
    _rx: std::sync::mpsc::Receiver<(u16, std::time::Instant, Vec<u8>)>,
    _out: watch::Sender<Option<Arc<Frame>>>,
    _need_key: Arc<std::sync::atomic::AtomicBool>,
    _hdr: bool,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(|| warn!("H.265 decoding is not available on this platform"))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestYuv {
        y: Vec<u8>,
        u: Vec<u8>,
        v: Vec<u8>,
    }

    impl YUVSource for TestYuv {
        fn dimensions(&self) -> (usize, usize) {
            (2, 2)
        }

        fn strides(&self) -> (usize, usize, usize) {
            (2, 1, 1)
        }

        fn y(&self) -> &[u8] {
            &self.y
        }

        fn u(&self) -> &[u8] {
            &self.u
        }

        fn v(&self) -> &[u8] {
            &self.v
        }
    }

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

    #[test]
    fn reads_actual_ps5_bt709_limited_vui() {
        let h264 = [
            0, 0, 0, 1, 0x67, 0x4d, 0x40, 0x32, 0x91, 0x8a, 0x01, 0xe0, 0x08, 0x9f, 0x97, 0x01, 0x6a, 0x02, 0x02, 0x02,
            0x80, 0x00, 0x03, 0xe9, 0x00, 0x01, 0xd4, 0xc0, 0x44, 0xd0, 0xf1, 0xf1, 0x50, 0, 0, 0, 1, 0x68, 0xee, 0x3c,
            0x80,
        ];
        let spec = ColorSpec::from_h264(&h264).unwrap();
        assert_eq!(
            spec,
            ColorSpec { full_range: false, matrix: Matrix::Bt709, primaries: 1, transfer: 1, matrix_code: 1 }
        );
    }

    #[test]
    fn bt709_limited_maps_reference_black_and_white() {
        let yuv = TestYuv { y: vec![16, 235, 16, 235], u: vec![128], v: vec![128] };
        let mut rgb = vec![0; 12];
        write_rgb8(
            &yuv,
            ColorSpec { full_range: false, matrix: Matrix::Bt709, primaries: 1, transfer: 1, matrix_code: 1 },
            &mut rgb,
        );
        assert_eq!(&rgb[0..3], &[0, 0, 0]);
        assert_eq!(&rgb[3..6], &[255, 255, 255]);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn hdr_display_curve_preserves_endpoints_and_never_reverses() {
        let lut = hdr_sdr_lut();
        assert_eq!((lut[0], lut[255]), (0, 255));
        assert!(lut.windows(2).all(|pair| pair[0] <= pair[1]));
    }
}
