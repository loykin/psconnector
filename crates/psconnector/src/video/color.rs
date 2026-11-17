//! YUV → RGB: the colour description in the stream's SPS (range, matrix) and the conversion itself.

use openh264::formats::YUVSource;
use rayon::prelude::*;

use super::bits::{Bits, start_code};

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
    /// Short name used on the command line and the command port.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Limited => "limited",
            Self::Full => "full",
        }
    }

    /// Reads [`name`](Self::name), ignoring case.
    pub fn parse(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "limited" => Some(Self::Limited),
            "full" => Some(Self::Full),
            _ => None,
        }
    }

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
