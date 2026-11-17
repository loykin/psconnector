//! Switches for testing loss recovery against a real console: dropping video units on purpose and dumping
//! them to a file. Both are off unless their environment variable is set.

use log::warn;

use crate::video;

/// Share of video units to drop on purpose (`PSCONNECTOR_DROP_VIDEO`), for testing loss recovery.
pub(super) fn drop_rate() -> Option<f64> {
    static RATE: std::sync::OnceLock<Option<f64>> = std::sync::OnceLock::new();
    *RATE.get_or_init(|| {
        let r = std::env::var("PSCONNECTOR_DROP_VIDEO").ok()?.parse::<f64>().ok().filter(|r| *r > 0.0)?;
        warn!("dropping {:.1} % of video units on purpose (PSCONNECTOR_DROP_VIDEO)", r * 100.0);
        Some(r)
    })
}

/// Diagnostics: PSCONNECTOR_DUMP_VIDEO=file writes the first 4000 decrypted video units (all of them,
/// FEC units included) as lines "frame unit src total hex", to check loss recovery against real data.
pub(super) fn dump_unit(p: &video::VideoPacket, data: &[u8]) {
    use std::io::Write;
    static DUMP: std::sync::OnceLock<Option<std::sync::Mutex<(std::fs::File, u32)>>> = std::sync::OnceLock::new();
    let Some(dump) = DUMP.get_or_init(|| {
        let path = std::env::var("PSCONNECTOR_DUMP_VIDEO").ok()?;
        std::fs::File::create(path).ok().map(|f| std::sync::Mutex::new((f, 0)))
    }) else {
        return;
    };
    let mut d = dump.lock().unwrap();
    if d.1 >= 4000 {
        return;
    }
    d.1 += 1;
    let hex: String = data.iter().map(|b| format!("{b:02x}")).collect();
    let _ = writeln!(d.0, "{} {} {} {} {hex}", p.frame, p.unit, p.src_units(), p.total);
}

/// Small deterministic generator for the drop test.
pub(super) struct XorShift(pub(super) u64);

impl XorShift {
    pub(super) fn next(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }
}
