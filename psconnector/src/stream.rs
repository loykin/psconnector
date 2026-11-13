//! Takion stream (UDP 9296).
//!
//! **Only this task writes to the socket.** Button and stick commands arrive over a channel and are sent
//! in order. In pyremoteplay the stick thread and button calls shared key_pos without a lock, so concurrent
//! sends could make the PS5 decrypt garbage (e.g. a PS button press nobody made).

use std::time::Duration;

use crate::error::{Error, Result};
use base64::{Engine, engine::general_purpose::STANDARD as B64};
use log::{debug, info, warn};
use prost::Message;
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, watch};
use tokio::time::{Instant, MissedTickBehavior, interval, sleep_until};

use crate::VideoCodec;
use crate::audio::{self, AudioConfig, AudioMsg, AudioTx, Reorder};
use crate::controller::{Button, EventHistory, Sticks};
use crate::gkcrypt::{Ecdh, StreamCipher};
use crate::proto::{self, PayloadType, TakionMessage};
use crate::rpcrypt::SessionCipher;
use crate::takion::{self, chunk, header};
use crate::video::{self, Assembled, Assembler};

pub const PORT: u16 = 9296;
const TAG_LOCAL: u32 = 1;
const DATA_ACK_LENGTH: usize = 29;
/// The stick state is re-sent at this interval even when unchanged (as chiaki does).
const STATE_INTERVAL: Duration = Duration::from_millis(200);
/// UDP receive buffer for the stream (the macOS default is about 0.8 MB)
const RECV_BUFFER: usize = 4 << 20;
const CONGESTION_INTERVAL: Duration = Duration::from_millis(500);
/// Delay before re-sending a button event packet unchanged. Guards against losing the last event
/// (usually a release).
const EVENT_RESEND: Duration = Duration::from_millis(30);

#[derive(Clone, Debug)]
pub struct StreamConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
}

impl Default for StreamConfig {
    /// 720p at 60 fps (enough for screen checks, cheap to decode)
    fn default() -> Self {
        Self { width: 1280, height: 720, fps: 60, bitrate_kbps: 10000 }
    }
}

impl StreamConfig {
    /// When video is not needed: the lowest quality
    pub fn minimal() -> Self {
        Self { width: 640, height: 360, fps: 30, bitrate_kbps: 2000 }
    }
}

/// Channel for sending complete H.264 frames to the decoder thread.
pub type VideoTx = std::sync::mpsc::Sender<(u16, std::time::Instant, Vec<u8>)>;

/// Where decoded-to-be video and audio go (None = not wanted)
pub(crate) struct Outputs {
    pub video: Option<(VideoTx, std::sync::Arc<std::sync::atomic::AtomicBool>)>,
    pub audio: Option<AudioTx>,
    pub video_codec: VideoCodec,
}

struct AudioState {
    tx: AudioTx,
    reorder: Reorder,
    /// Frames lost (filled by concealment) and received since the last report
    lost: u32,
    frames: u32,
    since: Instant,
    /// Longest wait between two audio packets since the last report
    last_packet: Option<Instant>,
    max_gap: Duration,
}

struct VideoState {
    tx: VideoTx,
    /// Set by the decoder when it needs a key frame to recover
    need_key: std::sync::Arc<std::sync::atomic::AtomicBool>,
    assembler: Option<Assembler>,
    last_corrupt: Option<Instant>,
}

#[derive(Debug)]
pub enum Cmd {
    Button { button: Button, pressed: bool, queued_at: std::time::Instant },
    Stick { right: bool, x: f32, y: f32, queued_at: std::time::Instant },
    Stop,
}

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

struct InputStats {
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
    fn button(&mut self, queued_at: std::time::Instant, dequeued_at: std::time::Instant) {
        self.button_queue.record(dequeued_at.saturating_duration_since(queued_at));
        self.button_total.record(queued_at.elapsed());
    }

    fn stick_event(&mut self, queued_at: std::time::Instant) {
        self.stick_events += 1;
        self.stick_queue.record(queued_at.elapsed());
    }

    fn stick_packet(&mut self, latest_at: std::time::Instant) {
        self.stick_packets += 1;
        self.stick_total.record(latest_at.elapsed());
    }

    fn report(&mut self) {
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

#[derive(Clone, Debug, PartialEq)]
pub enum Phase {
    Connecting,
    Ready,
    Closed(String),
}

fn launch_spec(cfg: &StreamConfig, handshake_key: &[u8; 16], codec: VideoCodec) -> String {
    let (video_codec, dynamic_range) = codec.launch_values();
    format!(
        concat!(
            r#"{{"sessionId":"sessionId4321","streamResolutions":[{{"resolution":{{"width":{},"height":{}}},"maxFps":{},"score":10}}],"#,
            r#""network":{{"bwKbpsSent":{},"bwLoss":0.001000,"mtu":1454,"rtt":1,"ports":[53,2053]}},"slotId":1,"#,
            r#""appSpecification":{{"minFps":30,"minBandwidth":0,"extTitleId":"ps3","version":1,"timeLimit":1,"startTimeout":100,"afkTimeout":100,"afkTimeoutDisconnect":100}},"#,
            r#""konan":{{"ps3AccessToken":"accessToken","ps3RefreshToken":"refreshToken"}},"#,
            r#""requestGameSpecification":{{"model":"bravia_tv","platform":"android","audioChannels":"5.1","language":"sp","acceptButton":"X","connectedControllers":["xinput","ds3","ds4"],"yuvCoefficient":"bt709","videoEncoderProfile":"hw4.1","audioEncoderProfile":"audio1","adaptiveStreamMode":"resize"}},"#,
            r#""userProfile":{{"onlineId":"psnId","npId":"npId","region":"US","languagesUsed":["en","jp"]}},"#,
            r#""videoCodec":"{}","dynamicRange":"{}","handshakeKey":"{}"}}"#,
        ),
        cfg.width,
        cfg.height,
        cfg.fps,
        cfg.bitrate_kbps,
        video_codec,
        dynamic_range,
        B64.encode(handshake_key),
    )
}

struct Stream {
    sock: UdpSocket,
    tsn: u32,
    tag_remote: u32,
    cipher: Option<StreamCipher>,
    ecdh: Ecdh,
    events: EventHistory,
    state_seq: u16,
    sticks: Sticks,
    av_received: u16,
    phase: watch::Sender<Phase>,
    video: Option<VideoState>,
    audio: Option<AudioState>,
}

impl Stream {
    async fn send(&self, pkt: &[u8]) {
        if let Err(e) = self.sock.send(pkt).await {
            warn!("UDP send failed: {e}");
        }
    }

    async fn control(&mut self, chunk_type: u8, payload: &[u8], advance_by: usize) {
        let pkt = takion::control(chunk_type, 0, self.tag_remote, payload, self.cipher.as_mut(), advance_by);
        self.send(&pkt).await;
    }

    async fn send_data(&mut self, data: &[u8], channel: u16, proto: bool) {
        if self.cipher.is_some() {
            self.tsn += 1;
        }
        let payload = takion::data_payload(self.tsn, channel, data);
        let advance_by = if proto { data.len() } else { 0 };
        let pkt = takion::control(chunk::DATA, 1, self.tag_remote, &payload, self.cipher.as_mut(), advance_by);
        self.send(&pkt).await;
    }

    async fn send_state(&mut self) {
        let Some(c) = self.cipher.as_mut() else { return };
        let pkt = takion::feedback(header::FEEDBACK_STATE, self.state_seq, &self.sticks.state_data(), c);
        self.state_seq = self.state_seq.wrapping_add(1);
        self.send(&pkt).await;
    }

    /// Sends a button event and returns the packet so it can be re-sent once.
    async fn send_event(&mut self, button: Button, pressed: bool) -> Option<Vec<u8>> {
        let c = self.cipher.as_mut()?;
        let data = self.events.push(button, pressed);
        let pkt = takion::feedback(header::FEEDBACK_EVENT, self.events.sequence, &data, c);
        self.events.sequence = self.events.sequence.wrapping_add(1);
        self.send(&pkt).await;
        Some(pkt)
    }

    /// Handles a received protobuf message. Returns false to end the stream.
    async fn handle_proto(&mut self, data: &[u8]) -> Result<bool> {
        let msg = match TakionMessage::decode(data) {
            Ok(m) => m,
            Err(e) => {
                debug!("cannot decode protobuf: {e}");
                return Ok(true);
            }
        };
        match PayloadType::try_from(msg.r#type) {
            Ok(PayloadType::Bang) if self.cipher.is_none() => {
                let bang = msg.bang_payload.unwrap_or_default();
                if bang.version_accepted != Some(true) {
                    return Err(Error::Rejected("client version not accepted".into()));
                }
                if bang.encrypted_key_accepted != Some(true) {
                    return Err(Error::Rejected("encrypted key not accepted".into()));
                }
                let secret = self
                    .ecdh
                    .shared_secret(&bang.ecdh_pub_key.unwrap_or_default(), &bang.ecdh_sig.unwrap_or_default())?;
                self.cipher = Some(StreamCipher::new(&secret, &self.ecdh.handshake_key));
                info!("stream ready (key exchange done)");
                let _ = self.phase.send(Phase::Ready);
            }
            Ok(PayloadType::StreamInfo) => {
                debug!("got STREAMINFO → ACK");
                if let Some(a) = &self.audio {
                    let cfg = msg
                        .stream_info_payload
                        .as_ref()
                        .and_then(|i| i.audio_header.as_deref())
                        .and_then(AudioConfig::parse);
                    match cfg {
                        Some(cfg) => {
                            info!("audio: {} Hz, {} channels", cfg.rate, cfg.channels);
                            let _ = a.tx.send(AudioMsg::Config(cfg));
                        }
                        None => warn!("STREAMINFO has no usable audio header"),
                    }
                }
                if let Some(v) = self.video.as_mut() {
                    let header = msg
                        .stream_info_payload
                        .and_then(|i| i.resolution.into_iter().next())
                        .and_then(|r| r.video_header)
                        .unwrap_or_default();
                    // the console sends this again when it changes the stream (resolution, after adapting to
                    // the network); frame numbers start over then, so start a fresh assembler
                    if v.assembler.is_some() {
                        info!("stream changed: new {}-byte video header", header.len());
                    } else {
                        info!("got {}-byte video header", header.len());
                    }
                    debug!(
                        "video header: {}",
                        header.iter().map(|byte| format!("{byte:02x}")).collect::<Vec<_>>().join("")
                    );
                    v.assembler = Some(Assembler::new(header));
                }
                self.send_data(&proto::simple(PayloadType::StreamInfoAck), 9, true).await;
            }
            Ok(PayloadType::Heartbeat) => {
                self.send_data(&proto::simple(PayloadType::Heartbeat), 1, true).await;
            }
            Ok(PayloadType::Disconnect) => {
                let reason = msg.disconnect_payload.and_then(|d| d.reason).unwrap_or_default();
                info!("console disconnected: {reason}");
                let _ = self.phase.send(Phase::Closed(format!("console disconnected: {reason}")));
                return Ok(false);
            }
            Ok(t) => debug!("received message {t:?}"),
            Err(_) => debug!("received message type {}", msg.r#type),
        }
        Ok(true)
    }

    /// Audio packet: decrypt → put frames in order → decoder.
    fn handle_audio(&mut self, pkt: &[u8]) {
        let (Some(c), Some(a)) = (self.cipher.as_ref(), self.audio.as_mut()) else { return };
        let Some(p) = audio::parse(pkt) else { return };
        let now = Instant::now();
        if let Some(prev) = a.last_packet {
            a.max_gap = a.max_gap.max(now - prev);
        }
        a.last_packet = Some(now);
        let mut data = p.data.to_vec();
        c.remote.xor(p.key_pos, &mut data);
        for frame in a.reorder.push(p.frame, p.src, p.fec, p.unit_size, &data) {
            a.frames += 1;
            if frame.is_none() {
                a.lost += 1;
            }
            let _ = a.tx.send(AudioMsg::Frame(frame));
        }
        if a.since.elapsed() >= Duration::from_secs(10) {
            if a.lost > 0 || a.max_gap > Duration::from_millis(250) {
                info!(
                    "audio: {} of {} frames lost in 10 s (concealed), longest wait between packets {} ms",
                    a.lost,
                    a.frames,
                    a.max_gap.as_millis()
                );
            }
            (a.lost, a.frames, a.since, a.max_gap) = (0, 0, Instant::now(), Duration::ZERO);
        }
    }

    /// Video packet: decrypt → assemble → decoder. Requests a key frame when frames are lost.
    async fn handle_video(&mut self, pkt: &[u8]) {
        let (Some(c), Some(v)) = (self.cipher.as_ref(), self.video.as_mut()) else { return };
        let Some(asm) = v.assembler.as_mut() else { return };
        let Some(p) = video::parse(pkt) else { return };
        let mut data = p.data.to_vec();
        c.remote.xor(p.key_pos, &mut data);
        let mut out = Vec::new();
        asm.push(p.frame, p.unit, p.src_units(), data, &mut out);
        let mut lost = None;
        for a in out {
            match a {
                Assembled::Frame(i, h264) => {
                    let _ = v.tx.send((i, std::time::Instant::now(), h264));
                }
                Assembled::Lost(start, end) => lost = Some((start, end)),
            }
        }
        if lost.is_none() && v.need_key.swap(false, std::sync::atomic::Ordering::SeqCst) {
            lost = Some((p.frame, p.frame));
        }
        if let Some((start, end)) = lost
            && v.last_corrupt.is_none_or(|t| t.elapsed() > Duration::from_millis(200))
        {
            v.last_corrupt = Some(Instant::now());
            debug!("lost frames {start}..={end} → requesting a key frame");
            self.send_data(&proto::corrupt(start as u32, end as u32), 2, true).await;
        }
    }
}

pub async fn run(
    host: String,
    session_id: String,
    launch: SessionCipher,
    cfg: StreamConfig,
    mut cmd: mpsc::UnboundedReceiver<Cmd>,
    phase: watch::Sender<Phase>,
    out: Outputs,
) -> Result<()> {
    let sock = UdpSocket::bind("0.0.0.0:0").await?;
    // video key frames arrive as bursts of hundreds of packets; a bigger receive buffer keeps the OS from
    // dropping some of them while this task is busy
    if let Err(e) = socket2::SockRef::from(&sock).set_recv_buffer_size(RECV_BUFFER) {
        debug!("cannot enlarge the UDP receive buffer: {e}");
    }
    sock.connect((host.as_str(), PORT)).await?;
    let video_codec = out.video_codec;
    let mut s = Stream {
        sock,
        tsn: 1,
        tag_remote: 0,
        cipher: None,
        ecdh: Ecdh::new(),
        events: EventHistory::new(),
        state_seq: 0,
        sticks: Sticks::default(),
        av_received: 0,
        phase,
        video: out.video.map(|(tx, need_key)| VideoState { tx, need_key, assembler: None, last_corrupt: None }),
        audio: out.audio.map(|tx| AudioState {
            tx,
            reorder: Reorder::default(),
            lost: 0,
            frames: 0,
            since: Instant::now(),
            last_packet: None,
            max_gap: Duration::ZERO,
        }),
    };
    s.control(chunk::INIT, &takion::init_payload(TAG_LOCAL, s.tsn), 0).await;

    let mut buf = vec![0u8; 2048];
    let mut state_tick = interval(STATE_INTERVAL);
    state_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut congestion_tick = interval(CONGESTION_INTERVAL);
    let mut resend: Option<(Instant, Vec<u8>)> = None;
    let far = Instant::now() + Duration::from_secs(3600 * 24);
    let started = Instant::now();
    let mut recv_errors = 0u8;
    // A non-stick command found while coalescing stick motion. Keeping it here preserves the FIFO boundary
    // between state changes and ordered button events.
    let mut pending_cmd = None;
    let mut input_stats = InputStats::default();

    loop {
        let resend_at = resend.as_ref().map(|r| r.0).unwrap_or(far);
        tokio::select! {
            r = s.sock.recv(&mut buf) => {
                let n = match r {
                    Ok(n) => {
                        recv_errors = 0;
                        n
                    }
                    Err(e) => {
                        recv_errors += 1;
                        if recv_errors >= 5 {
                            let _ = s.phase.send(Phase::Closed(format!("UDP receive failed repeatedly: {e}")));
                            return Err(e.into());
                        }
                        warn!("UDP receive error ({recv_errors}/5): {e}");
                        // Some socket errors can be returned immediately. A short delay prevents a hot loop;
                        // repeated failures terminate the session above instead of hiding a dead socket.
                        tokio::time::sleep(Duration::from_millis(10)).await;
                        continue;
                    }
                };
                let pkt = &buf[..n];
                if let Some(t) = takion::av_type(pkt) {
                    s.av_received = s.av_received.wrapping_add(1);
                    if t == header::VIDEO {
                        let pkt = pkt.to_vec();
                        s.handle_video(&pkt).await;
                    } else {
                        s.handle_audio(pkt);
                    }
                    continue;
                }
                let Some(ctl) = takion::parse_control(pkt) else { continue };
                match ctl.chunk_type {
                    chunk::INIT_ACK => {
                        let Some((tag, cookie)) = takion::parse_init_ack(ctl.payload) else { continue };
                        s.tag_remote = tag;
                        let cookie = cookie.to_vec();
                        s.control(chunk::COOKIE, &cookie, 0).await;
                    }
                    chunk::COOKIE_ACK => {
                        let spec = launch_spec(&cfg, &s.ecdh.handshake_key, video_codec);
                        let mut spec = spec.into_bytes();
                        spec.push(0);
                        let ks = launch.encrypt_at(0, &vec![0u8; spec.len()]);
                        let spec_xor: Vec<u8> = spec.iter().zip(&ks).map(|(a, b)| a ^ b).collect();
                        let big = proto::big(
                            session_id.clone(),
                            B64.encode(spec_xor),
                            s.ecdh.public_key.clone(),
                            s.ecdh.public_sig.clone(),
                        );
                        s.send_data(&big, 1, false).await;
                    }
                    chunk::DATA => {
                        let Some((tsn, _channel, data)) = takion::parse_data(ctl.payload) else { continue };
                        let data = data.to_vec();
                        s.control(chunk::DATA_ACK, &takion::data_ack_payload(tsn), DATA_ACK_LENGTH).await;
                        match s.handle_proto(&data).await {
                            Ok(true) => {}
                            Ok(false) => return Ok(()),
                            Err(e) => {
                                let _ = s.phase.send(Phase::Closed(e.to_string()));
                                return Err(e);
                            }
                        }
                    }
                    chunk::DATA_ACK => {}
                    other => debug!("unhandled chunk {other}"),
                }
            }
            c = async {
                match pending_cmd.take() {
                    Some(c) => Some(c),
                    None => cmd.recv().await,
                }
            } => match c {
                Some(Cmd::Button { button, pressed, queued_at }) => {
                    let dequeued_at = std::time::Instant::now();
                    if let Some(pkt) = s.send_event(button, pressed).await {
                        resend = Some((Instant::now() + EVENT_RESEND, pkt));
                    } else {
                        warn!("stream not ready, ignoring button {}", button.name());
                    }
                    input_stats.button(queued_at, dequeued_at);
                    input_stats.report();
                }
                Some(Cmd::Stick { right, x, y, queued_at }) => {
                    if right { s.sticks.right = (x, y) } else { s.sticks.left = (x, y) }
                    input_stats.stick_event(queued_at);
                    let mut latest_at = queued_at;
                    // Axis events can arrive much faster than the network task under video load. Merge only
                    // consecutive stick commands; stop at a button/Stop so their relative order is retained.
                    loop {
                        match cmd.try_recv() {
                            Ok(Cmd::Stick { right, x, y, queued_at }) => {
                                if right { s.sticks.right = (x, y) } else { s.sticks.left = (x, y) }
                                input_stats.stick_event(queued_at);
                                latest_at = queued_at;
                            }
                            Ok(other) => {
                                pending_cmd = Some(other);
                                break;
                            }
                            Err(mpsc::error::TryRecvError::Empty) => break,
                            Err(mpsc::error::TryRecvError::Disconnected) => {
                                pending_cmd = Some(Cmd::Stop);
                                break;
                            }
                        }
                    }
                    s.send_state().await;
                    input_stats.stick_packet(latest_at);
                    input_stats.report();
                }
                Some(Cmd::Stop) | None => {
                    if s.cipher.is_some() {
                        s.send_data(&proto::disconnect(), 1, false).await;
                    }
                    let _ = s.phase.send(Phase::Closed("closed by the client".into()));
                    return Ok(());
                }
            },
            _ = sleep_until(resend_at), if resend.is_some() => {
                if let Some((_, pkt)) = resend.take() {
                    s.send(&pkt).await;
                }
            }
            _ = state_tick.tick() => {
                if s.cipher.is_none() && started.elapsed() > Duration::from_secs(10) {
                    let _ = s.phase.send(Phase::Closed("stream setup timed out".into()));
                    return Err(Error::Timeout("stream setup (no reply to INIT/COOKIE/BANG)"));
                }
                s.send_state().await;
            }
            _ = congestion_tick.tick() => {
                if let Some(c) = s.cipher.as_mut() {
                    let pkt = takion::congestion(s.av_received, 0, c);
                    s.send(&pkt).await;
                }
            }
        }
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
