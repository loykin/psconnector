//! Reading PS5 registration data.
//!
//! PSN login and PIN registration are not implemented yet. Instead the regist keys saved by
//! pyremoteplay in `~/.pyremoteplay/.profile.json` are read as they are.

use std::path::PathBuf;

use crate::error::{Result, profile as err};
use serde_json::Value;

/// Registration result for one console.
#[derive(Clone)]
pub struct Registration {
    /// Console host-id (MAC address, 12 upper-case hex chars)
    pub host_id: String,
    /// "PS5" / "PS4"
    pub host_type: String,
    /// Hex string sent as-is in the session init request (8 bytes)
    pub regist_key: String,
    /// 16 bytes used to derive the session key
    pub rp_key: [u8; 16],
}

impl std::fmt::Debug for Registration {
    // Never log key material
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registration")
            .field("host_id", &self.host_id)
            .field("host_type", &self.host_type)
            .finish_non_exhaustive()
    }
}

pub fn default_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".pyremoteplay").join(".profile.json")
}

/// Finds the registration for `host_id` in the profile file.
/// If `user` is None, the first user registered with that console is used.
pub fn load(path: &PathBuf, user: Option<&str>, host_id: &str) -> Result<Registration> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| err(format!("cannot read registration file {}: {e}", path.display())))?;
    let root: Value = serde_json::from_str(&text).map_err(|_| err("registration file is not JSON"))?;
    let users = root.as_object().ok_or_else(|| err("unexpected registration file format"))?;
    for (name, profile) in users {
        if user.is_some_and(|u| u != name) {
            continue;
        }
        let Some(host) = profile["hosts"].get(host_id) else { continue };
        let data = &host["data"];
        let regist_key = data["RegistKey"].as_str().ok_or_else(|| err("missing RegistKey"))?;
        let rp_key_hex = data["RP-Key"].as_str().ok_or_else(|| err("missing RP-Key"))?;
        let rp_key: [u8; 16] = hex(rp_key_hex)?.try_into().map_err(|_| err("RP-Key is not 16 bytes"))?;
        return Ok(Registration {
            host_id: host_id.to_string(),
            host_type: host["type"].as_str().unwrap_or("PS5").to_string(),
            regist_key: regist_key.to_string(),
            rp_key,
        });
    }
    Err(err(format!("no user registered for host-id {host_id} (register with pyremoteplay/ps5rmtctl first)")))
}

pub(crate) fn hex(s: &str) -> Result<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return Err(err("odd-length hex string"));
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|e| err(format!("not hex: {e}"))))
        .collect()
}
