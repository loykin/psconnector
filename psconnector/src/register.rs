//! PIN registration: pairs this computer and a PSN account with a PS5.
//!
//! On the PS5: Settings → System → Remote Play → Pair Device shows an 8-digit PIN. While it is on screen the
//! console answers `SRC3` with `RES3` on UDP 9295, then accepts one encrypted registration request on TCP 9295
//! and replies with the regist keys (`RegistKey`, `RP-Key`, ...).

use std::collections::BTreeMap;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};

use crate::error::{Error, Result, protocol};
use crate::rpcrypt::SessionCipher;

const PORT: u16 = 9295;
const CLIENT_TYPE: &str = "dabfa2ec873de5839bee8d3f4c0239c4282c07c25c6077a2931afcf0adc0d34f";
/// The bytes of the registration key tables that are used (every 32nd, from the official app)
const REG_KEY_0: [u8; 16] =
    [0xD8, 0x60, 0xE1, 0x46, 0xBD, 0xB0, 0xBD, 0x94, 0xB4, 0x60, 0x07, 0x0B, 0xB1, 0x4F, 0xE7, 0x23];
const REG_KEY_1: [u8; 16] =
    [0xA8, 0x37, 0x61, 0xB5, 0x27, 0xFC, 0x07, 0x6E, 0x62, 0x43, 0x3A, 0xA7, 0x01, 0x95, 0x8A, 0x85];

/// Registration answer from the console, with the `PS5-` prefixes removed
/// (`RegistKey`, `RP-Key`, `Mac`, `Nickname`, `AP-Ssid`, ...).
pub type RegistData = BTreeMap<String, String>;

/// Key that encrypts the request: key table bytes with the PIN in the last 4.
fn key_0(pin: u32) -> [u8; 16] {
    let mut k = REG_KEY_0;
    for (i, b) in pin.to_be_bytes().iter().enumerate() {
        k[12 + i] ^= b;
    }
    k
}

/// Key sent inside the request, derived from our nonce.
fn key_1(nonce: &[u8; 16]) -> [u8; 16] {
    let mut k = [0u8; 16];
    for i in 0..16 {
        k[i] = (nonce[i] ^ REG_KEY_1[i]).wrapping_sub(45).wrapping_add(i as u8);
    }
    k
}

/// 480 bytes of 'A' with key_1 hidden at two places.
fn padding(k1: &[u8; 16]) -> Vec<u8> {
    let mut p = vec![b'A'; 480];
    p[199..207].copy_from_slice(&k1[8..]);
    p[401..409].copy_from_slice(&k1[..8]);
    p
}

fn request(account_id: &str, pin: u32, nonce: [u8; 16]) -> (SessionCipher, Vec<u8>) {
    let mut cipher = SessionCipher::new(key_0(pin), nonce);
    let mut body = padding(&key_1(&nonce));
    body.extend(cipher.encrypt(format!("Client-Type: {CLIENT_TYPE}\r\nNp-AccountId: {account_id}\r\n").as_bytes()));
    // the official client sends this malformed request line; keep it identical
    let mut req = format!(
        "POST /sie/ps5/rp/sess/rgst HTTP/1.1\r\n HTTP/1.1\r\nHOST: 10.0.2.15\r\nUser-Agent: remoteplay Windows\r\n\
         Connection: close\r\nContent-Length: {}\r\nRP-Version: 1.0\r\n\r\n",
        body.len()
    )
    .into_bytes();
    req.extend(body);
    (cipher, req)
}

/// Registers with the console at `host`. `account_id` is the base64 PSN account id from
/// [`crate::psn::account_from_redirect`]; `pin` is the 8-digit code shown on the PS5.
pub async fn register(host: &str, account_id: &str, pin: &str) -> Result<RegistData> {
    let pin: u32 = pin.trim().parse().map_err(|_| Error::Rejected("the PIN must be 8 digits".into()))?;
    let st = crate::ddp::status(host).await?;
    if st.host_type() != "PS5" {
        return Err(Error::Unsupported(format!("only PS5 can be registered ({})", st.host_type())));
    }

    // is the console waiting for a device?
    let udp = UdpSocket::bind("0.0.0.0:0").await?;
    let mut buf = [0u8; 64];
    let mut ready = false;
    for _ in 0..3 {
        udp.send_to(b"SRC3", (host, PORT)).await?;
        if let Ok(Ok((n, _))) = tokio::time::timeout(Duration::from_secs(1), udp.recv_from(&mut buf)).await
            && buf[..n].starts_with(b"RES3")
        {
            ready = true;
            break;
        }
    }
    if !ready {
        return Err(Error::Rejected(
            "the PS5 is not waiting for a device: open Settings → System → Remote Play → Pair Device".into(),
        ));
    }

    let (mut cipher, req) = request(account_id, pin, rand::random());
    let mut tcp = TcpStream::connect((host, PORT)).await?;
    tcp.set_nodelay(true)?;
    tcp.write_all(&req).await?;
    let mut resp = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), tcp.read_to_end(&mut resp))
        .await
        .map_err(|_| Error::Timeout("registration answer"))??;
    parse_answer(&mut cipher, &resp)
}

fn parse_answer(cipher: &mut SessionCipher, resp: &[u8]) -> Result<RegistData> {
    let head_end = resp.windows(4).position(|w| w == b"\r\n\r\n").ok_or_else(|| protocol("no registration answer"))?;
    let status = String::from_utf8_lossy(&resp[..resp.iter().position(|&b| b == b'\r').unwrap_or(0)]).to_string();
    if !status.contains("200") {
        return Err(Error::Rejected(format!("registration refused ({status}); check the PIN and try again")));
    }
    let text = cipher.decrypt(&resp[head_end + 4..]);
    let text = String::from_utf8_lossy(&text);
    let data: RegistData = text
        .split("\r\n")
        .filter_map(|l| l.split_once(": "))
        .map(|(k, v)| (k.strip_prefix("PS5-").unwrap_or(k).to_string(), v.to_string()))
        .collect();
    if !data.contains_key("RegistKey") || !data.contains_key("RP-Key") {
        return Err(protocol("registration answer has no keys"));
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    /// Expected values from pyremoteplay 0.7.6 with nonce 00..0f, PIN 12345678, account "AAAAAAAAAAA=".
    #[test]
    fn request_matches_pyremoteplay() {
        let nonce: [u8; 16] = std::array::from_fn(|i| i as u8);
        assert_eq!(hex(&key_0(12_345_678)), "d860e146bdb0bd94b460070bb1f3866d");
        assert_eq!(hex(&key_1(&nonce)), "7b0a388cfad1da4345260d8aec78656c");
        use sha2::Digest;
        let pad = padding(&key_1(&nonce));
        assert_eq!(
            hex(&sha2::Sha256::digest(&pad)),
            "105159bfe22c08f8f287e04acefc00dec72d71b873b9772eea593e0aa4a7fc56"
        );
        let (_, req) = request("AAAAAAAAAAA=", 12_345_678, nonce);
        let head_end = req.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        assert!(req.starts_with(b"POST /sie/ps5/rp/sess/rgst HTTP/1.1\r\n HTTP/1.1\r\n"));
        assert_eq!(
            hex(&req[head_end + 480..]),
            "27b366fdb231f7ad85ea2f8967e9c26bfbf73bb401c250591bb4988cba551a56fa665f6cdf8b9f2187d577529ea68ac7\
             2850a82902041ae692ce2f8dffa719d9634ff268f4e3d19802f78be8656ae0ab0b073cdf89a196b5b43bd8c49da95fb6\
             40db5b5b40d101d9464cef"
        );
    }

    #[test]
    fn answer_is_decrypted_and_prefixes_removed() {
        let nonce = [7u8; 16];
        let mut console = SessionCipher::new(key_0(11_112_222), nonce);
        let body = console.encrypt(
            b"PS5-RegistKey: 3031323334353637\r\nRP-Key: 00112233445566778899aabbccddeeff\r\nPS5-Nickname: PS5-123\r\n",
        );
        let mut resp = b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\n".to_vec();
        resp.extend(body);
        let mut ours = SessionCipher::new(key_0(11_112_222), nonce);
        let data = parse_answer(&mut ours, &resp).unwrap();
        assert_eq!(data["RegistKey"], "3031323334353637");
        assert_eq!(data["Nickname"], "PS5-123");
        let mut ours = SessionCipher::new(key_0(11_112_222), nonce);
        assert!(parse_answer(&mut ours, b"HTTP/1.1 403 Forbidden\r\n\r\n").is_err());
    }
}
