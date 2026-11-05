//! DDP (Device Discovery Protocol): finding consoles, their status, and wake-up.
//!
//! Sending the text `SRCH * HTTP/1.1` over UDP makes the console reply with `HTTP/1.1 200 Ok` (on)
//! or `620 Server Standby` (rest mode), plus fields such as `host-id`.

use std::collections::HashMap;
use std::time::Duration;

use crate::error::{Error, Result, profile, protocol};
use tokio::net::UdpSocket;

pub const PORT_PS5: u16 = 9302;
const VERSION: &str = "00030010";

#[derive(Debug, Clone)]
pub struct Status {
    /// 200 = on, 620 = rest mode
    pub code: u16,
    pub fields: HashMap<String, String>,
}

impl Status {
    pub fn is_on(&self) -> bool {
        self.code == 200
    }
    pub fn host_id(&self) -> Option<&str> {
        self.fields.get("host-id").map(String::as_str)
    }
    pub fn host_type(&self) -> &str {
        self.fields.get("host-type").map(String::as_str).unwrap_or("PS5")
    }
}

pub async fn status(host: &str) -> Result<Status> {
    let sock = UdpSocket::bind("0.0.0.0:0").await?;
    let msg = format!("SRCH * HTTP/1.1\ndevice-discovery-protocol-version:{VERSION}\n");
    let mut buf = [0u8; 2048];
    for _ in 0..3 {
        sock.send_to(msg.as_bytes(), (host, PORT_PS5)).await?;
        if let Ok(Ok((n, _))) = tokio::time::timeout(Duration::from_millis(800), sock.recv_from(&mut buf)).await {
            return parse(&buf[..n]);
        }
    }
    Err(Error::NoResponse { host: host.to_string() })
}

/// A console that answered a network-wide search.
#[derive(Debug, Clone)]
pub struct Found {
    pub addr: std::net::IpAddr,
    pub status: Status,
}

impl Found {
    /// Name set on the console (Settings → System → System Software → Device Name)
    pub fn name(&self) -> &str {
        self.status.fields.get("host-name").map(String::as_str).unwrap_or("PS5")
    }
}

/// Finds consoles on the local network: broadcasts a search and collects answers for `wait`.
/// Consoles in rest mode answer too (`status.code` 620).
pub async fn discover(wait: Duration) -> Result<Vec<Found>> {
    let sock = UdpSocket::bind("0.0.0.0:0").await?;
    sock.set_broadcast(true)?;
    let msg = format!("SRCH * HTTP/1.1\ndevice-discovery-protocol-version:{VERSION}\n");
    let mut found: Vec<Found> = Vec::new();
    let mut buf = [0u8; 2048];
    let end = tokio::time::Instant::now() + wait;
    let mut next_send = tokio::time::Instant::now();
    loop {
        let now = tokio::time::Instant::now();
        if now >= end {
            break;
        }
        if now >= next_send {
            // a few sends, since UDP broadcasts can be dropped
            let _ = sock.send_to(msg.as_bytes(), ("255.255.255.255", PORT_PS5)).await;
            next_send = now + Duration::from_millis(600);
        }
        let until = next_send.min(end);
        if let Ok(Ok((n, from))) = tokio::time::timeout_at(until, sock.recv_from(&mut buf)).await
            && let Ok(status) = parse(&buf[..n])
            && status.host_id().is_some()
            && !found.iter().any(|f| f.status.host_id() == status.host_id())
        {
            found.push(Found { addr: from.ip(), status });
        }
    }
    Ok(found)
}

/// Wakes the console from rest mode. Needs the user credential derived from the regist key.
pub async fn wakeup(host: &str, regist_key: &str) -> Result<()> {
    let credential = credential(regist_key)?;
    let msg = format!(
        "WAKEUP * HTTP/1.1\nuser-credential:{credential}\nclient-type:vr\nauth-type:R\nmodel:w\napp-type:r\ndevice-discovery-protocol-version:{VERSION}\n"
    );
    let sock = UdpSocket::bind("0.0.0.0:0").await?;
    sock.send_to(msg.as_bytes(), (host, PORT_PS5)).await?;
    Ok(())
}

/// The regist key (16 hex chars = 8 bytes) decodes to 8 more hex chars.
/// Reading those as a hex integer gives the user credential.
fn credential(regist_key: &str) -> Result<String> {
    let inner = crate::profile::hex(regist_key)?;
    let inner = std::str::from_utf8(&inner).map_err(|_| profile("unexpected regist key format"))?;
    let bytes = crate::profile::hex(inner)?;
    Ok(bytes.iter().fold(0u128, |acc, &b| acc << 8 | b as u128).to_string())
}

fn parse(data: &[u8]) -> Result<Status> {
    let text = std::str::from_utf8(data).map_err(|_| protocol("DDP response is not UTF-8"))?;
    let mut code = 0;
    let mut fields = HashMap::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if let Some(rest) = line.strip_prefix("HTTP/1.1 ") {
            code = rest.split_whitespace().next().unwrap_or("0").parse().unwrap_or(0);
        } else if let Some((k, v)) = line.split_once(':') {
            fields.insert(k.to_string(), v.to_string());
        }
    }
    Ok(Status { code, fields })
}
