//! Plays the console's audio on this computer's default output device.
//!
//! A feeder thread converts the decoded chunks to the device format and fills a small buffer; the device
//! callback drains it. How much to keep buffered adapts to the network: it starts at 120 ms, comes down
//! (to 30 ms) while the buffer never gets close to empty, and goes up by 40 ms whenever it runs dry. Around
//! that target, playback runs 0.5 % faster or slower to steer the buffer (a pitch change of under a tenth of a
//! semitone, not audible), so audio is never cut.
//! Only if it runs dry does the device play silence, and only far beyond the target is old audio dropped.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample};
use psconnector::Audio;

/// Buffer target at the start, and its limits
const INITIAL_TARGET_MS: usize = 120;
const MIN_TARGET_MS: usize = 30;
const MAX_TARGET_MS: usize = 200;
/// After running dry, the target goes up by this much
const GROW_MS: usize = 40;
/// The target comes down at most this much per window, keeping this much in hand at the lowest point
const SHRINK_STEP_MS: usize = 10;
const KEEP_MS: usize = 15;
/// How long the buffer must stay comfortably full before the target comes down
const ADAPT_WINDOW: std::time::Duration = std::time::Duration::from_secs(3);
/// Drop the oldest audio beyond this much buffered (down to the target)
const MAX_MS: usize = 350;
/// If decoded PCM has backed up this far before conversion, discard old chunks down to the target. A chunk
/// is normally 10 ms. This is deliberately above normal network bursts (observed around 130 ms).
const MAX_INPUT_CHUNKS: usize = 35;
const TARGET_INPUT_CHUNKS: usize = 12;
/// Playback speed change used to steer the buffer
const NUDGE: f64 = 0.005;

struct Buffer {
    samples: VecDeque<f32>,
    /// Current buffer target (adapts, see the module docs)
    target_ms: usize,
    /// Lowest level while playing in the current adaptation window, and when that window began
    window_min: usize,
    window_start: std::time::Instant,
    /// Sum and count of buffer levels seen by the device callback (average latency for the report)
    level_sum: u64,
    level_n: u64,
    /// Waiting for three quarters of the target before playing (at start and after running dry)
    priming: bool,
    /// Times it ran dry (audible gaps) and times old audio was dropped, since the last report
    gaps: u32,
    trims: u32,
    input_dropped: u32,
    min_len: usize,
    max_len: usize,
}

/// Taps on each side of the interpolation point (32 in all)
const HALF_TAPS: usize = 16;
/// Fractional positions the filter is tabulated at (linear between neighbours)
const PHASES: usize = 256;

/// A windowed-sinc (Kaiser) low-pass, tabulated at `PHASES + 1` fractional offsets. `cutoff` is relative
/// to the input's Nyquist frequency.
struct Kernel {
    cutoff: f64,
    /// `(PHASES + 1) × 2·HALF_TAPS` weights; row p is for a fraction of p / PHASES, tap j for input
    /// frame `i - HALF_TAPS + 1 + j`
    rows: Vec<f32>,
}

impl Kernel {
    fn new(cutoff: f64) -> Self {
        const BETA: f64 = 8.0;
        let bessel_i0 = |x: f64| {
            let (mut sum, mut term) = (1.0, 1.0);
            for k in 1..32 {
                term *= (x / 2.0 / k as f64).powi(2);
                sum += term;
            }
            sum
        };
        let taps = 2 * HALF_TAPS;
        let mut rows = Vec::with_capacity((PHASES + 1) * taps);
        for p in 0..=PHASES {
            let frac = p as f64 / PHASES as f64;
            let row: Vec<f64> = (0..taps)
                .map(|j| {
                    let x = j as f64 - (HALF_TAPS - 1) as f64 - frac;
                    let sinc = if x.abs() < 1e-12 {
                        cutoff
                    } else {
                        (std::f64::consts::PI * cutoff * x).sin() / (std::f64::consts::PI * x)
                    };
                    let w = (x / HALF_TAPS as f64).clamp(-1.0, 1.0);
                    sinc * bessel_i0(BETA * (1.0 - w * w).sqrt()) / bessel_i0(BETA)
                })
                .collect();
            // unity gain at DC, so a constant stays constant
            let sum: f64 = row.iter().sum();
            rows.extend(row.iter().map(|v| (v / sum) as f32));
        }
        Self { cutoff, rows }
    }

    /// Weights for a fractional position, interpolated between the two nearest tabulated rows.
    fn weights(&self, frac: f64, out: &mut [f32; 2 * HALF_TAPS]) {
        let at = frac * PHASES as f64;
        let p = (at.floor() as usize).min(PHASES - 1);
        let t = (at - p as f64) as f32;
        let taps = 2 * HALF_TAPS;
        let (a, b) = (&self.rows[p * taps..(p + 1) * taps], &self.rows[(p + 1) * taps..(p + 2) * taps]);
        for j in 0..taps {
            out[j] = a[j] + (b[j] - a[j]) * t;
        }
    }
}

/// Converts 16-bit interleaved chunks at any rate (and playback speed) to f32 at the device rate and
/// channel count. Extra device channels get silence, a mono device gets the average.
///
/// Positions between input samples are interpolated with a 32-tap windowed sinc, which keeps the treble
/// (linear interpolation dulls it: −5 dB at 16 kHz halfway between samples). At the same rate and normal
/// speed on a whole-sample position, samples pass through untouched.
struct Converter {
    out_rate: u32,
    out_channels: usize,
    /// Position in `hist`, in input frames
    pos: f64,
    /// Playback speed (1 = normal)
    speed: f64,
    /// Recent input frames, interleaved: what the filter still needs plus everything not played yet
    hist: Vec<f32>,
    channels: usize,
    kernel: Option<Kernel>,
}

impl Converter {
    fn new(out_rate: u32, out_channels: usize) -> Self {
        Self { out_rate, out_channels, pos: 0.0, speed: 1.0, hist: Vec::new(), channels: 0, kernel: None }
    }

    /// Starts over (after old audio was thrown away): nothing is interpolated across the cut.
    fn reset(&mut self) {
        self.pos = 0.0;
        self.hist.clear();
    }

    fn push(&mut self, a: &Audio, out: &mut VecDeque<f32>) {
        let ch = usize::from(a.channels.max(1));
        if a.samples.len() < ch {
            return;
        }
        if ch != self.channels {
            self.channels = ch;
            self.reset();
        }
        let ratio = f64::from(a.rate) / f64::from(self.out_rate);
        // below the output's Nyquist when downsampling; a little under the input's otherwise
        let cutoff = (1.0 / ratio).min(1.0) * 0.95;
        if self.kernel.as_ref().is_none_or(|k| (k.cutoff - cutoff).abs() > 1e-9) {
            self.kernel = Some(Kernel::new(cutoff));
        }
        let kernel = self.kernel.as_ref().expect("kernel");
        let whole = a.samples.len() / ch * ch;
        self.hist.extend(a.samples[..whole].iter().map(|&s| f32::from(s) / 32768.0));
        let frames = self.hist.len() / ch;
        let step = ratio * self.speed;
        let hist = &self.hist;
        // frames before the start repeat the first one (no fade-in from silence)
        let sample = |i: isize, c: usize| hist[i.clamp(0, frames as isize - 1) as usize * ch + c];
        let mut w = [0f32; 2 * HALF_TAPS];
        let mut frame = vec![0f32; ch];
        loop {
            let i = self.pos.floor() as isize;
            let frac = self.pos - i as f64;
            let exact = frac < 1e-9 && (step - 1.0).abs() < 1e-12;
            // the filter looks HALF_TAPS frames ahead
            let need = if exact { i } else { i + HALF_TAPS as isize };
            if need >= frames as isize {
                break;
            }
            if exact {
                for (c, v) in frame.iter_mut().enumerate() {
                    *v = sample(i, c);
                }
            } else {
                kernel.weights(frac, &mut w);
                let first = i - HALF_TAPS as isize + 1;
                for (c, v) in frame.iter_mut().enumerate() {
                    *v = w.iter().enumerate().map(|(j, wj)| wj * sample(first + j as isize, c)).sum();
                }
            }
            match self.out_channels {
                1 => out.push_back(frame.iter().sum::<f32>() / ch as f32),
                n => {
                    for c in 0..n {
                        out.push_back(if c < ch {
                            frame[c]
                        } else if ch == 1 {
                            frame[0]
                        } else {
                            0.0
                        });
                    }
                }
            }
            self.pos += step;
        }
        // keep what the filter still looks back at
        let drop = (self.pos.floor() as isize - HALF_TAPS as isize).clamp(0, frames as isize) as usize;
        if drop > 0 {
            self.hist.drain(..drop * ch);
            self.pos -= drop as f64;
        }
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
        target_ms: INITIAL_TARGET_MS,
        window_min: usize::MAX,
        window_start: std::time::Instant::now(),
        level_sum: 0,
        level_n: 0,
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
            let mut conv = Converter::new(rate, channels);
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
                    conv.reset();
                }
                for chunk in chunks {
                    // Read only the steering state while locked. Resampling and temporary allocation must not
                    // make the real-time device callback wait for this mutex.
                    let (held, priming, target) = {
                        let mut b = buf.lock().unwrap();
                        adapt(&mut b, per_ms);
                        (b.samples.len() / per_ms, b.priming, b.target_ms)
                    };
                    let (high, low) = (target + target / 2, target.saturating_sub(target / 2).max(MIN_TARGET_MS / 2));
                    conv.speed = if held > high {
                        1.0 + NUDGE
                    } else if held < low && !priming {
                        1.0 - NUDGE
                    } else {
                        1.0
                    };
                    conv.push(&chunk, &mut converted);
                    let mut b = buf.lock().unwrap();
                    b.samples.append(&mut converted);
                    b.max_len = b.max_len.max(b.samples.len());
                    let max = MAX_MS.max(b.target_ms * 3) * per_ms;
                    if b.samples.len() > max {
                        let extra = b.samples.len() - b.target_ms * per_ms;
                        let extra = extra - extra % channels;
                        b.samples.drain(..extra);
                        b.trims += 1;
                    }
                }
                let mut b = buf.lock().unwrap();
                b.input_dropped = b.input_dropped.saturating_add(dropped as u32);
                if report.elapsed() >= std::time::Duration::from_secs(10) {
                    let avg = b.level_sum.checked_div(b.level_n).unwrap_or(0) as usize / per_ms;
                    log::info!(
                        "audio: {} gaps, {} output trims, {} input chunks dropped in 10 s, buffer {}..{} ms (avg {} ms, target {} ms)",
                        b.gaps,
                        b.trims,
                        b.input_dropped,
                        if b.min_len == usize::MAX { 0 } else { b.min_len / per_ms },
                        b.max_len / per_ms,
                        avg,
                        b.target_ms
                    );
                    (b.gaps, b.trims, b.input_dropped, b.min_len, b.max_len) = (0, 0, 0, usize::MAX, 0);
                    (b.level_sum, b.level_n) = (0, 0);
                    report = std::time::Instant::now();
                }
            }
        })
        .ok();
    true
}

/// Diagnostics: PSCONNECTOR_AUDIO_FIXED=1 keeps the 120 ms target (the old behaviour) for comparison.
fn fixed_target() -> bool {
    static FIXED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FIXED.get_or_init(|| std::env::var_os("PSCONNECTOR_AUDIO_FIXED").is_some())
}

/// Brings the target down while the buffer has stayed comfortably above empty for a whole window.
fn adapt(b: &mut Buffer, per_ms: usize) {
    if fixed_target() || b.priming || b.window_start.elapsed() < ADAPT_WINDOW {
        return;
    }
    if b.window_min != usize::MAX {
        let lowest_ms = b.window_min / per_ms;
        if lowest_ms > KEEP_MS {
            let down = (lowest_ms - KEEP_MS).min(SHRINK_STEP_MS);
            b.target_ms = b.target_ms.saturating_sub(down).max(MIN_TARGET_MS);
        }
    }
    b.window_min = usize::MAX;
    b.window_start = std::time::Instant::now();
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
            // start once three quarters of the target is in, so playback begins near the target
            if b.priming && b.samples.len() >= (b.target_ms * 3 / 4).max(KEEP_MS) * per_ms {
                b.priming = false;
            }
            let mute = muted.load(Ordering::Relaxed);
            for slot in data.iter_mut() {
                let v = if b.priming { 0.0 } else { b.samples.pop_front().unwrap_or(0.0) };
                *slot = T::from_sample(if mute { 0.0 } else { v });
            }
            if !b.priming {
                let len = b.samples.len();
                b.min_len = b.min_len.min(len);
                b.window_min = b.window_min.min(len);
                b.level_sum += len as u64;
                b.level_n += 1;
            }
            if !b.priming && b.samples.is_empty() {
                b.priming = true;
                b.gaps += 1;
                // ran dry: the network needs more cushion
                if !fixed_target() {
                    b.target_ms = (b.target_ms + GROW_MS).min(MAX_TARGET_MS);
                }
                b.window_min = usize::MAX;
                b.window_start = std::time::Instant::now();
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

    fn buffer(target: usize) -> Buffer {
        Buffer {
            samples: VecDeque::new(),
            target_ms: target,
            window_min: usize::MAX,
            window_start: std::time::Instant::now() - ADAPT_WINDOW,
            level_sum: 0,
            level_n: 0,
            priming: false,
            gaps: 0,
            trims: 0,
            min_len: usize::MAX,
            max_len: 0,
            input_dropped: 0,
        }
    }

    #[test]
    fn target_shrinks_with_spare_buffer_and_stops_near_the_floor() {
        let per_ms = 96; // 48 kHz stereo
        let mut b = buffer(120);
        b.window_min = 90 * per_ms; // never below 90 ms in the last window
        adapt(&mut b, per_ms);
        assert_eq!(b.target_ms, 110, "comes down one step");
        for _ in 0..20 {
            b.window_min = 40 * per_ms;
            b.window_start = std::time::Instant::now() - ADAPT_WINDOW;
            adapt(&mut b, per_ms);
        }
        assert_eq!(b.target_ms, MIN_TARGET_MS, "floor");
        // a window that came close to empty keeps the target
        let mut b = buffer(80);
        b.window_min = 10 * per_ms;
        adapt(&mut b, per_ms);
        assert_eq!(b.target_ms, 80);
    }

    #[test]
    fn same_rate_passes_through() {
        let mut c = Converter::new(48_000, 2);
        let mut out = VecDeque::new();
        let a: Vec<i16> = (0..960).map(|i| (i * 10) as i16).collect();
        c.push(&chunk(48_000, a.clone()), &mut out);
        c.push(&chunk(48_000, a), &mut out);
        assert_eq!(out.len(), 1920, "no samples gained or lost across chunks");
        assert!((out[2] - 20.0 / 32768.0).abs() < 1e-6);
    }

    #[test]
    fn resamples_to_44_1_khz_and_mono() {
        let mut c = Converter::new(44_100, 1);
        let mut out = VecDeque::new();
        for _ in 0..100 {
            c.push(&chunk(48_000, vec![1000; 960]), &mut out);
        }
        // 1 s of input → about 44100 mono samples
        // (less the filter's look-ahead of 16 input frames)
        assert!((44_070..=44_110).contains(&out.len()), "{}", out.len());
        assert!(out.iter().all(|v| (v - 1000.0 / 32768.0).abs() < 1e-4));
    }

    /// Level in dB of a sine played through the converter, after a stretch at nudged speed has left the
    /// position between samples.
    fn level_after_nudge(f: f64) -> f64 {
        let tone = |n: usize, start: usize| -> Vec<i16> {
            (0..n)
                .flat_map(|i| {
                    let v = ((2.0 * std::f64::consts::PI * f * (start + i) as f64 / 48_000.0).sin() * 16_000.0) as i16;
                    [v, v]
                })
                .collect()
        };
        let mut c = Converter::new(48_000, 2);
        let mut out = VecDeque::new();
        c.speed = 1.0 + NUDGE;
        c.push(&chunk(48_000, tone(480, 0)), &mut out);
        c.speed = 1.0;
        let mut start = 480;
        for _ in 0..20 {
            c.push(&chunk(48_000, tone(480, start)), &mut out);
            start += 480;
        }
        let tail: Vec<f32> = out.iter().skip(2 * 2000).step_by(2).copied().collect();
        let rms =
            (tail.iter().map(|v| (f64::from(*v) * 32768.0 / 16_000.0).powi(2)).sum::<f64>() / tail.len() as f64).sqrt();
        20.0 * (rms * 2f64.sqrt()).log10()
    }

    #[test]
    fn treble_survives_a_speed_nudge() {
        // linear interpolation lost 5.5 dB at 16 kHz and 9.8 dB at 20 kHz here
        for f in [1_000.0, 8_000.0, 16_000.0, 20_000.0] {
            let db = level_after_nudge(f);
            assert!(db.abs() < 0.3, "{f} Hz: {db:.2} dB");
        }
    }
}
