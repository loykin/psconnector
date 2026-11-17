//! Decoded frames and their conversion to 8-bit RGB.

use rayon::prelude::*;

use super::hw;

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
    /// ([`Frame::native_buffer`]); `full_range` and `bt709` say how to turn it into RGB.
    Nv12 { full_range: bool, bt709: bool },
}

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
    /// The decoder's own buffer when the pixels were not copied out
    native: Option<hw::NativeBuf>,
}

impl Frame {
    pub fn new(width: usize, height: usize, format: PixelFormat, data: Vec<u8>, index: u16) -> Self {
        Self { width, height, format, data, index, rgb: std::sync::OnceLock::new(), native: None }
    }

    /// A frame that keeps the decoder's pixel buffer instead of a copy.
    pub(crate) fn native(buf: hw::NativeBuf, format: PixelFormat, index: u16) -> Self {
        let (width, height) = (buf.width(), buf.height());
        Self { native: Some(buf), ..Self::new(width, height, format, Vec::new(), index) }
    }

    /// The decoder's own buffer when the pixels were not copied out (macOS: an IOSurface-backed,
    /// Metal-compatible CVPixelBufferRef; other systems: always `None`). It lives as long as this frame:
    /// hold the frame while the GPU reads it.
    pub fn native_buffer(&self) -> Option<*mut std::ffi::c_void> {
        self.native.as_ref().map(hw::NativeBuf::handle)
    }

    /// 8-bit SDR RGB, 3 bytes per pixel.
    pub fn rgb(&self) -> &[u8] {
        if self.format == PixelFormat::Rgb8 {
            return &self.data;
        }
        self.rgb.get_or_init(|| {
            let mut out = vec![0u8; self.width * self.height * 3];
            if let Some(n) = &self.native {
                n.to_rgb(self.format, &mut out);
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

pub(crate) fn bgra_to_rgb(src: &[u8], out: &mut [u8]) {
    src.par_chunks_exact(4).zip(out.par_chunks_exact_mut(3)).for_each(|(s, d)| {
        d[0] = s[2];
        d[1] = s[1];
        d[2] = s[0];
    });
}

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
