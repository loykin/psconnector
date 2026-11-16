//! The platform's hardware video decoding, behind one interface so the rest of `video` has no platform code.
//!
//! macOS: VideoToolbox (`vt.rs`). Decoded pictures stay in the decoder's IOSurface-backed buffers, which the
//! window draws without a copy. Other systems have no hardware path yet: [`HwH264::new`] returns `None`,
//! a [`NativeBuf`] cannot exist and H.265 is not offered, so callers take the software path without
//! having to know why. A new platform implements the same items in its own `imp` module.

#[allow(unused_imports)]
use super::*;

/// How the hardware decoder stored a picture.
// Only a platform with a hardware decoder makes these.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Coded {
    Bgra,
    /// 8-bit 4:2:0; `full_range` as the stream coded it
    Nv12 {
        full_range: bool,
    },
}

pub(crate) use imp::*;

#[cfg(target_os = "macos")]
mod imp {
    use super::super::vt;
    use super::*;

    /// H.265 (SDR and HDR) can be decoded.
    pub(crate) const HEVC: bool = true;
    /// Shown in the log when the hardware H.264 decoder is in use.
    pub(crate) const H264_NAME: &str = "VideoToolbox (hardware)";

    /// The decoder's own pixel buffer: an IOSurface-backed, Metal-compatible CVPixelBuffer.
    #[derive(Clone)]
    pub(crate) struct NativeBuf(vt::PixelBuf);

    // SAFETY: a CVPixelBuffer is reference counted atomically; the decoder no longer writes to it once it
    // has handed it over, and it is only read (locked read-only) afterwards.
    unsafe impl Send for NativeBuf {}
    unsafe impl Sync for NativeBuf {}

    impl NativeBuf {
        pub(crate) fn new(pb: vt::PixelBuf) -> Self {
            Self(pb)
        }

        pub(crate) fn width(&self) -> usize {
            self.0.width()
        }

        pub(crate) fn height(&self) -> usize {
            self.0.height()
        }

        /// The CVPixelBufferRef, for the window's Metal texture cache.
        pub(crate) fn handle(&self) -> *mut std::ffi::c_void {
            &*self.0 as *const cidre::cv::PixelBuf as *mut std::ffi::c_void
        }

        /// Reads the buffer into packed RGB (locked read-only for the copy).
        pub(crate) fn to_rgb(&self, format: PixelFormat, out: &mut [u8]) {
            let mut pb = self.0.retained();
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
    }

    pub(crate) struct HwH264(vt::VtH264);

    impl HwH264 {
        pub(crate) fn new() -> Option<Self> {
            Some(Self(vt::VtH264::new()))
        }

        /// Drops the decoder's state (after a damaged frame left it stuck).
        pub(crate) fn reset(&mut self) {
            self.0.reset();
        }

        /// Decodes one Annex-B access unit; `None` when it gave no picture.
        pub(crate) fn decode(&mut self, annex_b: &[u8]) -> Result<Option<(NativeBuf, Coded)>, String> {
            Ok(self.0.decode(annex_b)?.map(|pb| {
                let coded = match pb.pixel_format() {
                    cidre::cv::PixelFormat::_32_BGRA => Coded::Bgra,
                    f => Coded::Nv12 { full_range: f == cidre::cv::PixelFormat::_420F },
                };
                (NativeBuf(pb), coded)
            }))
        }
    }

    pub(crate) use super::super::hevc::spawn_hevc_decoder;

    /// The 8-bit integer YUV→RGB that the window's shader also uses.
    #[allow(clippy::too_many_arguments)]
    fn nv12_to_rgb(y: &[u8], ys: usize, uv: &[u8], uvs: usize, w: usize, h: usize, spec: ColorSpec, out: &mut [u8]) {
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
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use super::*;

    pub(crate) const HEVC: bool = false;
    pub(crate) const H264_NAME: &str = "hardware";

    /// No native buffers here: this type has no values, so `Option<NativeBuf>` is always `None`.
    #[derive(Clone)]
    pub(crate) enum NativeBuf {}

    impl NativeBuf {
        pub(crate) fn width(&self) -> usize {
            match *self {}
        }

        pub(crate) fn height(&self) -> usize {
            match *self {}
        }

        pub(crate) fn handle(&self) -> *mut std::ffi::c_void {
            match *self {}
        }

        pub(crate) fn to_rgb(&self, _format: PixelFormat, _out: &mut [u8]) {
            match *self {}
        }
    }

    /// No hardware H.264 decoder: [`HwH264::new`] always returns `None`.
    pub(crate) enum HwH264 {}

    impl HwH264 {
        pub(crate) fn new() -> Option<Self> {
            None
        }

        pub(crate) fn reset(&mut self) {
            match *self {}
        }

        pub(crate) fn decode(&mut self, _annex_b: &[u8]) -> Result<Option<(NativeBuf, Coded)>, String> {
            match *self {}
        }
    }

    pub(crate) fn spawn_hevc_decoder(
        _rx: std::sync::mpsc::Receiver<(u16, std::time::Instant, Vec<u8>)>,
        _out: watch::Sender<Option<Arc<Frame>>>,
        _need_key: Arc<std::sync::atomic::AtomicBool>,
        _hdr: bool,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(|| warn!("H.265 decoding is not available on this platform"))
    }
}
