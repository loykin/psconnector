//! Input latency counters, logged every 10 s: how long button and stick commands wait in the queue and
//! take until their UDP packet is sent.

use std::time::Duration;

use log::info;

#[derive(Default)]
struct Latencies(Vec<u64>);

impl Latencies {
    fn record(&mut self, elapsed: Duration) {
        self.0.push(elapsed.as_micros().min(u128::from(u64::MAX)) as u64);
    }

    fn summary(&mut self) -> Option<String> {
        if self.0.is_empty() {
            return None;
        }
        self.0.sort_unstable();
        let at = |percent: usize| {
            let index = (self.0.len() * percent).div_ceil(100).saturating_sub(1);
            self.0[index] as f64 / 1000.0
        };
        let summary =
            format!("n={} p50={:.2} p95={:.2} p99={:.2} max={:.2} ms", self.0.len(), at(50), at(95), at(99), at(100));
        self.0.clear();
        Some(summary)
    }
}

pub(super) struct InputStats {
    since: std::time::Instant,
    button_queue: Latencies,
    button_total: Latencies,
    stick_queue: Latencies,
    stick_total: Latencies,
    stick_events: u32,
    stick_packets: u32,
}

impl Default for InputStats {
    fn default() -> Self {
        Self {
            since: std::time::Instant::now(),
            button_queue: Latencies::default(),
            button_total: Latencies::default(),
            stick_queue: Latencies::default(),
            stick_total: Latencies::default(),
            stick_events: 0,
            stick_packets: 0,
        }
    }
}

impl InputStats {
    pub(super) fn button(&mut self, queued_at: std::time::Instant, dequeued_at: std::time::Instant) {
        self.button_queue.record(dequeued_at.saturating_duration_since(queued_at));
        self.button_total.record(queued_at.elapsed());
    }

    pub(super) fn stick_event(&mut self, queued_at: std::time::Instant) {
        self.stick_events += 1;
        self.stick_queue.record(queued_at.elapsed());
    }

    pub(super) fn stick_packet(&mut self, latest_at: std::time::Instant) {
        self.stick_packets += 1;
        self.stick_total.record(latest_at.elapsed());
    }

    pub(super) fn report(&mut self) {
        let elapsed = self.since.elapsed();
        if elapsed < Duration::from_secs(10) {
            return;
        }
        let button_queue = self.button_queue.summary();
        let button_total = self.button_total.summary();
        let stick_queue = self.stick_queue.summary();
        let stick_total = self.stick_total.summary();
        if let (Some(queue), Some(total)) = (button_queue, button_total) {
            info!("input buttons over {:.0} s: queue [{queue}], API-to-UDP [{total}]", elapsed.as_secs_f64());
        }
        if let (Some(queue), Some(total)) = (stick_queue, stick_total) {
            info!(
                "input sticks over {:.0} s: {} events → {} packets, queue [{queue}], latest-to-UDP [{total}]",
                elapsed.as_secs_f64(),
                self.stick_events,
                self.stick_packets
            );
        }
        self.stick_events = 0;
        self.stick_packets = 0;
        self.since = std::time::Instant::now();
    }
}

/// When assembled video frames leave the receive loop, logged every 10 s: frames that come right after the
/// previous one (bunched) and the longest pause show whether frames already arrive unevenly here.
pub(super) struct ArrivalStats {
    since: std::time::Instant,
    last: Option<std::time::Instant>,
    frames: u32,
    bunched: u32,
    gaps: Latencies,
    slowest_packet: Duration,
}

impl Default for ArrivalStats {
    fn default() -> Self {
        Self {
            since: std::time::Instant::now(),
            last: None,
            frames: 0,
            bunched: 0,
            gaps: Latencies::default(),
            slowest_packet: Duration::ZERO,
        }
    }
}

impl ArrivalStats {
    pub(super) fn frame(&mut self, at: std::time::Instant) {
        if let Some(last) = self.last {
            let gap = at - last;
            self.bunched += u32::from(gap < Duration::from_millis(4));
            self.gaps.record(gap);
        }
        self.last = Some(at);
        self.frames += 1;
    }

    /// How long handling one received packet kept the receive loop busy.
    pub(super) fn packet(&mut self, took: Duration) {
        self.slowest_packet = self.slowest_packet.max(took);
    }

    pub(super) fn report(&mut self) {
        if self.since.elapsed() < Duration::from_secs(10) {
            return;
        }
        if let Some(gaps) = self.gaps.summary() {
            info!(
                "video arrival: {} frames, {} within 4 ms of the previous, gaps [{gaps}], slowest packet handling {:.2} ms",
                self.frames,
                self.bunched,
                self.slowest_packet.as_secs_f64() * 1000.0
            );
        }
        *self = Self { last: self.last, ..Self::default() };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latency_summary_reports_percentiles_and_clears_samples() {
        let mut values = Latencies::default();
        for ms in 1..=100 {
            values.record(Duration::from_millis(ms));
        }
        let summary = values.summary().unwrap();
        assert!(summary.contains("n=100"), "{summary}");
        assert!(summary.contains("p50=50.00"), "{summary}");
        assert!(summary.contains("p95=95.00"), "{summary}");
        assert!(summary.contains("p99=99.00"), "{summary}");
        assert!(summary.contains("max=100.00"), "{summary}");
        assert!(values.summary().is_none());
    }
}
