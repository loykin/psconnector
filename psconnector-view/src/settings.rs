//! Small per-user settings in `~/.psconnector/settings.json` (`%USERPROFILE%` on Windows):
//! the last console address and picture quality.

use std::path::PathBuf;

use serde_json::Value;

fn path() -> PathBuf {
    std::env::home_dir().unwrap_or_default().join(".psconnector").join("settings.json")
}

fn read() -> Value {
    std::fs::read_to_string(path())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| serde_json::json!({}))
}

pub fn get(key: &str) -> Option<Value> {
    read().get(key).cloned().filter(|v| !v.is_null())
}

/// Sets one setting, keeping the others.
pub fn set(key: &str, value: Value) {
    let mut v = read();
    v[key] = value;
    let path = path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(e) = std::fs::write(&path, format!("{v:#}\n")) {
        log::warn!("cannot save {}: {e}", path.display());
    }
}

/// Address of the console connected to last time.
pub fn last_host() -> Option<String> {
    get("host").and_then(|v| v.as_str().map(str::to_string))
}

pub fn save_host(host: &str) {
    set("host", host.into());
}
