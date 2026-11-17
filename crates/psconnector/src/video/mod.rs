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
//! them, and prepending the video header (SPS/PPS) from STREAMINFO gives an Annex-B frame. Missing units are
//! rebuilt from the FEC units (`fec.rs`); only when that is not enough is the frame dropped and a key frame
//! requested.
//!
//! Modules: `packet` (unit assembly), `fec`, `h264` (openh264), `hw` / `vt` / `hevc` (VideoToolbox),
//! `frame` (decoded frames → RGB), `color` (YUV → RGB from the SPS), `bits` (bitstream reading), `stats`.

use std::collections::BTreeMap;
use std::sync::Arc;

use log::{debug, warn};
use openh264::decoder::Decoder;
use openh264::formats::YUVSource;
// rayon and the frame helpers below are only used by the macOS decoder path (`hw`) through `use super::*`
#[cfg(target_os = "macos")]
use rayon::prelude::*;
use tokio::sync::watch;

use crate::VideoCodec;

mod bits;
mod color;
mod fec;
mod frame;
mod h264;
// H.265 and VideoToolbox are macOS-only; everything else reaches them through `hw`
#[cfg(target_os = "macos")]
mod hevc;
mod hw;
mod packet;
mod stats;
#[cfg(target_os = "macos")]
mod vt;

pub use color::VideoRange;
pub(crate) use color::*;
#[cfg(target_os = "macos")]
pub(crate) use frame::*;
pub use frame::{Frame, PixelFormat, hdr_sdr_curve};
pub(crate) use stats::*;

#[allow(unused_imports)]
use h264::*;
pub use packet::*;

/// Whether this platform can decode the H.265 stream (else only H.264 is requested).
pub(crate) const HEVC_AVAILABLE: bool = hw::HEVC;

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
        VideoCodec::H265 => hw::spawn_hevc_decoder(rx, out, need_key, false),
        VideoCodec::H265Hdr => hw::spawn_hevc_decoder(rx, out, need_key, true),
    }
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

    #[test]
    fn hdr_display_curve_preserves_endpoints_and_never_reverses() {
        let lut = hdr_sdr_curve();
        assert_eq!((lut[0], lut[255]), (0, 255));
        assert!(lut.windows(2).all(|pair| pair[0] <= pair[1]));
    }
}
