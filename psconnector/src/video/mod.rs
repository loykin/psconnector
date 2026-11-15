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

mod fec;
mod h264;
mod hevc;
mod packet;
#[cfg(target_os = "macos")]
mod vt;

#[allow(unused_imports)]
use h264::*;
#[allow(unused_imports)]
use hevc::*;
pub use packet::*;

/// How a frame's pixels are stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelFormat {
    /// 8-bit RGB, 3 bytes per pixel (software H.264 decoding)
    Rgb8,
    /// 8-bit BGRA, 4 bytes per pixel, straight from a hardware decoder
    Bgra8,
    /// BGRA from an HDR stream, display-referred: [`hdr_sdr_curve`] and a small chroma restore turn it into SDR
    BgraHdr8,
    /// 8-bit YUV 4:2:0, a Y plane and an interleaved CbCr plane (hardware H.264). Only in a native frame
    /// ([`Frame::cv_pixel_buffer`]); `full_range` and `bt709` say how to turn it into RGB.
    Nv12 { full_range: bool, bt709: bool },
}

/// A decoder's own pixel buffer (macOS: an IOSurface-backed CVPixelBuffer).
#[cfg(target_os = "macos")]
#[derive(Clone)]
pub(crate) struct Native(pub(crate) vt::PixelBuf);

// SAFETY: a CVPixelBuffer is reference counted atomically; the decoder no longer writes to it once it
// has handed it over, and it is only read (locked read-only) afterwards.
#[cfg(target_os = "macos")]
unsafe impl Send for Native {}
#[cfg(target_os = "macos")]
unsafe impl Sync for Native {}

/// One decoded frame (row-major, no padding between rows).
///
/// The pixels stay in the decoder's format so the window can draw them without a CPU conversion;
/// [`Frame::rgb`] gives 8-bit SDR RGB (converted once, on first use) for macros, OCR and screenshots.
#[derive(Clone)]
pub struct Frame {
    pub width: usize,
    pub height: usize,
    pub format: PixelFormat,
    /// Pixels in `format`; empty for a native frame (the pixels stay in the decoder's buffer)
    pub data: Vec<u8>,
    /// Frame index assigned by the console
    pub index: u16,
    rgb: std::sync::OnceLock<Vec<u8>>,
    #[cfg(target_os = "macos")]
    native: Option<Native>,
}

impl Frame {
    pub fn new(width: usize, height: usize, format: PixelFormat, data: Vec<u8>, index: u16) -> Self {
        Self {
            width,
            height,
            format,
            data,
            index,
            rgb: std::sync::OnceLock::new(),
            #[cfg(target_os = "macos")]
            native: None,
        }
    }

    /// A frame that keeps the decoder's pixel buffer instead of a copy.
    #[cfg(target_os = "macos")]
    pub(crate) fn native(pb: vt::PixelBuf, format: PixelFormat, index: u16) -> Self {
        let (width, height) = (pb.width(), pb.height());
        Self { native: Some(Native(pb)), ..Self::new(width, height, format, Vec::new(), index) }
    }

    /// The decoder's CVPixelBuffer (IOSurface-backed, Metal-compatible) when the pixels were not copied
    /// out. It lives as long as this frame: hold the frame while the GPU reads it.
    #[cfg(target_os = "macos")]
    pub fn cv_pixel_buffer(&self) -> Option<*mut std::ffi::c_void> {
        self.native.as_ref().map(|n| &*n.0 as *const cidre::cv::PixelBuf as *mut std::ffi::c_void)
    }

    /// 8-bit SDR RGB, 3 bytes per pixel.
    pub fn rgb(&self) -> &[u8] {
        if self.format == PixelFormat::Rgb8 {
            return &self.data;
        }
        self.rgb.get_or_init(|| {
            let mut out = vec![0u8; self.width * self.height * 3];
            #[cfg(target_os = "macos")]
            if let Some(n) = &self.native {
                native_to_rgb(&n.0, self.format, &mut out);
                return out;
            }
            match self.format {
                PixelFormat::Bgra8 => bgra_to_rgb(&self.data, &mut out),
                PixelFormat::BgraHdr8 => hdr_bgra_to_sdr_rgb(&self.data, &mut out, &hdr_sdr_curve()),
                PixelFormat::Rgb8 | PixelFormat::Nv12 { .. } => {}
            }
            out
        })
    }

    /// Takes the pixel buffer back for reuse (when nothing else holds the frame).
    pub(crate) fn into_data(self) -> Vec<u8> {
        self.data
    }
}

fn bgra_to_rgb(src: &[u8], out: &mut [u8]) {
    src.par_chunks_exact(4).zip(out.par_chunks_exact_mut(3)).for_each(|(s, d)| {
        d[0] = s[2];
        d[1] = s[1];
        d[2] = s[0];
    });
}

/// Reads a native frame's pixel buffer into packed RGB (locked read-only for the copy).
#[cfg(target_os = "macos")]
fn native_to_rgb(pb: &vt::PixelBuf, format: PixelFormat, out: &mut [u8]) {
    let mut pb = pb.retained();
    let flags = cidre::cv::pixel_buffer::LockFlags::READ_ONLY;
    // SAFETY: the planes are valid while locked; every read stays inside each plane's rows.
    unsafe {
        if pb.lock_base_addr(flags).result().is_err() {
            return;
        }
        let (w, h) = (pb.width(), pb.height());
        match format {
            PixelFormat::Nv12 { full_range, bt709 } if pb.plane_count() == 2 => {
                let (y, ys) = (pb.plane_base_address(0), pb.plane_bytes_per_row(0));
                let (uv, uvs) = (pb.plane_base_address(1), pb.plane_bytes_per_row(1));
                if !y.is_null() && !uv.is_null() {
                    let y = std::slice::from_raw_parts(y, ys * h);
                    let uv = std::slice::from_raw_parts(uv, uvs * h.div_ceil(2));
                    let spec = ColorSpec {
                        full_range,
                        matrix: if bt709 { Matrix::Bt709 } else { Matrix::Bt601 },
                        ..ColorSpec::default()
                    };
                    nv12_to_rgb(y, ys, uv, uvs, w, h, spec, out);
                }
            }
            PixelFormat::Bgra8 | PixelFormat::BgraHdr8 => {
                let (base, stride) = (pb.base_address() as *const u8, pb.bytes_per_row());
                if !base.is_null() {
                    let mut bgra = Vec::with_capacity(w * h * 4);
                    for row in 0..h {
                        bgra.extend_from_slice(std::slice::from_raw_parts(base.add(row * stride), w * 4));
                    }
                    if format == PixelFormat::Bgra8 {
                        bgra_to_rgb(&bgra, out);
                    } else {
                        hdr_bgra_to_sdr_rgb(&bgra, out, &hdr_sdr_curve());
                    }
                }
            }
            _ => {}
        }
        pb.unlock_lock_base_addr(flags);
    }
}

/// The 8-bit integer YUV→RGB that the window's shader also uses.
#[allow(clippy::too_many_arguments)]
pub(crate) fn nv12_to_rgb(y: &[u8], ys: usize, uv: &[u8], uvs: usize, w: usize, h: usize, spec: ColorSpec, out: &mut [u8]) {
    let (ym, rv, gu, gv, bu) = coefficients(spec);
    let y_offset = if spec.full_range { 0 } else { 16 };
    out.par_chunks_mut(w * 3).take(h).enumerate().for_each(|(row, line)| {
        for x in 0..w {
            let c = i32::from(y[row * ys + x]) - y_offset;
            let d = i32::from(uv[(row / 2) * uvs + (x / 2) * 2]) - 128;
            let e = i32::from(uv[(row / 2) * uvs + (x / 2) * 2 + 1]) - 128;
            let base = ym * c;
            let p = &mut line[x * 3..x * 3 + 3];
            p[0] = ((base + rv * e + 128) >> 8).clamp(0, 255) as u8;
            p[1] = ((base + gu * d + gv * e + 128) >> 8).clamp(0, 255) as u8;
            p[2] = ((base + bu * d + 128) >> 8).clamp(0, 255) as u8;
        }
    });
}

/// Fixed-point (×256) YUV→RGB factors: Y scale, Cr→R, Cb→G, Cr→G, Cb→B.
pub(crate) fn coefficients(spec: ColorSpec) -> (i32, i32, i32, i32, i32) {
    match (spec.full_range, spec.matrix) {
        (false, Matrix::Bt709) => (298, 459, -55, -136, 541),
        (false, Matrix::Bt601) => (298, 409, -100, -208, 516),
        (true, Matrix::Bt709) => (256, 403, -48, -120, 475),
        (true, Matrix::Bt601) => (256, 359, -88, -183, 454),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Matrix {
    Bt601,
    Bt709,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ColorSpec {
    pub(crate) full_range: bool,
    pub(crate) matrix: Matrix,
    pub(crate) primaries: u8,
    pub(crate) transfer: u8,
    pub(crate) matrix_code: u8,
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
    pub(crate) fn from_atomic(value: u8) -> Self {
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

pub(crate) struct Bits {
    pub(crate) data: Vec<u8>,
    pub(crate) at: usize,
}

impl Bits {
    pub(crate) fn from_ebsp(data: &[u8]) -> Self {
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

    pub(crate) fn bit(&mut self) -> Option<u32> {
        let byte = *self.data.get(self.at / 8)?;
        let value = u32::from((byte >> (7 - self.at % 8)) & 1);
        self.at += 1;
        Some(value)
    }

    pub(crate) fn bits(&mut self, count: usize) -> Option<u32> {
        let mut value = 0;
        for _ in 0..count {
            value = (value << 1) | self.bit()?;
        }
        Some(value)
    }

    pub(crate) fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        Some((1u32 << zeros) - 1 + if zeros == 0 { 0 } else { self.bits(zeros)? })
    }

    pub(crate) fn se(&mut self) -> Option<i32> {
        let value = self.ue()?;
        Some(if value & 1 == 0 { -((value / 2) as i32) } else { value.div_ceil(2) as i32 })
    }

    pub(crate) fn scaling_list(&mut self, size: usize) -> Option<()> {
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

pub(crate) fn start_code(data: &[u8], from: usize) -> Option<(usize, usize)> {
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
    pub(crate) fn from_h264(data: &[u8]) -> Option<Self> {
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

    pub(crate) fn from_sps(sps: &[u8]) -> Option<Self> {
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

pub(crate) fn write_rgb8(yuv: &(impl YUVSource + Sync), spec: ColorSpec, target: &mut [u8]) {
    let (width, height) = yuv.dimensions();
    let (ys, us, vs) = yuv.strides();
    assert_eq!(target.len(), width * height * 3);
    let (ym, rv, gu, gv, bu) = coefficients(spec);
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

/// Decoding speed, logged every 10 s.
#[derive(Default)]
pub(crate) struct DecodeStats {
    pub(crate) since: Option<std::time::Instant>,
    pub(crate) decoded: u32,
    pub(crate) shown: u32,
    pub(crate) skipped: u32,
    pub(crate) queue: std::time::Duration,
    pub(crate) max_queue: std::time::Duration,
    pub(crate) decode: std::time::Duration,
    pub(crate) convert: std::time::Duration,
}

#[derive(Clone, Copy)]
pub(crate) struct PlaneLevels {
    pub(crate) min: u8,
    pub(crate) max: u8,
    pub(crate) below_limited: u64,
    pub(crate) above_limited: u64,
    pub(crate) at_zero: u64,
    pub(crate) at_full: u64,
    pub(crate) samples: u64,
}

impl PlaneLevels {
    pub(crate) fn new() -> Self {
        Self { min: u8::MAX, max: 0, below_limited: 0, above_limited: 0, at_zero: 0, at_full: 0, samples: 0 }
    }

    pub(crate) fn add(&mut self, value: u8, low: u8, high: u8) {
        self.min = self.min.min(value);
        self.max = self.max.max(value);
        self.below_limited += u64::from(value < low);
        self.above_limited += u64::from(value > high);
        self.at_zero += u64::from(value == 0);
        self.at_full += u64::from(value == u8::MAX);
        self.samples += 1;
    }

    pub(crate) fn outside_percent(self) -> f64 {
        100.0 * (self.below_limited + self.above_limited) as f64 / self.samples.max(1) as f64
    }
}

#[derive(Clone, Copy)]
pub(crate) struct YuvLevels {
    pub(crate) y: PlaneLevels,
    pub(crate) u: PlaneLevels,
    pub(crate) v: PlaneLevels,
}

impl YuvLevels {
    pub(crate) fn from_frame(yuv: &impl YUVSource) -> Self {
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
    pub(crate) fn report(&mut self) {
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
/// SDR display curve for HDR frames (see [`PixelFormat::BgraHdr8`]); the window applies it on the GPU.
pub fn hdr_sdr_curve() -> [u8; 256] {
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

pub(crate) fn hdr_bgra_to_sdr_rgb(src: &[u8], dst: &mut [u8], lut: &[u8; 256]) {
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
        a.push(frame, 0, 1, 1, vec![0, 0, 9], &mut out);
        out.iter().filter(|x| matches!(x, Assembled::Frame(..))).count()
    }

    #[test]
    fn rebuilds_a_lost_source_unit_from_fec() {
        // 3 source units of 40 bytes (2-byte padding field + data; the last one has 6 bytes of filler)
        let mut src: Vec<Vec<u8>> = (0..3u8).map(|i| [vec![0, 0], vec![i + 1; 38]].concat()).collect();
        src[2] = [vec![0, 6], vec![9; 32], vec![0; 6]].concat();
        let fec = fec::encode(&src, 2);
        let expected: Vec<u8> = [vec![1u8; 38], vec![2; 38], vec![9; 32]].concat();
        for lost in 0..3u16 {
            let mut a = Assembler::new(vec![]);
            let mut out = Vec::new();
            for (i, u) in src.iter().chain(&fec).enumerate() {
                if i as u16 != lost {
                    a.push(7, i as u16, 3, 5, u.clone(), &mut out);
                }
            }
            let frames: Vec<Vec<u8>> = out
                .into_iter()
                .filter_map(|x| match x {
                    Assembled::Frame(_, f) => Some(f),
                    _ => None,
                })
                .collect();
            assert_eq!(frames.len(), 1, "one frame when unit {lost} is lost");
            let got = &frames[0];
            assert_eq!(&got[..76], &expected[..76], "unit {lost}");
            assert_eq!(&got[76..76 + 32], &expected[76..], "unit {lost}");
        }
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
        // and a stream of late units for recent frames never looks like a restart
        let last = (0..200u16).rev().find(|_| true).unwrap();
        for _ in 0..1000 {
            assert_eq!(unit(&mut a, last), 0);
        }
        assert_eq!(unit(&mut a, last + 1), 1, "still following the stream");
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
        let lut = hdr_sdr_curve();
        assert_eq!((lut[0], lut[255]), (0, 255));
        assert!(lut.windows(2).all(|pair| pair[0] <= pair[1]));
    }
}
