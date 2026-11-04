//! Text recognition on a cropped frame.
//!
//! macOS uses the Vision framework (Korean + English, accurate mode, no language correction).
//! Other platforms are not supported yet.

use psconnector::Frame;

/// Crops `region` = [x, y, w, h] (frame pixels) from the frame and scales it by `scale` (bilinear).
pub fn crop_scale(f: &Frame, region: [u32; 4], scale: f32) -> (usize, usize, Vec<u8>) {
    let [x, y, w, h] = region.map(|v| v as usize);
    let x = x.min(f.width.saturating_sub(1));
    let y = y.min(f.height.saturating_sub(1));
    let w = w.clamp(1, f.width - x);
    let h = h.clamp(1, f.height - y);
    let scale = scale.clamp(0.1, 8.0);
    let ow = ((w as f32 * scale).round() as usize).max(1);
    let oh = ((h as f32 * scale).round() as usize).max(1);
    let mut out = vec![0u8; ow * oh * 3];
    let px = |sx: usize, sy: usize, c: usize| f.rgb[((y + sy) * f.width + x + sx) * 3 + c] as f32;
    for oy in 0..oh {
        let fy = ((oy as f32 + 0.5) / scale - 0.5).clamp(0.0, (h - 1) as f32);
        let (y0, ty) = (fy.floor() as usize, fy.fract());
        let y1 = (y0 + 1).min(h - 1);
        for ox in 0..ow {
            let fx = ((ox as f32 + 0.5) / scale - 0.5).clamp(0.0, (w - 1) as f32);
            let (x0, tx) = (fx.floor() as usize, fx.fract());
            let x1 = (x0 + 1).min(w - 1);
            for c in 0..3 {
                let top = px(x0, y0, c) * (1.0 - tx) + px(x1, y0, c) * tx;
                let bottom = px(x0, y1, c) * (1.0 - tx) + px(x1, y1, c) * tx;
                out[(oy * ow + ox) * 3 + c] = (top * (1.0 - ty) + bottom * ty).round() as u8;
            }
        }
    }
    (ow, oh, out)
}

/// Recognized lines of text in a PNG image.
#[cfg(target_os = "macos")]
pub fn read_text(png: &[u8]) -> Result<Vec<String>, String> {
    use objc2::AnyThread;
    use objc2_foundation::{NSArray, NSData, NSDictionary, NSString};
    use objc2_vision::{VNImageRequestHandler, VNRecognizeTextRequest, VNRequest, VNRequestTextRecognitionLevel};

    objc2::rc::autoreleasepool(|_| {
        let data = NSData::with_bytes(png);
        let options = NSDictionary::new();
        let handler = VNImageRequestHandler::initWithData_options(VNImageRequestHandler::alloc(), &data, &options);
        let req = VNRecognizeTextRequest::new();
        req.setRecognitionLevel(VNRequestTextRecognitionLevel::Accurate);
        req.setRecognitionLanguages(&NSArray::from_retained_slice(&[
            NSString::from_str("ko-KR"),
            NSString::from_str("en-US"),
        ]));
        req.setUsesLanguageCorrection(false);
        let base: &VNRequest = &req;
        handler.performRequests_error(&NSArray::from_slice(&[base])).map_err(|e| e.to_string())?;
        let mut lines = Vec::new();
        if let Some(results) = req.results() {
            for obs in results.iter() {
                if let Some(text) = obs.topCandidates(1).firstObject() {
                    lines.push(text.string().to_string());
                }
            }
        }
        Ok(lines)
    })
}

#[cfg(not(target_os = "macos"))]
pub fn read_text(_png: &[u8]) -> Result<Vec<String>, String> {
    Err("OCR is only supported on macOS for now".into())
}
