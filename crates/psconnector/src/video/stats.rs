//! Decoder timing and sample-level statistics, for the log.

use openh264::formats::YUVSource;

/// Decoding speed, logged every 10 s.
#[derive(Default)]
pub(crate) struct DecodeStats {
    pub(crate) since: Option<std::time::Instant>,
    pub(crate) decoded: u32,
    pub(crate) shown: u32,
    pub(crate) skipped: u32,
    pub(crate) queue: std::time::Duration,
    pub(crate) max_queue: std::time::Duration,
    pub(crate) decode: std::time::Duration,
    pub(crate) convert: std::time::Duration,
}

#[derive(Clone, Copy)]
pub(crate) struct PlaneLevels {
    pub(crate) min: u8,
    pub(crate) max: u8,
    pub(crate) below_limited: u64,
    pub(crate) above_limited: u64,
    pub(crate) at_zero: u64,
    pub(crate) at_full: u64,
    pub(crate) samples: u64,
}

impl PlaneLevels {
    pub(crate) fn new() -> Self {
        Self { min: u8::MAX, max: 0, below_limited: 0, above_limited: 0, at_zero: 0, at_full: 0, samples: 0 }
    }

    pub(crate) fn add(&mut self, value: u8, low: u8, high: u8) {
        self.min = self.min.min(value);
        self.max = self.max.max(value);
        self.below_limited += u64::from(value < low);
        self.above_limited += u64::from(value > high);
        self.at_zero += u64::from(value == 0);
        self.at_full += u64::from(value == u8::MAX);
        self.samples += 1;
    }

    pub(crate) fn outside_percent(self) -> f64 {
        100.0 * (self.below_limited + self.above_limited) as f64 / self.samples.max(1) as f64
    }
}

#[derive(Clone, Copy)]
pub(crate) struct YuvLevels {
    pub(crate) y: PlaneLevels,
    pub(crate) u: PlaneLevels,
    pub(crate) v: PlaneLevels,
}

impl YuvLevels {
    pub(crate) fn from_frame(yuv: &impl YUVSource) -> Self {
        let (width, height) = yuv.dimensions();
        let (y_stride, u_stride, v_stride) = yuv.strides();
        let mut levels = Self { y: PlaneLevels::new(), u: PlaneLevels::new(), v: PlaneLevels::new() };
        for row in 0..height {
            for &value in &yuv.y()[row * y_stride..row * y_stride + width] {
                levels.y.add(value, 16, 235);
            }
        }
        for row in 0..height / 2 {
            for &value in &yuv.u()[row * u_stride..row * u_stride + width / 2] {
                levels.u.add(value, 16, 240);
            }
            for &value in &yuv.v()[row * v_stride..row * v_stride + width / 2] {
                levels.v.add(value, 16, 240);
            }
        }
        levels
    }
}

impl DecodeStats {
    pub(crate) fn report(&mut self) {
        let since = *self.since.get_or_insert_with(std::time::Instant::now);
        if since.elapsed() < std::time::Duration::from_secs(10) {
            return;
        }
        let ms = |d: std::time::Duration, n: u32| d.as_secs_f64() * 1000.0 / f64::from(n.max(1));
        log::info!(
            "video: {} frames decoded ({:.1} ms each, queue {:.1} ms avg/{:.1} ms max), {} shown ({:.1} ms to RGB), {} skipped to catch up",
            self.decoded,
            ms(self.decode, self.decoded),
            ms(self.queue, self.decoded),
            self.max_queue.as_secs_f64() * 1000.0,
            self.shown,
            ms(self.convert, self.shown),
            self.skipped
        );
        *self = Self { since: Some(std::time::Instant::now()), ..Self::default() };
    }
}
