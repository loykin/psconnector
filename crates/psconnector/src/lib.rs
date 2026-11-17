//! psconnector: a PS5 Remote Play client library.
//!
//! Scope: console status and wake-up, session setup, controller input (buttons and sticks), rest mode,
//! video (H.264 → RGB) and audio (Opus → 16-bit PCM, when `Options::audio` is on).
//!
//! ```no_run
//! # async fn demo() -> psconnector::Result<()> {
//! use psconnector::{Button, Client};
//! let client = Client::connect("192.0.2.10", Default::default()).await?;
//! client.tap(Button::Cross, std::time::Duration::from_millis(80)).await;
//! client.close().await;
//! # Ok(()) }
//! ```

mod account;
mod audio;
mod controller;
mod crypto;
mod error;
mod net;
mod stream;
mod video;

// Public: finding consoles and waking them, PSN login, PIN registration, registration data
pub use account::{profile, psn, register};
pub use net::ddp;

// Internals (protocol, crypto, packets). Only the needed types are re-exported below.
use net::session;

use std::path::PathBuf;
use std::time::{Duration, Instant};

pub use error::{Error, Result};
use tokio::sync::{mpsc, watch};

pub use audio::Audio;
pub use controller::{Button, TOUCHPAD_SIZE};
use std::sync::Arc;
pub use stream::{Phase, StreamConfig};
pub use video::VideoRange;
pub use video::{Frame, PixelFormat, hdr_sdr_curve};

/// Video stream requested from the PS5.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum VideoCodec {
    #[default]
    H264,
    H265,
    H265Hdr,
}

impl VideoCodec {
    pub const ALL: [VideoCodec; 3] = [VideoCodec::H264, VideoCodec::H265, VideoCodec::H265Hdr];

    /// Short name used on the command line and the command port.
    pub const fn name(self) -> &'static str {
        match self {
            Self::H264 => "h264",
            Self::H265 => "h265",
            Self::H265Hdr => "h265-hdr",
        }
    }

    /// Reads [`name`](Self::name) and a few aliases (avc, hevc, hdr), ignoring case.
    pub fn parse(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "h264" | "avc" => Some(Self::H264),
            "h265" | "hevc" => Some(Self::H265),
            "h265-hdr" | "hevc-hdr" | "hdr" => Some(Self::H265Hdr),
            _ => None,
        }
    }

    pub(crate) const fn streaming_type(self) -> u32 {
        match self {
            Self::H264 => 1,
            Self::H265 => 2,
            Self::H265Hdr => 3,
        }
    }

    pub(crate) const fn launch_values(self) -> (&'static str, &'static str) {
        match self {
            Self::H264 => ("avc", "SDR"),
            Self::H265 => ("hevc", "SDR"),
            Self::H265Hdr => ("hevc", "HDR"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Options {
    /// Registration file (default: ~/.psconnector/profile.json, then pyremoteplay's ~/.pyremoteplay/.profile.json)
    pub profile: Option<PathBuf>,
    /// Registered PSN user name (default: the first user registered with that console)
    pub user: Option<String>,
    pub stream: StreamConfig,
    /// Whether to receive and decode video (when off, only input is used and the lowest quality is requested)
    pub video: bool,
    /// Whether to decode audio. Take the decoded chunks with [`Client::take_audio`].
    pub audio: bool,
    /// YUV sample range. Auto follows H.264 metadata; overrides help streams with incorrect VUI flags.
    pub video_range: VideoRange,
    /// H.264 SDR, H.265 SDR, or H.265 HDR (PS5 only).
    pub video_codec: VideoCodec,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            profile: None,
            user: None,
            stream: StreamConfig::default(),
            video: true,
            audio: false,
            video_range: VideoRange::Auto,
            video_codec: VideoCodec::H264,
        }
    }
}

/// A connected Remote Play session. Input goes through a channel to the stream task, so it is safe to call
/// from many places. It can be shared between tasks as `Arc<Client>`.
pub struct Client {
    cmd: mpsc::UnboundedSender<stream::Cmd>,
    ctrl: mpsc::UnboundedSender<session::CtrlCmd>,
    phase: watch::Receiver<Phase>,
    frame: watch::Receiver<Option<Arc<Frame>>>,
    audio: std::sync::Mutex<Option<std::sync::mpsc::Receiver<Audio>>>,
    stream_task: std::sync::Mutex<Option<tokio::task::JoinHandle<Result<()>>>>,
    ctrl_task: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    video_range: Arc<std::sync::atomic::AtomicU8>,
}

impl Client {
    /// Connects to the console and waits until the stream is ready.
    pub async fn connect(host: &str, opts: Options) -> Result<Client> {
        let st = ddp::status(host).await?;
        if !st.is_on() {
            return Err(Error::NotOn { code: st.code });
        }
        let host_id = st.host_id().ok_or_else(|| error::protocol("console response has no host-id"))?;
        let reg = match &opts.profile {
            Some(path) => profile::load(path, opts.user.as_deref(), host_id)?,
            None => profile::load_any(opts.user.as_deref(), host_id)?,
        };

        if opts.video_codec != VideoCodec::H264 && !video::HEVC_AVAILABLE {
            return Err(Error::Unsupported("H.265 decoding is not available on this platform".into()));
        }
        let started = session::start(host, &reg, opts.video_codec).await?;
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (phase_tx, mut phase_rx) = watch::channel(Phase::Connecting);
        let (frame_tx, frame_rx) = watch::channel(None);
        let video_range = Arc::new(std::sync::atomic::AtomicU8::new(opts.video_range.atomic()));
        let (video_tx, cfg) = if opts.video {
            let (tx, rx) = std::sync::mpsc::channel();
            let need_key = Arc::new(std::sync::atomic::AtomicBool::new(false));
            video::spawn_decoder(rx, frame_tx, need_key.clone(), video_range.clone(), opts.video_codec);
            (Some((tx, need_key)), opts.stream)
        } else {
            (None, StreamConfig::minimal())
        };
        // decoded audio waits here for at most ~0.5 s; past that it is dropped rather than piling up
        let (audio_tx, audio_rx) = if opts.audio {
            let (tx, rx) = std::sync::mpsc::channel();
            let (out_tx, out_rx) = std::sync::mpsc::sync_channel(50);
            audio::spawn_decoder(rx, out_tx);
            (Some(tx), Some(out_rx))
        } else {
            (None, None)
        };
        let stream_task = tokio::spawn(stream::run(
            host.to_string(),
            started.session_id,
            started.launch_cipher,
            cfg,
            cmd_rx,
            phase_tx,
            stream::Outputs { video: video_tx, audio: audio_tx, video_codec: opts.video_codec },
        ));
        let phase =
            match tokio::time::timeout(Duration::from_secs(15), phase_rx.wait_for(|p| *p != Phase::Connecting)).await {
                Ok(Ok(p)) => p.clone(),
                Ok(Err(_)) => Phase::Closed("stream task ended".into()),
                Err(_) => Phase::Closed("timed out waiting for the stream".into()),
            };
        if let Phase::Closed(why) = phase {
            stream_task.abort();
            started.ctrl_task.abort();
            return Err(Error::Closed(format!("stream setup failed: {why}")));
        }
        Ok(Client {
            cmd: cmd_tx,
            ctrl: started.ctrl,
            phase: phase_rx,
            frame: frame_rx,
            audio: std::sync::Mutex::new(audio_rx),
            stream_task: std::sync::Mutex::new(Some(stream_task)),
            ctrl_task: std::sync::Mutex::new(Some(started.ctrl_task)),
            video_range,
        })
    }

    pub fn phase(&self) -> Phase {
        self.phase.borrow().clone()
    }

    pub fn is_ready(&self) -> bool {
        self.phase() == Phase::Ready
    }

    /// The most recently decoded frame (when video is on).
    pub fn frame(&self) -> Option<Arc<Frame>> {
        self.frame.borrow().clone()
    }

    /// Waits for the next frame.
    pub async fn next_frame(&mut self) -> Option<Arc<Frame>> {
        self.frame.changed().await.ok()?;
        self.frame()
    }

    /// Returns another frame receiver (for receiving frames on other threads or tasks).
    pub fn frames(&self) -> watch::Receiver<Option<Arc<Frame>>> {
        self.frame.clone()
    }

    /// Changes sample-range interpretation for subsequent frames without reconnecting.
    pub fn set_video_range(&self, range: VideoRange) {
        self.video_range.store(range.atomic(), std::sync::atomic::Ordering::Relaxed);
    }

    /// Decoded audio, in 10 ms chunks (only with `Options::audio`). Can be taken once.
    pub fn take_audio(&self) -> Option<std::sync::mpsc::Receiver<Audio>> {
        self.audio.lock().unwrap().take()
    }

    pub fn press(&self, b: Button) {
        let _ = self.cmd.send(stream::Cmd::Button { button: b, pressed: true, queued_at: Instant::now() });
    }

    pub fn release(&self, b: Button) {
        let _ = self.cmd.send(stream::Cmd::Button { button: b, pressed: false, queued_at: Instant::now() });
    }

    /// Presses, waits `hold`, then releases.
    pub async fn tap(&self, b: Button, hold: Duration) {
        self.press(b);
        tokio::time::sleep(hold).await;
        self.release(b);
    }

    /// A finger on the touchpad: `id` tells fingers apart (a new touch should get a new id), `x`/`y` are in
    /// [`TOUCHPAD_SIZE`] coordinates. Send `down` with each new position while it moves, then `down = false`
    /// (with the last position) when it lifts.
    pub fn touch(&self, id: u8, down: bool, x: u16, y: u16) {
        let _ = self.cmd.send(stream::Cmd::Touch { id, down, x, y });
    }

    /// Stick: x and y are -1.0 to 1.0 (up is negative).
    pub fn stick(&self, right: bool, x: f32, y: f32) {
        let _ = self.cmd.send(stream::Cmd::Stick { right, x, y, queued_at: Instant::now() });
    }

    /// Puts the console into rest mode (this also ends the session).
    pub fn standby(&self) {
        let _ = self.ctrl.send(session::CtrlCmd::Standby);
    }

    /// Tells the console we are disconnecting and cleans up. Safe to call more than once.
    pub async fn close(&self) {
        let _ = self.cmd.send(stream::Cmd::Stop);
        let task = self.stream_task.lock().unwrap().take();
        if let Some(mut t) = task {
            match tokio::time::timeout(Duration::from_secs(2), &mut t).await {
                Ok(_) => {}
                Err(_) => {
                    log::warn!("stream task did not stop in time; aborting it");
                    t.abort();
                }
            }
        }
        let _ = self.ctrl.send(session::CtrlCmd::Close);
        let task = self.ctrl_task.lock().unwrap().take();
        if let Some(mut t) = task {
            match tokio::time::timeout(Duration::from_secs(1), &mut t).await {
                Ok(_) => {}
                Err(_) => {
                    log::warn!("control task did not stop in time; aborting it");
                    t.abort();
                }
            }
        }
    }
}
