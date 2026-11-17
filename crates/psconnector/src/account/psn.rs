//! PSN login, to get the account id that PIN registration needs.
//!
//! Same flow as the official Remote Play app: the user signs in on Sony's page in a browser
//! ([`login_url`]); afterwards the browser lands on a `.../remoteplay/redirect?code=...` address.
//! That address goes to [`account_from_redirect`], which trades the code for the account id.
//! The password is only ever typed into Sony's page.

use base64::{Engine, engine::general_purpose::STANDARD as B64};
use serde_json::Value;

use crate::error::{Error, Result};

const CLIENT_ID: &str = "ba495a24-818c-472b-b12d-ff231c1b5745";
/// base64 of the official app's client secret
const CLIENT_SECRET_B64: &str = "bXZhaVprUnNBc0kxSUJrWQ==";
const REDIRECT_URL: &str = "https://remoteplay.dl.playstation.net/remoteplay/redirect";
const TOKEN_URL: &str = "https://auth.api.sonyentertainmentnetwork.com/2.0/oauth/token";

/// A signed-in PSN account.
#[derive(Debug, Clone)]
pub struct Account {
    /// PSN online id (user name)
    pub online_id: String,
    /// Account id as base64 of its 8 little-endian bytes (what registration sends)
    pub account_id: String,
}

/// Sony's sign-in page. Open it in a browser.
pub fn login_url() -> String {
    format!(
        "https://auth.api.sonyentertainmentnetwork.com/2.0/oauth/authorize?service_entity=urn:service-entity:psn\
         &response_type=code&client_id={CLIENT_ID}&redirect_uri={REDIRECT_URL}&scope=psn:clientapp\
         &request_locale=en_US&ui=pr&service_logo=ps&layout_type=popup&smcid=remoteplay&prompt=always\
         &PlatformPrivacyWs1=minimal&no_captcha=true&"
    )
}

/// The `code` in the address the browser ends up on after signing in.
pub fn code_from_redirect(url: &str) -> Result<String> {
    let url = url.trim();
    if !url.starts_with(REDIRECT_URL) {
        return Err(Error::Rejected(format!("paste the address that starts with {REDIRECT_URL}")));
    }
    let query = url.split_once('?').map(|(_, q)| q).unwrap_or_default();
    query
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .find(|(k, _)| *k == "code")
        .map(|(_, v)| v.to_string())
        .filter(|c| c.len() > 1)
        .ok_or_else(|| Error::Rejected("that address has no sign-in code; sign in again".into()))
}

/// Base64 of the account id's 8 little-endian bytes.
pub fn account_id_b64(user_id: u64) -> String {
    B64.encode(user_id.to_le_bytes())
}

/// Trades the redirect address for the account (two HTTPS calls to Sony).
pub async fn account_from_redirect(url: &str) -> Result<Account> {
    let code = code_from_redirect(url)?;
    tokio::task::spawn_blocking(move || fetch_account(&code))
        .await
        .map_err(|e| Error::Closed(format!("login task failed: {e}")))?
}

fn basic_auth() -> String {
    let secret = B64.decode(CLIENT_SECRET_B64).unwrap_or_default();
    let secret = String::from_utf8_lossy(&secret);
    format!("Basic {}", B64.encode(format!("{CLIENT_ID}:{secret}")))
}

fn http_err(what: &str, e: ureq::Error) -> Error {
    match e {
        ureq::Error::StatusCode(code) => Error::Rejected(format!("{what}: Sony answered {code}; sign in again")),
        other => Error::Closed(format!("{what}: {other}")),
    }
}

fn fetch_account(code: &str) -> Result<Account> {
    let body = format!("grant_type=authorization_code&code={code}&redirect_uri={REDIRECT_URL}&");
    let token: Value = ureq::post(TOKEN_URL)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Authorization", &basic_auth())
        .send(body)
        .map_err(|e| http_err("token request", e))?
        .body_mut()
        .read_json()
        .map_err(|e| http_err("token answer", e))?;
    let access =
        token["access_token"].as_str().ok_or_else(|| Error::Rejected("no access token in the answer".into()))?;

    let info: Value = ureq::get(&format!("{TOKEN_URL}/{access}"))
        .header("Authorization", &basic_auth())
        .call()
        .map_err(|e| http_err("account request", e))?
        .body_mut()
        .read_json()
        .map_err(|e| http_err("account answer", e))?;
    let user_id: u64 = match &info["user_id"] {
        Value::String(s) => s.parse().ok(),
        Value::Number(n) => n.as_u64(),
        _ => None,
    }
    .ok_or_else(|| Error::Rejected("no user id in the account answer".into()))?;
    Ok(Account {
        online_id: info["online_id"].as_str().unwrap_or("psn-user").to_string(),
        account_id: account_id_b64(user_id),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirect_code_and_account_id() {
        let url = format!("{REDIRECT_URL}?code=v3.AbCdEf&cid=1234");
        assert_eq!(code_from_redirect(&url).unwrap(), "v3.AbCdEf");
        assert!(code_from_redirect("https://example.com/?code=x").is_err());
        assert!(code_from_redirect(&format!("{REDIRECT_URL}?error=access_denied")).is_err());
        // pyremoteplay: base64.b64encode(int(uid).to_bytes(8, "little"))
        assert_eq!(account_id_b64(1), "AQAAAAAAAAA=");
        assert_eq!(account_id_b64(0x0102030405060708), "CAcGBQQDAgE=");
    }
}
