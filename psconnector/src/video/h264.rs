//! H.264 decoding: VideoToolbox (hardware) on macOS, openh264 (software) elsewhere or with
//! PSCONNECTOR_H264=software.

#[allow(unused_imports)]
use super::*;

pub(crate) fn spawn_h264_decoder(
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
            #[cfg(target_os = "macos")]
            let mut vt = super::vt::VtH264::new();
            #[cfg(target_os = "macos")]
            let mut use_vt = std::env::var("PSCONNECTOR_H264").as_deref() != Ok("software");
            #[cfg(target_os = "macos")]
            let (mut was_hw, mut vt_frames, mut vt_errors) = (false, 0u64, 0u64);
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
                #[cfg(target_os = "macos")]
                {
                    let hw = use_vt;
                    if hw != was_hw {
                        // the other decoder has no reference frames yet
                        need_key.store(true, std::sync::atomic::Ordering::SeqCst);
                        log::info!("H.264 decoded by {}", if hw { "VideoToolbox (hardware)" } else { "openh264 (software)" });
                        was_hw = hw;
                    }
                    if hw {
                        let queued = queued_at.elapsed();
                        let t0 = std::time::Instant::now();
                        let decoded = vt.decode(&h264);
                        let t_decode = t0.elapsed();
                        let waiting = rx.try_recv();
                        let behind = waiting.is_ok();
                        match decoded {
                            Ok(Some(pb)) => {
                                failing = 0;
                                vt_frames += 1;
                                if behind {
                                    stats.skipped += 1;
                                } else {
                                    let t1 = std::time::Instant::now();
                                    let format = match pb.pixel_format() {
                                        cidre::cv::PixelFormat::_32_BGRA => PixelFormat::Bgra8,
                                        f => {
                                            // the samples are as coded; a range picked by hand only changes how
                                            // the window reads them, so it stays on the hardware path
                                            let coded_full = f == cidre::cv::PixelFormat::_420F;
                                            let full_range = match VideoRange::from_atomic(
                                                range.load(std::sync::atomic::Ordering::Relaxed),
                                            ) {
                                                VideoRange::Auto => coded_full,
                                                VideoRange::Limited => false,
                                                VideoRange::Full => true,
                                            };
                                            let bt709 = color.unwrap_or_default().matrix == Matrix::Bt709;
                                            PixelFormat::Nv12 { full_range, bt709 }
                                        }
                                    };
                                    out.send_replace(Some(Arc::new(Frame::native(pb, format, index))));
                                    stats.shown += 1;
                                    stats.convert += t1.elapsed();
                                }
                            }
                            Ok(None) => failing += 1,
                            Err(e) => {
                                failing += 1;
                                vt_errors += 1;
                                if vt_errors <= 3 || vt_errors.is_multiple_of(100) {
                                    warn!("VideoToolbox H.264: {e}");
                                }
                                if vt_frames == 0 && vt_errors >= 120 {
                                    warn!("VideoToolbox cannot decode this stream; using openh264");
                                    use_vt = false;
                                }
                            }
                        }
                        if failing > 0 && failing.is_multiple_of(10) {
                            need_key.store(true, std::sync::atomic::Ordering::SeqCst);
                        }
                        if failing >= 60 {
                            log::info!("video decoder stuck after a damaged frame; starting a fresh one");
                            vt.reset();
                            failing = 0;
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
                        continue;
                    }
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
                            rgb = frame.into_data();
                        }
                        rgb.resize(w * h * 3, 0);
                        let mut spec = color.unwrap_or_default();
                        match VideoRange::from_atomic(range.load(std::sync::atomic::Ordering::Relaxed)) {
                            VideoRange::Auto => {}
                            VideoRange::Limited => spec.full_range = false,
                            VideoRange::Full => spec.full_range = true,
                        }
                        write_rgb8(&yuv, spec, &mut rgb);
                        let frame = Arc::new(Frame::new(w, h, PixelFormat::Rgb8, std::mem::take(&mut rgb), index));
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
