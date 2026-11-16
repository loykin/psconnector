//! The connection to the console while the window is open: shared state, start/stop, reconnect, supervisor.

#[allow(unused_imports)]
use super::*;

pub(crate) struct Shared {
    /// Current connection; replaced when the viewer reconnects (quality change)
    pub(crate) client: std::sync::RwLock<Arc<Client>>,
    pub(crate) host: String,
    pub(crate) quality: Mutex<Quality>,
    pub(crate) video_codec: Mutex<VideoCodec>,
    pub(crate) video_range: Mutex<VideoRange>,
    pub(crate) bitrate_kbps: Option<u32>,
    pub(crate) use_audio: bool,
    /// Where each connection's audio is forwarded to (None when sound is off)
    pub(crate) audio_out: Option<std::sync::mpsc::Sender<Audio>>,
    /// Set while reconnecting; shown in the window
    pub(crate) reconnecting: Mutex<Option<String>>,
    pub(crate) rt: tokio::runtime::Handle,
    pub(crate) runner: Arc<Mutex<Runner>>,
    pub(crate) osd: Mutex<String>,
    /// Name of the game controller connected to this computer, if any
    pub(crate) pad: Arc<Mutex<Option<String>>>,
    pub(crate) recorder: Arc<record::Recorder>,
    /// Finger ids for swipes sent through the command port
    pub(crate) touch_ids: std::sync::atomic::AtomicU32,
    /// Sound is playing (false with --no-audio or without an output device)
    pub(crate) sound: bool,
    pub(crate) muted: Arc<AtomicBool>,
    pub(crate) playing: AtomicBool,
    pub(crate) stop_play: AtomicBool,
    /// The window's egui context, to wake it up from other threads
    pub(crate) ctx: std::sync::OnceLock<egui::Context>,
    /// Disconnect was pressed: go back to the start screen
    pub(crate) leave: AtomicBool,
    /// The session is over; its threads and tasks stop
    pub(crate) ended: AtomicBool,
    pub(crate) serve_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Picture adjustment for the window, and a counter bumped on every change
    pub(crate) picture: Mutex<picture::Picture>,
    pub(crate) picture_version: std::sync::atomic::AtomicU64,
}

impl Shared {
    /// Changes the picture adjustment for this session. Named presets are persisted separately;
    /// a new session always starts from the untouched picture.
    pub(crate) fn set_picture(&self, p: picture::Picture) {
        *self.picture.lock().unwrap() = p;
        self.picture_version.fetch_add(1, Ordering::SeqCst);
        if let Some(ctx) = self.ctx.get() {
            ctx.request_repaint();
        }
    }

    pub(crate) fn set_video_range(&self, range: VideoRange) {
        *self.video_range.lock().unwrap() = range;
        self.client().set_video_range(range);
    }

    pub(crate) fn client(&self) -> Arc<Client> {
        self.client.read().unwrap().clone()
    }

    pub(crate) fn release_all(&self) {
        let c = self.client();
        for b in Button::ALL {
            c.release(b);
        }
        c.stick(false, 0.0, 0.0);
        c.stick(true, 0.0, 0.0);
    }

    pub(crate) fn options(&self, q: Quality, codec: VideoCodec) -> Options {
        Options {
            audio: self.use_audio,
            stream: q.stream(self.bitrate_kbps),
            video_range: *self.video_range.lock().unwrap(),
            video_codec: codec,
            ..Options::default()
        }
    }

    /// Sends this connection's decoded audio on to the sound output.
    pub(crate) fn attach_audio(&self, c: &Client) {
        if let (Some(out), Some(rx)) = (self.audio_out.clone(), c.take_audio()) {
            std::thread::spawn(move || {
                while let Ok(a) = rx.recv() {
                    if out.send(a).is_err() {
                        break;
                    }
                }
            });
        }
    }

    /// Reconnects at another quality (no-op if already there).
    pub(crate) fn reconnect(self: &Arc<Self>, q: Quality, codec: VideoCodec) -> Result<(), String> {
        if *self.quality.lock().unwrap() == q && *self.video_codec.lock().unwrap() == codec {
            return Ok(());
        }
        {
            let mut r = self.reconnecting.lock().unwrap();
            if r.as_deref().is_some_and(|n| n.starts_with("reconnecting")) {
                return Err("already reconnecting".into());
            }
            *r = Some(format!("reconnecting at {} / {}...", q.label(), video_codec_name(codec)));
        }
        let sh = self.clone();
        self.rt.spawn(async move {
            sh.release_all();
            let old_q = *sh.quality.lock().unwrap();
            let old_codec = *sh.video_codec.lock().unwrap();
            sh.client().close().await;
            // Give the console time to release the old streaming slot. Reconnecting immediately can be accepted
            // while the previous stream is still draining, which produces long packet bursts and audio gaps.
            tokio::time::sleep(Duration::from_secs(3)).await;
            let result = match connect(&sh.host, sh.options(q, codec)).await {
                Ok(c) => Ok((c, q, codec)),
                Err(e) => {
                    log::warn!(
                        "reconnect at {} / {} failed: {e:#}; going back to {} / {}",
                        q.label(),
                        video_codec_name(codec),
                        old_q.label(),
                        video_codec_name(old_codec)
                    );
                    connect(&sh.host, sh.options(old_q, old_codec)).await.map(|c| (c, old_q, old_codec))
                }
            };
            match result {
                Ok((c, got, got_codec)) => {
                    sh.attach_audio(&c);
                    *sh.client.write().unwrap() = Arc::new(c);
                    *sh.quality.lock().unwrap() = got;
                    *sh.video_codec.lock().unwrap() = got_codec;
                    quality::save(got);
                    log::info!("connected at {} / {}", got.label(), video_codec_name(got_codec));
                    *sh.reconnecting.lock().unwrap() = None;
                }
                Err(e) => *sh.reconnecting.lock().unwrap() = Some(format!("reconnect failed: {e:#}")),
            }
        });
        Ok(())
    }

    pub(crate) fn set_quality(self: &Arc<Self>, q: Quality) -> Result<(), String> {
        let codec = *self.video_codec.lock().unwrap();
        self.reconnect(q, codec)
    }

    pub(crate) fn set_video_codec(self: &Arc<Self>, codec: VideoCodec) -> Result<(), String> {
        let quality = *self.quality.lock().unwrap();
        self.reconnect(quality, codec)
    }
}

/// Everything that runs once a console is connected: sound, pad, command port, macros.
pub(crate) fn start_session(
    rt: &tokio::runtime::Handle,
    l: &Launch,
    out: &Outputs,
    host: String,
    client: Client,
) -> Result<(Arc<Shared>, Arc<Mutex<Runner>>)> {
    let client = Arc::new(client);
    let runner = Arc::new(Mutex::new(Runner::new(load_macros(&l.macro_sources), l.macro_sources.clone(), l.port)));
    let shared = Arc::new(Shared {
        client: std::sync::RwLock::new(client.clone()),
        host,
        quality: Mutex::new(l.quality),
        video_codec: Mutex::new(l.video_codec),
        video_range: Mutex::new(l.video_range),
        bitrate_kbps: l.bitrate_kbps,
        use_audio: l.use_audio,
        audio_out: out.audio.clone(),
        reconnecting: Mutex::new(None),
        rt: rt.clone(),
        runner: runner.clone(),
        osd: Mutex::new(String::new()),
        pad: out.pad.clone(),
        recorder: out.recorder.clone(),
        touch_ids: std::sync::atomic::AtomicU32::new(0),
        sound: out.audio.is_some(),
        muted: out.muted.clone(),
        playing: AtomicBool::new(false),
        stop_play: AtomicBool::new(false),
        ctx: std::sync::OnceLock::new(),
        leave: AtomicBool::new(false),
        ended: AtomicBool::new(false),
        serve_task: Mutex::new(None),
        picture: Mutex::new(picture::Picture::default()),
        picture_version: std::sync::atomic::AtomicU64::new(1),
    });
    shared.attach_audio(&client);
    let listener = rt
        .block_on(TcpListener::bind(("127.0.0.1", l.port)))
        .with_context(|| format!("cannot open the command port {}", l.port))?;
    *shared.serve_task.lock().unwrap() = Some(rt.spawn(serve(listener, shared.clone())));
    spawn_supervisor(shared.clone(), rt.clone());
    println!("connected. command port {}. press Q in the window to quit", l.port);
    if let Some(name) = &l.autorun {
        let msg = runner.lock().unwrap().run_named(name, &Value::Object(l.run_opts.clone()), l.run_at.as_deref());
        match msg {
            Ok(m) => println!("--run {name}: {m}"),
            Err(e) => println!("--run {name}: {e}"),
        }
    }
    Ok((shared, runner))
}

/// Ends a session (Disconnect): stops its macro, frees the command port, and closes the connection.
pub(crate) fn end_session(rt: &tokio::runtime::Handle, sh: &Arc<Shared>) {
    sh.runner.lock().unwrap().shutdown();
    // a recording in progress is dropped with the connection it was recording
    sh.recorder.stop();
    sh.stop_play.store(true, Ordering::SeqCst);
    sh.release_all();
    sh.ended.store(true, Ordering::SeqCst);
    if let Some(t) = sh.serve_task.lock().unwrap().take() {
        t.abort();
    }
    let c = sh.client();
    rt.spawn(async move { c.close().await });
    println!("disconnected");
}

/// Runs macros' schedules and time limits, and handles a quit signal, off the window thread:
/// macOS stops repainting a hidden or minimized window, and these must not wait for it.
pub(crate) fn spawn_supervisor(sh: Arc<Shared>, rt: tokio::runtime::Handle) {
    std::thread::Builder::new()
        .name("supervisor".into())
        .spawn(move || {
            let mut quit_since: Option<Instant> = None;
            while !sh.ended.load(Ordering::SeqCst) {
                let ended = sh.runner.lock().unwrap().poll();
                if let Some(ended) = ended {
                    // however the macro ended, release any input left pressed
                    sh.release_all();
                    sh.stop_play.store(true, Ordering::SeqCst);
                    if ended.standby {
                        sh.client().standby();
                    }
                }
                if QUIT.load(Ordering::SeqCst)
                    && let Some(ctx) = sh.ctx.get()
                {
                    // the window closes on its next frame; make sure there is one
                    ctx.request_repaint();
                    // a window macOS keeps asleep may never draw it: close the session here instead
                    if quit_since.get_or_insert_with(Instant::now).elapsed() > Duration::from_secs(3) {
                        sh.runner.lock().unwrap().shutdown();
                        sh.release_all();
                        rt.block_on(sh.client().close());
                        println!("closed");
                        std::process::exit(0);
                    }
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        })
        .expect("supervisor thread");
}

/// Wakes egui as soon as the decoder publishes a frame. The outer loop follows a replacement Client after
/// a quality change; the slow periodic repaint in Viewer remains for status text and other non-frame state.
pub(crate) fn spawn_frame_repaint(sh: Arc<Shared>) {
    std::thread::Builder::new()
        .name("frame-repaint".into())
        .spawn(move || {
            while !sh.ended.load(Ordering::SeqCst) {
                let client = sh.client();
                let mut frames = client.frames();
                while !sh.ended.load(Ordering::SeqCst) && sh.rt.block_on(frames.changed()).is_ok() {
                    if let Some(ctx) = sh.ctx.get() {
                        ctx.request_repaint();
                    }
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        })
        .expect("frame repaint thread");
}

/// Waits for Ctrl+C (and SIGTERM on Unix).
pub(crate) async fn wait_for_quit_signal() {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("SIGTERM");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}

/// The console keeps reporting "another session connected" (and sometimes refuses the connection) for a
/// while after the previous session ends. It is usually quick, but has taken over 90 seconds in practice,
/// so keep retrying long enough that a quality change does not fall back just before the console is ready.
pub(crate) async fn connect(host: &str, opts: Options) -> Result<Client> {
    let t0 = Instant::now();
    loop {
        match Client::connect(host, opts.clone()).await {
            Ok(c) => return Ok(c),
            Err(e) if t0.elapsed() < Duration::from_secs(180) && is_transient(&e) => {
                println!("previous session still active ({e}), retrying in 3 s");
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
            Err(e) => return Err(e.into()),
        }
    }
}

pub(crate) fn is_transient(e: &psconnector::Error) -> bool {
    use std::io::ErrorKind::{ConnectionRefused, ConnectionReset};
    match e {
        psconnector::Error::Busy => true,
        // a console that has just woken up can be slow to finish the session setup
        psconnector::Error::Timeout(_) | psconnector::Error::NoResponse { .. } => true,
        psconnector::Error::Io(io) => {
            matches!(io.kind(), ConnectionRefused | ConnectionReset | std::io::ErrorKind::TimedOut)
        }
        _ => false,
    }
}
