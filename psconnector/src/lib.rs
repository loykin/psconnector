//! psconnector: a PS5 Remote Play client library.
//!
//! Scope: console status and wake-up, session setup, controller input (buttons and sticks), rest mode,
//! and video (H.264 → RGB). Audio is received but discarded for now.
//!
//! ```no_run
//! # async fn demo() -> psconnector::Result<()> {
//! use psconnector::{Button, Client};
//! let client = Client::connect("192.168.1.20", Default::default()).await?;
//! client.tap(Button::Cross, std::time::Duration::from_millis(80)).await;
//! client.close().await;
//! # Ok(()) }
//! ```

// Public: console status and wake-up, reading registration data
pub mod ddp;
pub mod profile;

// Internals (protocol, crypto, packets). Only the needed types are re-exported below.
mod controller;
mod error;
mod gkcrypt;
mod keys;
mod proto;
mod rpcrypt;
mod session;
mod stream;
mod takion;
mod video;

#[cfg(test)]
mod crypto_tests;

use std::path::PathBuf;
use std::time::Duration;

pub use error::{Error, Result};
use tokio::sync::{mpsc, watch};

pub use controller::Button;
use std::sync::Arc;
pub use stream::{Phase, StreamConfig};
pub use video::Frame;

#[derive(Clone, Debug)]
pub struct Options {
    /// Registration file (default: ~/.pyremoteplay/.profile.json)
    pub profile: Option<PathBuf>,
    /// Registered PSN user name (default: the first user registered with that console)
    pub user: Option<String>,
    pub stream: StreamConfig,
    /// Whether to receive and decode video (when off, only input is used and the lowest quality is requested)
    pub video: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self { profile: None, user: None, stream: StreamConfig::default(), video: true }
    }
}

/// A connected Remote Play session. Input goes through a channel to the stream task, so it is safe to call
/// from many places. It can be shared between tasks as `Arc<Client>`.
pub struct Client {
    cmd: mpsc::UnboundedSender<stream::Cmd>,
    ctrl: mpsc::UnboundedSender<session::CtrlCmd>,
    phase: watch::Receiver<Phase>,
    frame: watch::Receiver<Option<Arc<Frame>>>,
    stream_task: std::sync::Mutex<Option<tokio::task::JoinHandle<Result<()>>>>,
    ctrl_task: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Client {
    /// Connects to the console and waits until the stream is ready.
    pub async fn connect(host: &str, opts: Options) -> Result<Client> {
        let st = ddp::status(host).await?;
        if !st.is_on() {
            return Err(Error::NotOn { code: st.code });
        }
        let host_id = st.host_id().ok_or_else(|| error::protocol("console response has no host-id"))?;
        let path = opts.profile.clone().unwrap_or_else(profile::default_path);
        let reg = profile::load(&path, opts.user.as_deref(), host_id)?;

        let started = session::start(host, &reg).await?;
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (phase_tx, mut phase_rx) = watch::channel(Phase::Connecting);
        let (frame_tx, frame_rx) = watch::channel(None);
        let (video_tx, cfg) = if opts.video {
            let (tx, rx) = std::sync::mpsc::channel();
            video::spawn_decoder(rx, frame_tx);
            (Some(tx), opts.stream)
        } else {
            (None, StreamConfig::minimal())
        };
        let stream_task = tokio::spawn(stream::run(
            host.to_string(),
            started.session_id,
            started.launch_cipher,
            cfg,
            cmd_rx,
            phase_tx,
            video_tx,
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
            stream_task: std::sync::Mutex::new(Some(stream_task)),
            ctrl_task: std::sync::Mutex::new(Some(started.ctrl_task)),
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

    pub fn press(&self, b: Button) {
        let _ = self.cmd.send(stream::Cmd::Button(b, true));
    }

    pub fn release(&self, b: Button) {
        let _ = self.cmd.send(stream::Cmd::Button(b, false));
    }

    /// Presses, waits `hold`, then releases.
    pub async fn tap(&self, b: Button, hold: Duration) {
        self.press(b);
        tokio::time::sleep(hold).await;
        self.release(b);
    }

    /// Stick: x and y are -1.0 to 1.0 (up is negative).
    pub fn stick(&self, right: bool, x: f32, y: f32) {
        let _ = self.cmd.send(stream::Cmd::Stick { right, x, y });
    }

    /// Puts the console into rest mode (this also ends the session).
    pub fn standby(&self) {
        let _ = self.ctrl.send(session::CtrlCmd::Standby);
    }

    /// Tells the console we are disconnecting and cleans up. Safe to call more than once.
    pub async fn close(&self) {
        let _ = self.cmd.send(stream::Cmd::Stop);
        let task = self.stream_task.lock().unwrap().take();
        if let Some(t) = task {
            let _ = tokio::time::timeout(Duration::from_secs(2), t).await;
        }
        let _ = self.ctrl.send(session::CtrlCmd::Close);
        let task = self.ctrl_task.lock().unwrap().take();
        if let Some(t) = task {
            let _ = tokio::time::timeout(Duration::from_secs(1), t).await;
        }
    }
}
