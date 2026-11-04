//! Session setup (TCP 9295).
//!
//! 1. `GET /sie/ps5/rp/sess/init` with the regist key → the console returns a nonce.
//! 2. Derive the session cipher from the nonce and RP-Key, encrypt the auth values, `GET /sie/ps5/rp/sess/ctrl`.
//! 3. After the ctrl response the same TCP connection becomes the control channel, carrying the session id,
//!    heartbeats, and the standby command.

use std::time::{Duration, Instant};

use crate::error::{Error, Result, protocol};
use base64::{Engine, engine::general_purpose::STANDARD as B64};
use log::{debug, info, warn};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};

use crate::profile::{Registration, hex};
use crate::rpcrypt::{self, SessionCipher};

pub const PORT: u16 = 9295;
const USER_AGENT: &str = "remoteplay Windows";
const OS_TYPE: &str = "Win10.0.0";
const RP_VERSION_PS5: &str = "1.0";
const DID_PREFIX: [u8; 10] = [0x00, 0x18, 0x00, 0x00, 0x00, 0x07, 0x00, 0x40, 0x00, 0x80];
const STREAM_TYPE_H264: u32 = 1;

mod msg {
    pub const SESSION_ID: u16 = 0x33;
    pub const HEARTBEAT_REQUEST: u16 = 0xFE;
    pub const HEARTBEAT_RESPONSE: u16 = 0x1FE;
    pub const STANDBY: u16 = 0x50;
}
const HEARTBEAT_RESPONSE: [u8; 8] = [0x00, 0x00, 0x00, 0x00, 0x01, 0xFE, 0x00, 0x00];

pub enum CtrlCmd {
    Standby,
    Close,
}

/// Result of a successful session setup.
pub struct Started {
    pub session_id: String,
    /// Used to encrypt the launch spec (counter 0 key stream)
    pub launch_cipher: SessionCipher,
    pub ctrl: mpsc::UnboundedSender<CtrlCmd>,
    pub ctrl_task: tokio::task::JoinHandle<()>,
}

struct Response {
    status: u16,
    headers: Vec<(String, String)>,
    rest: Vec<u8>,
}

impl Response {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

async fn request(sock: &mut TcpStream, host: &str, path: &str, headers: &[(&str, String)]) -> Result<Response> {
    let mut req = format!("GET {path} HTTP/1.1\r\nHost: {host}:{PORT}\r\nUser-Agent: {USER_AGENT}\r\n");
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    sock.write_all(req.as_bytes()).await?;

    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let end = loop {
        let n = tokio::time::timeout(Duration::from_secs(5), sock.read(&mut tmp))
            .await
            .map_err(|_| Error::Timeout("session response"))??;
        if n == 0 {
            return Err(Error::Closed("during the session response".into()));
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
    };
    let head = std::str::from_utf8(&buf[..end]).map_err(|_| protocol("HTTP headers are not UTF-8"))?;
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| protocol("unexpected HTTP response format"))?;
    let headers =
        lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_string(), v.trim().to_string())).collect();
    Ok(Response { status, headers, rest: buf[end + 4..].to_vec() })
}

/// Rejection response → error. "Remote Play in use" gets its own variant (retrying shortly often works).
fn rejection(stage: &str, resp: &Response) -> Error {
    let why = match resp.header("RP-Application-Reason") {
        Some(r) => match u32::from_str_radix(r, 16) {
            Ok(0x8010_8b10) => return Error::Busy,
            Ok(0x8010_8b09) => "registration failed (regist key does not match)".into(),
            Ok(0x8010_8b11) => "Remote Play version mismatch".into(),
            Ok(0x8010_8b15) => "console-side error".into(),
            Ok(code) => format!("rejection code 0x{code:08x}"),
            Err(_) => r.to_string(),
        },
        None => format!("HTTP {}", resp.status),
    };
    Error::Rejected(format!("{stage}: {why}"))
}

pub async fn start(host: &str, reg: &Registration) -> Result<Started> {
    if reg.host_type != "PS5" {
        return Err(Error::Unsupported(format!("only PS5 is supported ({})", reg.host_type)));
    }
    // 1. init → nonce
    let mut sock = TcpStream::connect((host, PORT)).await?;
    let resp = request(
        &mut sock,
        host,
        "/sie/ps5/rp/sess/init",
        &[
            ("Connection", "close".into()),
            ("Content-Length", "0".into()),
            ("RP-Registkey", reg.regist_key.clone()),
            ("Rp-Version", RP_VERSION_PS5.into()),
        ],
    )
    .await?;
    drop(sock);
    if resp.status != 200 {
        return Err(rejection("session init", &resp));
    }
    let nonce: [u8; 16] = B64
        .decode(resp.header("RP-Nonce").ok_or_else(|| protocol("missing RP-Nonce"))?)
        .map_err(|_| protocol("RP-Nonce is not base64"))?
        .try_into()
        .map_err(|_| protocol("nonce is not 16 bytes"))?;

    // 2. ctrl → authenticate
    let key = rpcrypt::aes_key(&nonce, &reg.rp_key);
    let rp_nonce = rpcrypt::rp_nonce(&nonce);
    let mut cipher = SessionCipher::new(key, rp_nonce);
    let mut regist = hex(&reg.regist_key)?;
    regist.resize(16, 0);
    let mut did = DID_PREFIX.to_vec();
    did.extend_from_slice(&rand::random::<[u8; 16]>());
    did.extend_from_slice(&[0; 6]);
    let mut os = OS_TYPE.as_bytes().to_vec();
    os.resize(10, 0);
    let mut enc = |data: &[u8]| B64.encode(cipher.encrypt(data));
    let headers = [
        ("Connection", "keep-alive".to_string()),
        ("Content-Length", "0".into()),
        ("RP-Auth", enc(&regist)),
        ("RP-Version", RP_VERSION_PS5.into()),
        ("RP-Did", enc(&did)),
        ("RP-ControllerType", "3".into()),
        ("RP-ClientType", "11".into()),
        ("RP-OSType", enc(&os)),
        ("RP-ConPath", "1".into()),
        ("RP-StartBitrate", enc(&[0; 4])),
        ("RP-StreamingType", enc(&STREAM_TYPE_H264.to_le_bytes())),
    ];
    let mut sock = TcpStream::connect((host, PORT)).await?;
    let resp = request(&mut sock, host, "/sie/ps5/rp/sess/ctrl", &headers).await?;
    if resp.status != 200 {
        return Err(rejection("session auth", &resp));
    }
    let server_type = resp.header("RP-Server-Type").ok_or_else(|| protocol("auth response has no RP-Server-Type"))?;
    let server_type = cipher.decrypt(&B64.decode(server_type).map_err(|_| protocol("RP-Server-Type is not base64"))?);
    debug!("server type: {server_type:?}");
    info!("session authenticated");

    // 3. control channel: wait for the session id
    let (id_tx, id_rx) = oneshot::channel();
    let (ctrl_tx, ctrl_rx) = mpsc::unbounded_channel();
    let launch_cipher = SessionCipher::new(key, rp_nonce);
    let ctrl_task = tokio::spawn(ctrl_loop(sock, resp.rest, cipher, id_tx, ctrl_rx));
    let session_id = tokio::time::timeout(Duration::from_secs(5), id_rx)
        .await
        .map_err(|_| Error::Timeout("session id"))?
        .map_err(|_| Error::Closed("control channel closed before the session id arrived".into()))?;
    Ok(Started { session_id, launch_cipher, ctrl: ctrl_tx, ctrl_task })
}

fn build(t: u16, payload: &[u8], cipher: &mut SessionCipher) -> Vec<u8> {
    // like pyremoteplay, send the payload in plain text and only advance the counter (fine over 13 hours of use)
    cipher.skip_encrypt();
    let mut buf = Vec::with_capacity(8 + payload.len());
    buf.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    buf.extend_from_slice(&t.to_be_bytes());
    buf.extend_from_slice(&[0, 0]);
    buf.extend_from_slice(payload);
    buf
}

/// The session id is usually ASCII but sometimes contains odd bytes. Map bytes to chars one to one.
fn session_id_string(b: &[u8]) -> String {
    match std::str::from_utf8(b) {
        Ok(s) => s.to_string(),
        Err(_) => b.iter().map(|&c| c as char).collect(),
    }
}

async fn ctrl_loop(
    mut sock: TcpStream,
    mut buf: Vec<u8>,
    mut cipher: SessionCipher,
    id_tx: oneshot::Sender<String>,
    mut cmd: mpsc::UnboundedReceiver<CtrlCmd>,
) {
    let mut id_tx = Some(id_tx);
    let mut last_hb = Instant::now();
    let mut tmp = [0u8; 4096];
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        // handle every complete message received so far
        while buf.len() >= 8 {
            let len = u32::from_be_bytes(buf[0..4].try_into().unwrap()) as usize;
            if buf.len() < 8 + len {
                break;
            }
            let t = u16::from_be_bytes([buf[4], buf[5]]);
            let payload = if len > 0 { cipher.decrypt(&buf[8..8 + len]) } else { Vec::new() };
            buf.drain(..8 + len);
            match t {
                msg::SESSION_ID => {
                    if let Some(tx) = id_tx.take() {
                        let _ = tx.send(session_id_string(payload.get(2..).unwrap_or_default()));
                    }
                }
                msg::HEARTBEAT_REQUEST => {
                    last_hb = Instant::now();
                    let out = build(msg::HEARTBEAT_RESPONSE, &HEARTBEAT_RESPONSE, &mut cipher);
                    if sock.write_all(&out).await.is_err() {
                        return;
                    }
                }
                msg::HEARTBEAT_RESPONSE => last_hb = Instant::now(),
                other => debug!("control channel message 0x{other:x} ({len} bytes)"),
            }
        }
        tokio::select! {
            r = sock.read(&mut tmp) => match r {
                Ok(0) | Err(_) => { info!("control channel closed"); return; }
                Ok(n) => buf.extend_from_slice(&tmp[..n]),
            },
            c = cmd.recv() => match c {
                Some(CtrlCmd::Standby) => {
                    info!("requesting standby");
                    let out = build(msg::STANDBY, &[], &mut cipher);
                    let _ = sock.write_all(&out).await;
                }
                Some(CtrlCmd::Close) | None => return, // closed by the client
            },
            _ = tick.tick() => {
                if last_hb.elapsed() > Duration::from_secs(5) {
                    let out = build(msg::HEARTBEAT_REQUEST, &[], &mut cipher);
                    if sock.write_all(&out).await.is_err() {
                        warn!("failed to send heartbeat");
                        return;
                    }
                    last_hb = Instant::now();
                }
            }
        }
    }
}
