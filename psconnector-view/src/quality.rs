//! Picture quality presets. The resolution is agreed with the console when connecting, so changing it
//! reconnects. The last choice is remembered in the settings file.

use psconnector::StreamConfig;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Quality {
    /// 360p 30 fps: enough for screen checks, lightest to decode
    Low,
    /// 720p 60 fps
    Hd720,
    /// 1080p 60 fps: for playing yourself
    Hd1080,
}

impl Quality {
    pub const ALL: [Quality; 3] = [Quality::Low, Quality::Hd720, Quality::Hd1080];

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "low" | "360p" | "macro" => Some(Self::Low),
            "720p" | "720" | "hd" => Some(Self::Hd720),
            "1080p" | "1080" | "fhd" | "high" => Some(Self::Hd1080),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Hd720 => "720p",
            Self::Hd1080 => "1080p",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Low => "360p (macros)",
            Self::Hd720 => "720p",
            Self::Hd1080 => "1080p",
        }
    }

    /// Stream settings for this preset; `bitrate_kbps` overrides the preset's bit rate.
    pub fn stream(self, bitrate_kbps: Option<u32>) -> StreamConfig {
        let (width, height, fps, kbps) = match self {
            Self::Low => (640, 360, 30, 2_000),
            Self::Hd720 => (1280, 720, 60, 10_000),
            Self::Hd1080 => (1920, 1080, 60, 15_000),
        };
        StreamConfig { width, height, fps, bitrate_kbps: bitrate_kbps.unwrap_or(kbps) }
    }
}

/// Quality saved from the last run, if any.
pub fn saved() -> Option<Quality> {
    Quality::parse(crate::settings::get("quality")?.as_str()?)
}

/// Remembers the quality for the next run.
pub fn save(q: Quality) {
    crate::settings::set("quality", q.name().into());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets() {
        assert_eq!(Quality::parse("1080P"), Some(Quality::Hd1080));
        assert_eq!(Quality::parse("macro"), Some(Quality::Low));
        assert_eq!(Quality::parse("4k"), None);
        let s = Quality::Hd1080.stream(None);
        assert_eq!((s.width, s.height, s.fps, s.bitrate_kbps), (1920, 1080, 60, 15_000));
        assert_eq!(Quality::Hd720.stream(Some(20_000)).bitrate_kbps, 20_000);
        for q in Quality::ALL {
            assert_eq!(Quality::parse(q.name()), Some(q));
        }
    }
}
