//! PS5 registration data.
//!
//! psconnector keeps its own file, `~/.psconnector/profile.json`, written by [`save_account`] and
//! [`save_registration`] (see the `psn` and `register` modules). It has the same layout as pyremoteplay's
//! `~/.pyremoteplay/.profile.json`, which is still read when a console is not in ours:
//! `{"<online id>": {"id": "<account id>", "hosts": {"<host-id>": {"type": "PS5", "data": {"RegistKey", "RP-Key", ...}}}}}`

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

/// psconnector's own registration file, `~/.psconnector/profile.json` (`%USERPROFILE%` on Windows).
pub fn default_path() -> PathBuf {
    std::env::home_dir().unwrap_or_default().join(".psconnector").join("profile.json")
}

/// pyremoteplay's file, read as a fallback so earlier registrations keep working.
pub fn pyremoteplay_path() -> PathBuf {
    std::env::home_dir().unwrap_or_default().join(".pyremoteplay").join(".profile.json")
}

static READ_PYREMOTEPLAY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// Whether [`load_any`] (and so `Client::connect`) also looks in pyremoteplay's file (default: yes).
/// Turned off after signing out of an account that came from there.
pub fn set_read_pyremoteplay(on: bool) {
    READ_PYREMOTEPLAY.store(on, std::sync::atomic::Ordering::Relaxed);
}

pub fn read_pyremoteplay() -> bool {
    READ_PYREMOTEPLAY.load(std::sync::atomic::Ordering::Relaxed)
}

/// Registration for `host_id` from psconnector's file, or else from pyremoteplay's.
pub fn load_any(user: Option<&str>, host_id: &str) -> Result<Registration> {
    match load(&default_path(), user, host_id) {
        Ok(r) => Ok(r),
        Err(ours) if read_pyremoteplay() => load(&pyremoteplay_path(), user, host_id).map_err(|_| ours),
        Err(ours) => Err(ours),
    }
}

/// A signed-in account in the file and the consoles registered with it.
#[derive(Debug, Clone)]
pub struct User {
    pub online_id: String,
    pub account_id: String,
    pub hosts: Vec<String>,
}

/// Accounts in a registration file (missing file = none).
pub fn users(path: &PathBuf) -> Vec<User> {
    let Some(root) = read(path) else { return Vec::new() };
    root.as_object()
        .map(|o| {
            o.iter()
                .map(|(name, u)| User {
                    online_id: name.clone(),
                    account_id: u["id"].as_str().unwrap_or_default().to_string(),
                    hosts: u["hosts"].as_object().map(|h| h.keys().cloned().collect()).unwrap_or_default(),
                })
                .collect()
        })
        .unwrap_or_default()
}

fn read(path: &PathBuf) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

fn write(path: &PathBuf, root: &Value) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, format!("{root:#}\n"))?;
    restrict(&tmp);
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// The file holds keys to the console: readable by this user only (Unix).
fn restrict(path: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// Adds (or keeps) a signed-in account in the file.
pub fn save_account(path: &PathBuf, online_id: &str, account_id: &str) -> Result<()> {
    let mut root = read(path).filter(Value::is_object).unwrap_or_else(|| serde_json::json!({}));
    let user = &mut root[online_id];
    if !user.is_object() {
        *user = serde_json::json!({"hosts": {}});
    }
    user["id"] = account_id.into();
    write(path, &root)
}

/// Removes an account and every console registered with it (sign out). Missing = nothing to do.
pub fn remove_account(path: &PathBuf, online_id: &str) -> Result<()> {
    let Some(mut root) = read(path) else { return Ok(()) };
    if let Some(o) = root.as_object_mut() {
        o.remove(online_id);
    }
    write(path, &root)
}

/// Forgets one console's registration (under every account). The PS5 keeps this computer in its device
/// list until it is removed there (Settings → System → Remote Play → Paired Devices).
pub fn remove_host(path: &PathBuf, host_id: &str) -> Result<()> {
    let Some(mut root) = read(path) else { return Ok(()) };
    if let Some(users) = root.as_object_mut() {
        for u in users.values_mut() {
            if let Some(h) = u.get_mut("hosts").and_then(Value::as_object_mut) {
                h.remove(host_id);
            }
        }
    }
    write(path, &root)
}

/// Stores a console's registration answer under the account.
pub fn save_registration(
    path: &PathBuf,
    online_id: &str,
    account_id: &str,
    host_id: &str,
    data: &std::collections::BTreeMap<String, String>,
) -> Result<()> {
    save_account(path, online_id, account_id)?;
    let mut root = read(path).unwrap_or_else(|| serde_json::json!({}));
    root[online_id]["hosts"][host_id] = serde_json::json!({"type": "PS5", "data": data});
    write(path, &root)
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
    Err(err(format!("this console ({host_id}) is not registered yet")))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_load_and_remove() {
        let path = std::env::temp_dir().join(format!("psconnector-profile-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let data: std::collections::BTreeMap<String, String> =
            [("RegistKey", "3031323334353637"), ("RP-Key", "00112233445566778899aabbccddeeff")]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
        save_registration(&path, "someone", "AQAAAAAAAAA=", "AABBCCDDEEFF", &data).unwrap();
        let reg = load(&path, None, "AABBCCDDEEFF").unwrap();
        assert_eq!(reg.regist_key, "3031323334353637");
        assert_eq!(users(&path)[0].hosts, ["AABBCCDDEEFF"]);
        remove_host(&path, "AABBCCDDEEFF").unwrap();
        assert!(load(&path, None, "AABBCCDDEEFF").is_err());
        assert_eq!(users(&path).len(), 1, "forgetting a console keeps the account");
        remove_account(&path, "someone").unwrap();
        assert!(users(&path).is_empty());
        let _ = std::fs::remove_file(&path);
    }
}
