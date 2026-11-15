//! H.265 decoding (VideoToolbox, macOS hardware).

#[allow(unused_imports)]
use super::*;

#[cfg(target_os = "macos")]
pub(crate) fn spawn_hevc_decoder(
    rx: std::sync::mpsc::Receiver<(u16, std::time::Instant, Vec<u8>)>,
    out: watch::Sender<Option<Arc<Frame>>>,
    need_key: Arc<std::sync::atomic::AtomicBool>,
    hdr: bool,
) -> std::thread::JoinHandle<()> {
    use super::vt::{HevcError, VtHevc};

    std::thread::Builder::new()
        .name("psconnector-hevc".into())
        .spawn(move || {
            let mut decoder = VtHevc::new();
            let mut stats = DecodeStats::default();
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
                    Ok(pb) if !behind => {
                        let t1 = std::time::Instant::now();
                        let (width, height) = (pb.width(), pb.height());
                        // hand the decoder's buffer over as it is: the window maps HDR to SDR on the GPU
                        let format = if hdr { PixelFormat::BgraHdr8 } else { PixelFormat::Bgra8 };
                        if !announced {
                            log::info!(
                                "HEVC decoded by VideoToolbox: {}x{} BGRA ({})",
                                width,
                                height,
                                if hdr { "HDR mapped to SDR display" } else { "SDR" }
                            );
                            announced = true;
                        }
                        out.send_replace(Some(Arc::new(Frame::native(pb, format, index))));
                        stats.shown += 1;
                        stats.convert += t1.elapsed();
                    }
                    Ok(_) => stats.skipped += u32::from(behind),
                    Err(HevcError::MissingKeyframe) => {
                        need_key.store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                    Err(HevcError::Frame(e)) => {
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
pub(crate) fn spawn_hevc_decoder(
    _rx: std::sync::mpsc::Receiver<(u16, std::time::Instant, Vec<u8>)>,
    _out: watch::Sender<Option<Arc<Frame>>>,
    _need_key: Arc<std::sync::atomic::AtomicBool>,
    _hdr: bool,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(|| warn!("H.265 decoding is not available on this platform"))
}
