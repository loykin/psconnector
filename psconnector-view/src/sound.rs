//! Plays the console's audio on this computer's default output device.
//!
//! A feeder thread converts the decoded chunks to the device format and fills a small buffer; the device
//! callback drains it. Audio arrives in bursts (it shares the network with large video frames), so the
//! buffer aims at about 120 ms: when it holds more, playback runs 2 % faster until it is back; when it holds
//! less, 2 % slower. Only if it runs dry does the device play silence, and only far beyond the target is
//! old audio dropped.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample};
use psconnector::Audio;

/// Start playing once this much is buffered (and again after running dry)
const START_MS: usize = 80;
/// Buffer size to steer towards
const TARGET_MS: usize = 120;
/// Above this, play slightly faster; below LOW_MS, slightly slower
const HIGH_MS: usize = 180;
const LOW_MS: usize = 60;
/// Drop the oldest audio beyond this much buffered (down to TARGET_MS)
const MAX_MS: usize = 350;
/// If decoded PCM has backed up this far before conversion, discard old chunks down to the target. A chunk
/// is normally 10 ms. This is deliberately above normal network bursts (observed around 130 ms).
const MAX_INPUT_CHUNKS: usize = 35;
const TARGET_INPUT_CHUNKS: usize = 12;
/// Playback speed change used to steer the buffer
const NUDGE: f64 = 0.02;

struct Buffer {
    samples: VecDeque<f32>,
    /// Waiting for START_MS before playing (at start and after running dry)
    priming: bool,
    /// Times it ran dry (audible gaps) and times old audio was dropped, since the last report
    gaps: u32,
    trims: u32,
    input_dropped: u32,
    min_len: usize,
    max_len: usize,
}

/// Converts 16-bit interleaved chunks at any rate to f32 at the device rate and channel count
/// (linear interpolation; extra device channels get silence, a mono device gets the average).
struct Converter {
    out_rate: u32,
    out_channels: usize,
    /// Position in the input, in input frames, carried across chunks
    pos: f64,
    /// Playback speed (1 = normal)
    speed: f64,
    prev: Vec<f32>,
}

impl Converter {
    fn push(&mut self, a: &Audio, out: &mut VecDeque<f32>) {
        let ch = usize::from(a.channels.max(1));
        let frames: Vec<&[i16]> = a.samples.chunks_exact(ch).collect();
        if frames.is_empty() {
            return;
        }
        if self.prev.len() != ch {
            self.prev = vec![0.0; ch];
        }
        let step = f64::from(a.rate) / f64::from(self.out_rate) * self.speed;
        let sample = |i: isize, c: usize| -> f32 {
            if i < 0 { self.prev[c] } else { f32::from(frames[i as usize][c]) / 32768.0 }
        };
        // pos is relative to this chunk; -1 is the last frame of the previous chunk
        while self.pos < (frames.len() - 1) as f64 + 1e-9 {
            let i = self.pos.floor() as isize;
            let t = (self.pos - i as f64) as f32;
            let at = |c: usize| {
                let a = sample(i, c);
                let b = if i + 1 < frames.len() as isize { sample(i + 1, c) } else { a };
                a + (b - a) * t
            };
            match self.out_channels {
                1 => out.push_back((0..ch).map(at).sum::<f32>() / ch as f32),
                n => {
                    for c in 0..n {
                        out.push_back(if c < ch {
                            at(c)
                        } else if ch == 1 {
                            at(0)
                        } else {
                            0.0
                        });
                    }
                }
            }
            self.pos += step;
        }
        self.pos -= frames.len() as f64;
        let last: Vec<f32> = (0..ch).map(|c| sample(frames.len() as isize - 1, c)).collect();
        self.prev = last;
    }
}

/// Starts playback. Returns false when there is no usable output device.
pub fn spawn(rx: Receiver<Audio>, muted: Arc<AtomicBool>) -> bool {
    let host = cpal::default_host();
    let Some(device) = host.default_output_device() else {
        log::warn!("no audio output device; sound is off");
        return false;
    };
    let supported = match device.default_output_config() {
        Ok(c) => c,
        Err(e) => {
            log::warn!("audio output unavailable: {e}");
            return false;
        }
    };
    let config = supported.config();
    let (rate, channels) = (config.sample_rate, usize::from(config.channels));
    let per_ms = rate as usize * channels / 1000;
    let buf = Arc::new(Mutex::new(Buffer {
        samples: VecDeque::new(),
        priming: true,
        gaps: 0,
        trims: 0,
        input_dropped: 0,
        min_len: usize::MAX,
        max_len: 0,
    }));

    // the stream must stay on the thread that made it, so a thread owns it for the rest of the run
    let out = buf.clone();
    let format = supported.sample_format();
    std::thread::Builder::new()
        .name("audio-out".into())
        .spawn(move || {
            let stream = match format {
                SampleFormat::F32 => build::<f32>(&device, &config, out, muted, per_ms),
                SampleFormat::I16 => build::<i16>(&device, &config, out, muted, per_ms),
                SampleFormat::U16 => build::<u16>(&device, &config, out, muted, per_ms),
                SampleFormat::I32 => build::<i32>(&device, &config, out, muted, per_ms),
                other => {
                    log::warn!("unsupported audio sample format {other:?}; sound is off");
                    return;
                }
            };
            match stream {
                Ok(s) => {
                    if let Err(e) = s.play() {
                        log::warn!("cannot start audio output: {e}");
                        return;
                    }
                    log::info!("audio output: {rate} Hz, {channels} channels");
                    loop {
                        std::thread::park();
                    }
                }
                Err(e) => log::warn!("cannot open audio output: {e}"),
            }
        })
        .ok();

    std::thread::Builder::new()
        .name("audio-feed".into())
        .spawn(move || {
            let mut conv = Converter { out_rate: rate, out_channels: channels, pos: 0.0, speed: 1.0, prev: Vec::new() };
            let mut converted = VecDeque::new();
            let mut report = std::time::Instant::now();
            while let Ok(first) = rx.recv() {
                let mut chunks = vec![first];
                chunks.extend(rx.try_iter());
                let dropped = if chunks.len() > MAX_INPUT_CHUNKS {
                    let keep_from = chunks.len().saturating_sub(TARGET_INPUT_CHUNKS);
                    chunks.drain(..keep_from);
                    keep_from
                } else {
                    0
                };
                if dropped > 0 {
                    // Do not interpolate between samples that were separated by a deliberate latency trim.
                    conv.pos = 0.0;
                    conv.prev.clear();
                }
                for chunk in chunks {
                    // Read only the steering state while locked. Resampling and temporary allocation must not
                    // make the real-time device callback wait for this mutex.
                    let (held, priming) = {
                        let b = buf.lock().unwrap();
                        (b.samples.len() / per_ms, b.priming)
                    };
                    conv.speed = if held > HIGH_MS {
                        1.0 + NUDGE
                    } else if held < LOW_MS && !priming {
                        1.0 - NUDGE
                    } else {
                        1.0
                    };
                    conv.push(&chunk, &mut converted);
                    let mut b = buf.lock().unwrap();
                    b.samples.append(&mut converted);
                    b.max_len = b.max_len.max(b.samples.len());
                    let max = MAX_MS * per_ms;
                    if b.samples.len() > max {
                        let extra = b.samples.len() - TARGET_MS * per_ms;
                        let extra = extra - extra % channels;
                        b.samples.drain(..extra);
                        b.trims += 1;
                    }
                }
                let mut b = buf.lock().unwrap();
                b.input_dropped = b.input_dropped.saturating_add(dropped as u32);
                if report.elapsed() >= std::time::Duration::from_secs(10) {
                    if b.gaps > 0 || b.trims > 0 || b.input_dropped > 0 {
                        log::info!(
                            "audio: {} gaps, {} output trims, {} input chunks dropped in 10 s, buffer {}..{} ms",
                            b.gaps,
                            b.trims,
                            b.input_dropped,
                            if b.min_len == usize::MAX { 0 } else { b.min_len / per_ms },
                            b.max_len / per_ms
                        );
                    }
                    (b.gaps, b.trims, b.input_dropped, b.min_len, b.max_len) = (0, 0, 0, usize::MAX, 0);
                    report = std::time::Instant::now();
                }
            }
        })
        .ok();
    true
}

fn build<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    buf: Arc<Mutex<Buffer>>,
    muted: Arc<AtomicBool>,
    per_ms: usize,
) -> Result<cpal::Stream, cpal::Error>
where
    T: SizedSample + FromSample<f32>,
{
    device.build_output_stream(
        *config,
        move |data: &mut [T], _| {
            let mut b = buf.lock().unwrap();
            if b.priming && b.samples.len() >= START_MS * per_ms {
                b.priming = false;
            }
            let mute = muted.load(Ordering::Relaxed);
            for slot in data.iter_mut() {
                let v = if b.priming { 0.0 } else { b.samples.pop_front().unwrap_or(0.0) };
                *slot = T::from_sample(if mute { 0.0 } else { v });
            }
            if !b.priming {
                b.min_len = b.min_len.min(b.samples.len());
            }
            if !b.priming && b.samples.is_empty() {
                b.priming = true;
                b.gaps += 1;
            }
        },
        |e| log::warn!("audio output error: {e}"),
        None,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(rate: u32, samples: Vec<i16>) -> Audio {
        Audio { rate, channels: 2, samples }
    }

    #[test]
    fn same_rate_passes_through() {
        let mut c = Converter { out_rate: 48_000, out_channels: 2, pos: 0.0, speed: 1.0, prev: Vec::new() };
        let mut out = VecDeque::new();
        let a: Vec<i16> = (0..960).map(|i| (i * 10) as i16).collect();
        c.push(&chunk(48_000, a.clone()), &mut out);
        c.push(&chunk(48_000, a), &mut out);
        assert_eq!(out.len(), 1920, "no samples gained or lost across chunks");
        assert!((out[2] - 20.0 / 32768.0).abs() < 1e-6);
    }

    #[test]
    fn resamples_to_44_1_khz_and_mono() {
        let mut c = Converter { out_rate: 44_100, out_channels: 1, pos: 0.0, speed: 1.0, prev: Vec::new() };
        let mut out = VecDeque::new();
        for _ in 0..100 {
            c.push(&chunk(48_000, vec![1000; 960]), &mut out);
        }
        // 1 s of input → about 44100 mono samples
        assert!((44_090..=44_110).contains(&out.len()), "{}", out.len());
        assert!(out.iter().all(|v| (v - 1000.0 / 32768.0).abs() < 1e-4));
    }
}
