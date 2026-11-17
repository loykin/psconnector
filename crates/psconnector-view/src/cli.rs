//! Command line.

use std::path::PathBuf;

use anyhow::{Result, anyhow, bail};
use psconnector_protocol::RunOptions;

use crate::quality::Quality;

pub(crate) enum Mode {
    /// `--login-window`: the PSN sign-in web view, run as its own process
    LoginWindow,
    /// `--replay FILE [--repeat N]`: play a recording against a running window (a macro process)
    Replay {
        path: PathBuf,
        repeat: Option<u32>,
    },
    Window(Args),
}

/// Choices for the window, from the command line.
pub(crate) struct Args {
    pub(crate) host: Option<String>,
    pub(crate) port: u16,
    /// Empty: the default macro folder
    pub(crate) macro_sources: Vec<PathBuf>,
    /// `--run MACRO`, with `--for` / `--after` / `--at` in the same shape as the macro_run command
    pub(crate) autorun: Option<String>,
    pub(crate) run_opts: RunOptions,
    pub(crate) run_at: Option<String>,
    pub(crate) use_pad: bool,
    pub(crate) use_audio: bool,
    pub(crate) start_muted: bool,
    pub(crate) quality: Option<Quality>,
    pub(crate) bitrate_kbps: Option<u32>,
}

/// Home folder (`%USERPROFILE%` on Windows).
pub(crate) fn home() -> PathBuf {
    std::env::home_dir().unwrap_or_default()
}

pub(crate) fn expand(p: &str) -> PathBuf {
    match p.strip_prefix("~/").or_else(|| p.strip_prefix("~\\")) {
        Some(rest) => home().join(rest),
        None => PathBuf::from(p),
    }
}

pub(crate) fn parse(args: impl IntoIterator<Item = String>) -> Result<Mode> {
    let all: Vec<String> = args.into_iter().collect();
    match all.first().map(String::as_str) {
        Some("--login-window") => return Ok(Mode::LoginWindow),
        Some("--replay") => {
            let path = PathBuf::from(all.get(1).map_or("recording.json", String::as_str));
            let repeat = match all.get(2).map(String::as_str) {
                Some("--repeat") => all.get(3).and_then(|n| n.parse().ok()),
                _ => None,
            };
            return Ok(Mode::Replay { path, repeat });
        }
        _ => {}
    }
    let mut a = Args {
        host: None,
        port: psconnector_protocol::DEFAULT_PORT,
        macro_sources: Vec::new(),
        autorun: None,
        run_opts: RunOptions::default(),
        run_at: None,
        use_pad: true,
        use_audio: true,
        start_muted: false,
        quality: None,
        bitrate_kbps: None,
    };
    let mut args = all.into_iter();
    while let Some(arg) = args.next() {
        let mut value = |what: &str| args.next().ok_or_else(|| anyhow!("{what}"));
        match arg.as_str() {
            "--port" => {
                a.port = value("missing value for --port")?.parse().map_err(|_| anyhow!("missing value for --port"))?
            }
            "--macros" => a.macro_sources.push(expand(&value("missing value for --macros")?)),
            "--run" => a.autorun = Some(value("missing value for --run")?),
            "--for" => {
                let min: f64 = value("--for needs minutes")?.parse().map_err(|_| anyhow!("--for needs minutes"))?;
                a.run_opts.limit_min = Some(min);
            }
            "--after" => a.run_opts.after = Some(value("--after needs none|standby")?),
            "--at" => a.run_at = Some(value("--at needs HH:MM or +MINUTES")?),
            "--no-pad" => a.use_pad = false,
            "--no-audio" => a.use_audio = false,
            "--mute" => a.start_muted = true,
            "--quality" => {
                let v = args.next().unwrap_or_default();
                a.quality = Some(Quality::parse(&v).ok_or_else(|| anyhow!("--quality must be low, 720p or 1080p"))?);
            }
            "--bitrate" => {
                let kbps = value("--bitrate needs kbit/s")?.parse().map_err(|_| anyhow!("--bitrate needs kbit/s"))?;
                a.bitrate_kbps = Some(kbps);
            }
            _ => a.host = Some(arg),
        }
    }
    if a.autorun.is_none() && (a.run_opts != RunOptions::default() || a.run_at.is_some()) {
        bail!("--for, --after and --at go with --run MACRO");
    }
    Ok(Mode::Window(a))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(args: &[&str]) -> Result<Args> {
        match parse(args.iter().map(|s| s.to_string()))? {
            Mode::Window(a) => Ok(a),
            _ => panic!("not a window"),
        }
    }

    #[test]
    fn window_options() {
        let a = window(&["192.0.2.10", "--port", "9000", "--run", "lap", "--for", "30", "--after", "standby"]).unwrap();
        assert_eq!(a.host.as_deref(), Some("192.0.2.10"));
        assert_eq!(a.port, 9000);
        assert_eq!(a.autorun.as_deref(), Some("lap"));
        assert_eq!(a.run_opts.limit_min, Some(30.0));
        assert_eq!(a.run_opts.after.as_deref(), Some("standby"));
        assert!(window(&["--for", "30"]).is_err());
        assert!(window(&["--quality", "4k"]).is_err());
        assert!(window(&["--port"]).is_err());
    }

    #[test]
    fn replay_mode() {
        let Mode::Replay { path, repeat } = parse(["--replay", "r.json", "--repeat", "3"].map(String::from)).unwrap()
        else {
            panic!()
        };
        assert_eq!(path, PathBuf::from("r.json"));
        assert_eq!(repeat, Some(3));
    }
}
