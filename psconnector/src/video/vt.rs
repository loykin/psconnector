//! VideoToolbox decoding (macOS hardware) for H.264 and H.265.
//!
//! Decoded pictures are not copied: each frame keeps the decoder's IOSurface-backed, Metal-compatible
//! pixel buffer, and the window draws it straight from GPU memory. H.264 comes out as YUV 4:2:0 (NV12)
//! with the stream's own sample range, so the window does YUV→RGB itself and a hand-picked range costs
//! nothing extra; H.265 comes out as BGRA (VideoToolbox also maps HDR to the display there).
//!
//! The H.265 reference-picture repair follows macos-media-toolkit's `HevcDecoder` (MIT OR Apache-2.0),
//! using its public slice-header helpers.

use std::ffi::c_void;
use std::sync::Mutex;

use cidre::{arc, cf, cm, cv, os, vt};
use macos_media_toolkit::hevc;

pub(crate) type PixelBuf = arc::R<cv::PixelBuf>;

struct Shared {
    output: Mutex<Option<Result<PixelBuf, String>>>,
}

extern "C" fn output_callback(
    ctx: *mut Shared,
    _frame_ref: *mut c_void,
    status: os::Status,
    _flags: vt::DecodeInfoFlags,
    image_buf: Option<&cv::ImageBuf>,
    _pts: cm::Time,
    _duration: cm::Time,
) {
    // SAFETY: `ctx` is the Box::into_raw'd Shared registered with the session; it outlives the session
    // (reclaimed only after invalidate) and decoding is synchronous, so there is no concurrent access.
    let shared = unsafe { &*ctx };
    let result = status
        .result()
        .map_err(|e| format!("decode failed: {e:?}"))
        .and_then(|()| image_buf.map(|i| i.retained()).ok_or_else(|| "no image buffer".to_string()));
    *shared.output.lock().unwrap() = Some(result);
}

/// One decompression session: its format description and the slot the output callback fills.
struct Session {
    session: arc::R<vt::DecompressionSession>,
    format: arc::R<cm::VideoFormatDesc>,
    shared: *mut Shared,
}

impl Drop for Session {
    fn drop(&mut self) {
        self.session.invalidate();
        // SAFETY: the session is invalidated, so the callback can no longer run; reclaimed exactly once.
        unsafe { drop(Box::from_raw(self.shared)) };
    }
}

// SAFETY: the session is only used from the decoder thread that owns it.
unsafe impl Send for Session {}

impl Session {
    fn new(format: arc::R<cm::VideoFormatDesc>, pixel_format: cv::PixelFormat) -> Result<Self, String> {
        let shared = Box::into_raw(Box::new(Shared { output: Mutex::new(None) }));
        // IOSurface-backed and Metal-compatible, so the window can wrap the planes as textures
        let surface_props = cf::Dictionary::new();
        let attrs = cf::DictionaryOf::<cf::String, cf::Type>::with_keys_values(
            &[
                cv::pixel_buffer_keys::pixel_format(),
                cv::pixel_buffer_keys::metal_compatibility(),
                cv::pixel_buffer_keys::io_surf_props(),
            ],
            &[pixel_format.to_cf_number().as_type_ref(), cf::Boolean::value_true().as_type_ref(), surface_props.as_type_ref()],
        );
        let record = vt::DecompressionOutputCbRecord::new(shared, Some(output_callback));
        match vt::DecompressionSession::new(&format, None, Some(&attrs), Some(&record)) {
            Ok(session) => Ok(Self { session, format, shared }),
            Err(e) => {
                // SAFETY: no session was created, nothing else holds `shared`.
                unsafe { drop(Box::from_raw(shared)) };
                Err(format!("VTDecompressionSession: {e:?}"))
            }
        }
    }

    /// Decodes one access unit of length-prefixed (4-byte) NAL units, synchronously.
    fn decode(&self, packed: &[u8]) -> Result<PixelBuf, String> {
        let mut block = cm::BlockBuf::with_mem_block(packed.len()).map_err(|e| format!("block buffer: {e:?}"))?;
        block.as_mut_slice().map_err(|e| format!("block buffer: {e:?}"))?.copy_from_slice(packed);
        let timing =
            cm::SampleTimingInfo { duration: cm::Time::new(1, 60), pts: cm::Time::new(0, 60), dts: cm::Time::invalid() };
        let sizes = [packed.len()];
        let mut sample = None;
        // SAFETY: everything passed lives for the call; the counts (1) match the one-element arrays.
        unsafe {
            cm::SampleBuf::create_in(
                None,
                Some(&block),
                true,
                None,
                std::ptr::null(),
                Some(&self.format),
                1,
                1,
                &timing,
                1,
                sizes.as_ptr(),
                &mut sample,
            )
            .map_err(|e| format!("sample buffer: {e:?}"))?;
        }
        let sample = sample.ok_or("no sample buffer")?;
        // SAFETY: see Session; the callback fills `output` before decode returns (synchronous decoding).
        let shared = unsafe { &*self.shared };
        *shared.output.lock().unwrap() = None;
        let submitted = self.session.decode(&sample, vt::DecodeFrameFlags::default());
        let result = shared.output.lock().unwrap().take();
        submitted.map_err(|e| format!("submit: {e:?}")).and_then(|()| result.unwrap_or(Err("no output".into())))
    }
}

fn push_nal(out: &mut Vec<u8>, nal: &[u8]) {
    out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
    out.extend_from_slice(nal);
}

/// Splits Annex B data into NAL units (without start codes).
fn nal_units(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::with_capacity(starts.len());
    for (n, &s) in starts.iter().enumerate() {
        let mut end = starts.get(n + 1).map_or(data.len(), |&next| next - 3);
        // a 4-byte start code leaves a zero before the next one
        while end > s && data[end - 1] == 0 && starts.get(n + 1).is_some() {
            end -= 1;
        }
        if end > s {
            out.push(&data[s..end]);
        }
    }
    out
}

/// H.264 → NV12 with the stream's own sample range (VideoToolbox does no colour conversion).
pub(crate) struct VtH264 {
    session: Option<Session>,
    params: Option<(Vec<u8>, Vec<u8>)>,
    waiting_for_key: bool,
    packed: Vec<u8>,
}

impl VtH264 {
    pub fn new() -> Self {
        Self { session: None, params: None, waiting_for_key: true, packed: Vec::new() }
    }

    pub fn reset(&mut self) {
        self.session = None;
        self.params = None;
        self.waiting_for_key = true;
    }

    /// Decodes one Annex B access unit. Ok(None) while waiting for a key frame.
    pub fn decode(&mut self, annex_b: &[u8]) -> Result<Option<PixelBuf>, String> {
        let nals = nal_units(annex_b);
        let kind = |n: &[u8]| n.first().map_or(0, |b| b & 0x1F);
        let sps = nals.iter().find(|n| kind(n) == 7);
        let pps = nals.iter().find(|n| kind(n) == 8);
        if let (Some(sps), Some(pps)) = (sps, pps)
            && self.params.as_ref().is_none_or(|(s, p)| s.as_slice() != *sps || p.as_slice() != *pps)
        {
            let pointers = [sps.as_ptr(), pps.as_ptr()];
            let sizes = [sps.len(), pps.len()];
            let format = cm::VideoFormatDesc::with_h264_param_sets(&pointers, &sizes, 4)
                .map_err(|e| format!("H.264 format description: {e:?}"))?;
            // ask for the range the stream is coded in, so the samples arrive untouched
            let full = super::ColorSpec::from_sps(&sps[1..]).is_some_and(|c| c.full_range);
            let pixel_format = if full { cv::PixelFormat::_420F } else { cv::PixelFormat::_420V };
            self.session = Some(Session::new(format, pixel_format)?);
            self.params = Some((sps.to_vec(), pps.to_vec()));
        }
        let Some(session) = &self.session else { return Ok(None) };
        let key = nals.iter().any(|n| kind(n) == 5);
        if self.waiting_for_key && !key {
            return Ok(None);
        }
        self.packed.clear();
        for n in nals.iter().filter(|n| !matches!(kind(n), 7..=9)) {
            push_nal(&mut self.packed, n);
        }
        if self.packed.is_empty() {
            return Ok(None);
        }
        match session.decode(&self.packed) {
            Ok(pb) => {
                if key {
                    self.waiting_for_key = false;
                }
                Ok(Some(pb))
            }
            Err(e) => {
                self.waiting_for_key = true;
                Err(e)
            }
        }
    }
}

/// Why an H.265 access unit gave no picture.
pub(crate) enum HevcError {
    /// No key frame yet (or resyncing after a failure)
    MissingKeyframe,
    /// This frame failed; the decoder now waits for a key frame
    Frame(String),
}

/// H.265 → BGRA, repairing slices that reference pictures lost on the way (like NVDEC's concealment):
/// without it, one lost frame stops the picture until the next key frame.
pub(crate) struct VtHevc {
    session: Option<Session>,
    params: Option<hevc::ParameterSets>,
    headers: Option<(hevc::SpsInfo, hevc::PpsInfo)>,
    /// POC lsbs believed to be in the decoder's picture buffer
    decoded: Vec<u32>,
    waiting_for_key: bool,
    packed: Vec<u8>,
    repairs: u32,
    repairs_logged: Option<std::time::Instant>,
}

impl VtHevc {
    pub fn new() -> Self {
        Self {
            session: None,
            params: None,
            headers: None,
            decoded: Vec::new(),
            waiting_for_key: true,
            packed: Vec::new(),
            repairs: 0,
            repairs_logged: None,
        }
    }

    pub fn reset(&mut self) {
        self.session = None;
        self.params = None;
        self.headers = None;
        self.decoded.clear();
        self.waiting_for_key = true;
    }

    pub fn decode(&mut self, annex_b: &[u8]) -> Result<PixelBuf, HevcError> {
        let nals = hevc::annex_b_nal_units(annex_b);
        if let Some(params) = hevc::parameter_sets_from_nals(&nals)
            && self.params.as_ref() != Some(&params)
        {
            let pointers = [params.vps.as_ptr(), params.sps.as_ptr(), params.pps.as_ptr()];
            let sizes = [params.vps.len(), params.sps.len(), params.pps.len()];
            let format = cm::VideoFormatDesc::with_hevc_param_sets(3, &pointers, &sizes, 4, None)
                .map_err(|e| HevcError::Frame(format!("HEVC format description: {e:?}")))?;
            let session = Session::new(format, cv::PixelFormat::_32_BGRA).map_err(HevcError::Frame)?;
            self.headers = hevc::parse_sps(&params.sps)
                .and_then(|sps| Ok((sps, hevc::parse_pps(&params.pps)?)))
                .inspect_err(|e| log::warn!("HEVC parameter sets not understood, no reference repair: {e:#}"))
                .ok();
            self.session = Some(session);
            self.params = Some(params);
            self.decoded.clear();
        }
        let Some(session) = &self.session else { return Err(HevcError::MissingKeyframe) };
        let key = hevc::nals_contain_keyframe(&nals);
        if self.waiting_for_key && !key {
            return Err(HevcError::MissingKeyframe);
        }
        // only the first slice's header is rewritten, so repair single-slice pictures only
        let single_slice = nals.iter().filter(|n| hevc::nal_unit_type(n).is_some_and(|t| t <= 21)).count() <= 1;
        let mut dpb_after: Option<Vec<u32>> = None;
        self.packed.clear();
        for &nal in &nals {
            if hevc::is_parameter_set(nal) {
                continue;
            }
            let mut repaired = None;
            if let Some((sps, pps)) = &self.headers
                && let Ok(info) = hevc::parse_slice(nal, sps, pps)
            {
                let poc_max = 1i64 << sps.poc_lsb_bits;
                let poc = i64::from(info.poc_lsb.unwrap_or(0));
                let (entries, missing_used) = conceal_rps(&info.rps, poc, poc_max, &self.decoded);
                let repairable = !missing_used || entries.iter().any(|e| e.used);
                if single_slice
                    && entries != info.rps
                    && repairable
                    && let Ok(rewritten) = hevc::rewrite_rps(nal, sps, pps, &entries)
                {
                    self.repairs += 1;
                    repaired = Some(rewritten);
                }
                if dpb_after.is_none() {
                    let rps = if repaired.is_some() { &entries } else { &info.rps };
                    let mut dpb: Vec<u32> = rps.iter().map(|e| ref_poc_lsb(poc, e.delta_poc, poc_max)).collect();
                    dpb.push(poc as u32);
                    dpb_after = Some(dpb);
                }
            }
            push_nal(&mut self.packed, repaired.as_deref().unwrap_or(nal));
        }
        if self.repairs > 0 && self.repairs_logged.is_none_or(|t| t.elapsed() >= std::time::Duration::from_secs(10)) {
            log::warn!("HEVC: repaired {} frame(s) that referenced lost pictures", self.repairs);
            self.repairs = 0;
            self.repairs_logged = Some(std::time::Instant::now());
        }
        match session.decode(&self.packed) {
            Ok(pb) => {
                if key {
                    self.waiting_for_key = false;
                }
                if self.headers.is_some() {
                    self.decoded = dpb_after.unwrap_or_default();
                }
                Ok(pb)
            }
            Err(e) => {
                self.waiting_for_key = true;
                Err(HevcError::Frame(e))
            }
        }
    }
}

/// POC lsb of the picture `delta` away from `poc`, wrapping at `poc_max`.
fn ref_poc_lsb(poc: i64, delta: i32, poc_max: i64) -> u32 {
    (poc + i64::from(delta)).rem_euclid(poc_max) as u32
}

/// The reference set to submit instead of `rps`, given the pictures in the decoder's buffer, and whether a
/// used reference was missing. Missing used references are pointed at the newest picture still held.
fn conceal_rps(rps: &[hevc::RpsEntry], poc: i64, poc_max: i64, decoded: &[u32]) -> (Vec<hevc::RpsEntry>, bool) {
    let mut entries = Vec::with_capacity(rps.len());
    let mut missing_used = false;
    for e in rps {
        if decoded.contains(&ref_poc_lsb(poc, e.delta_poc, poc_max)) {
            entries.push(*e);
        } else if e.used {
            missing_used = true;
        }
    }
    if missing_used
        && let Some(delta_poc) = decoded
            .iter()
            .map(|&lsb| (poc - i64::from(lsb)).rem_euclid(poc_max))
            .filter(|&d| d > 0)
            .min()
            .map(|d| -d as i32)
    {
        match entries.iter_mut().find(|e| e.delta_poc == delta_poc) {
            Some(e) => e.used = true,
            None => entries.push(hevc::RpsEntry { delta_poc, used: true }),
        }
    }
    (entries, missing_used)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_annex_b() {
        let data = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 9, 9];
        let nals = nal_units(&data);
        assert_eq!(nals, vec![&[0x67, 1, 2][..], &[0x68, 3][..], &[0x65, 9, 9][..]]);
    }

    #[test]
    fn lost_reference_is_pointed_at_the_newest_held_picture() {
        let rps = [hevc::RpsEntry { delta_poc: -1, used: true }, hevc::RpsEntry { delta_poc: -2, used: false }];
        // picture 9 was lost; 8 is held
        let (entries, missing) = conceal_rps(&rps, 10, 256, &[8]);
        assert!(missing);
        assert_eq!(entries, vec![hevc::RpsEntry { delta_poc: -2, used: true }]);
    }
}
