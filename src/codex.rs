//! OpenAI Codex (ChatGPT) accounts and usage. Ported from `Codex.swift`.
//!
//! Codex (CLI + app) signs into ChatGPT and stores OAuth tokens in
//! `~/.codex/auth.json` (or `$CODEX_HOME/auth.json`) — a plain JSON file, the
//! same on every platform. The live account is whatever's in that file; saved
//! snapshots let PitStop switch by swapping it. Usage comes from
//! `chatgpt.com/backend-api/codex/usage`.

use crate::usage_api::ApiError;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use chrono::{DateTime, Local, SecondsFormat, Utc};
use serde_json::{json, Value};
use std::fmt;
use std::time::Duration;

const USAGE_URL: &str = "https://chatgpt.com/backend-api/codex/usage";
pub const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
/// Codex CLI's public OAuth client (the `aud` claim of its id_token).
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

pub const PROVIDER: &str = "codex";

#[derive(Debug, Clone)]
pub enum CodexError {
    SessionExpired,
    Malformed,
}

impl fmt::Display for CodexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CodexError::SessionExpired => write!(f, "Codex token expired"),
            CodexError::Malformed => write!(f, "Unexpected Codex usage response"),
        }
    }
}
impl std::error::Error for CodexError {}

/// Credentials parsed from an `auth.json` blob.
pub struct Creds {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub account_id: String,
    pub email: String,
    pub plan_label: String,
}

/// Provider-neutral usage for the row: a labelled bar per rate-limit window.
#[derive(Clone)]
pub struct Usage {
    pub windows: Vec<UsageWindow>,
    pub fetched_at: DateTime<Local>,
}

#[derive(Clone)]
pub struct UsageWindow {
    pub label: String,
    pub used_percent: f64,
    pub resets_at: Option<DateTime<Utc>>,
}

impl Usage {
    pub fn max_utilization(&self) -> f64 {
        self.windows
            .iter()
            .map(|w| w.used_percent)
            .fold(0.0, f64::max)
    }
}

pub struct Refreshed {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub id_token: Option<String>,
}

/// The current `~/.codex/auth.json` contents, or `None` if absent.
pub fn live_blob() -> Option<Vec<u8>> {
    std::fs::read(crate::codex_store::auth_path()).ok()
}

/// True when Codex is installed and configured at all.
pub fn is_present() -> bool {
    crate::codex_store::auth_path().exists()
}

/// Re-serialize an auth blob as compact, key-sorted JSON (serde_json sorts
/// object keys by default), so saved snapshots are byte-stable for change
/// detection and read back cleanly.
pub fn normalized_blob(data: &[u8]) -> Vec<u8> {
    match serde_json::from_slice::<Value>(data) {
        Ok(v) => serde_json::to_vec(&v).unwrap_or_else(|_| data.to_vec()),
        Err(_) => data.to_vec(),
    }
}

/// Parse a ChatGPT (not API-key) Codex auth blob into credentials + identity.
/// Returns `None` for API-key auth or a blob without ChatGPT tokens.
pub fn credentials(blob: &[u8]) -> Option<Creds> {
    let root: Value = serde_json::from_slice(blob).ok()?;
    let tokens = root.get("tokens")?;
    let access = tokens.get("access_token")?.as_str()?;
    if access.is_empty() {
        return None;
    }
    let account_id = tokens.get("account_id")?.as_str()?.to_string();
    let id_token = tokens.get("id_token")?.as_str()?;
    let claims = decode_jwt_claims(id_token);
    let email = claims
        .as_ref()
        .and_then(|c| c.get("email"))
        .and_then(Value::as_str)
        .unwrap_or("Codex account")
        .to_string();
    let plan = claims
        .as_ref()
        .and_then(|c| c.get("https://api.openai.com/auth"))
        .and_then(|a| a.get("chatgpt_plan_type"))
        .and_then(Value::as_str)
        .map(capitalize)
        .unwrap_or_default();
    Some(Creds {
        access_token: access.to_string(),
        refresh_token: tokens
            .get("refresh_token")
            .and_then(Value::as_str)
            .map(String::from),
        account_id,
        email,
        plan_label: plan,
    })
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Decode (without verifying) the claims of a JWT.
fn decode_jwt_claims(jwt: &str) -> Option<Value> {
    let payload = jwt.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Live usage for one account's credentials. `SessionExpired` on 401/403.
pub async fn fetch_usage(client: &reqwest::Client, creds: &Creds) -> Result<Usage, ApiError> {
    let resp = client
        .get(USAGE_URL)
        .header("Authorization", format!("Bearer {}", creds.access_token))
        .header("chatgpt-account-id", &creds.account_id)
        .header("User-Agent", "PitStop")
        .header("Accept", "application/json")
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| ApiError::Network(e.to_string()))?;
    let status = resp.status().as_u16();
    if status == 401 || status == 403 {
        // Token aged out. Surfaced as Unauthorized so it shares the app's
        // re-auth/refresh handling with Claude's equivalent.
        return Err(ApiError::Unauthorized);
    }
    if status == 429 {
        let ra = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.trim().parse::<f64>().ok());
        return Err(ApiError::RateLimited(ra));
    }
    if status != 200 {
        return Err(ApiError::Http(status));
    }
    let data = resp
        .bytes()
        .await
        .map_err(|e| ApiError::Network(e.to_string()))?;
    parse_usage(&data).map_err(|_| ApiError::Malformed)
}

/// Exchange a refresh token for fresh tokens. Used only for inactive accounts.
pub async fn refresh(client: &reqwest::Client, refresh_token: &str) -> Result<Refreshed, CodexError> {
    let body = json!({
        "grant_type": "refresh_token",
        "refresh_token": refresh_token,
        "client_id": CLIENT_ID,
    });
    let resp = client
        .post(TOKEN_URL)
        .header("Content-Type", "application/json")
        .timeout(Duration::from_secs(15))
        .json(&body)
        .send()
        .await
        .map_err(|_| CodexError::Malformed)?;
    let status = resp.status().as_u16();
    if status == 400 || status == 401 || status == 403 {
        return Err(CodexError::SessionExpired);
    }
    if status != 200 {
        return Err(CodexError::Malformed);
    }
    let root: Value = resp.json().await.map_err(|_| CodexError::Malformed)?;
    let access = root
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or(CodexError::Malformed)?;
    Ok(Refreshed {
        access_token: access.to_string(),
        refresh_token: root
            .get("refresh_token")
            .and_then(Value::as_str)
            .map(String::from),
        id_token: root.get("id_token").and_then(Value::as_str).map(String::from),
    })
}

/// Return a copy of `blob` with refreshed tokens patched into `tokens`,
/// leaving every other field untouched.
pub fn patching(blob: &[u8], refreshed: &Refreshed) -> Option<Vec<u8>> {
    let mut root: Value = serde_json::from_slice(blob).ok()?;
    {
        let tokens = root.get_mut("tokens")?.as_object_mut()?;
        tokens.insert("access_token".into(), json!(refreshed.access_token));
        if let Some(rt) = &refreshed.refresh_token {
            tokens.insert("refresh_token".into(), json!(rt));
        }
        if let Some(idt) = &refreshed.id_token {
            tokens.insert("id_token".into(), json!(idt));
        }
    }
    root.as_object_mut()?.insert(
        "last_refresh".into(),
        json!(Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)),
    );
    serde_json::to_vec(&root).ok()
}

/// Exchange an authorization code for Codex tokens. Form-urlencoded, no `state`
/// in the body — the shape the Codex CLI uses. Used only for re-login.
pub async fn exchange_code(
    client: &reqwest::Client,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<Refreshed, CodexError> {
    let params = [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("client_id", CLIENT_ID),
        ("code_verifier", verifier),
    ];
    let resp = client
        .post(TOKEN_URL)
        .form(&params)
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .map_err(|_| CodexError::Malformed)?;
    let status = resp.status().as_u16();
    if status == 400 || status == 401 || status == 403 {
        return Err(CodexError::SessionExpired);
    }
    if status != 200 {
        return Err(CodexError::Malformed);
    }
    let root: Value = resp.json().await.map_err(|_| CodexError::Malformed)?;
    let access = root
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or(CodexError::Malformed)?;
    Ok(Refreshed {
        access_token: access.to_string(),
        refresh_token: root.get("refresh_token").and_then(Value::as_str).map(String::from),
        id_token: root.get("id_token").and_then(Value::as_str).map(String::from),
    })
}

/// Decode identity (email + ChatGPT account id) from an id_token JWT.
/// Returns `(email, Option<account_id>)`, or `None` if the JWT cannot be parsed.
pub fn identity_from_id_token(id_token: &str) -> Option<(String, Option<String>)> {
    let claims = decode_jwt_claims(id_token)?;
    let email = claims
        .get("email")
        .and_then(Value::as_str)
        .or_else(|| {
            claims
                .get("https://api.openai.com/profile")
                .and_then(|p| p.get("email"))
                .and_then(Value::as_str)
        })?
        .to_string();
    let account_id = claims
        .get("https://api.openai.com/auth")
        .and_then(|a| a.get("chatgpt_account_id"))
        .and_then(Value::as_str)
        .map(String::from);
    Some((email, account_id))
}

fn parse_usage(data: &[u8]) -> Result<Usage, CodexError> {
    let root: Value = serde_json::from_slice(data).map_err(|_| CodexError::Malformed)?;
    let mut windows = Vec::new();
    if let Some(rl) = root.get("rate_limit").and_then(Value::as_object) {
        for key in ["primary_window", "secondary_window"] {
            if let Some(w) = window(rl.get(key)) {
                windows.push(w);
            }
        }
    }
    Ok(Usage {
        windows,
        fetched_at: Local::now(),
    })
}

fn window(any: Option<&Value>) -> Option<UsageWindow> {
    let d = any?.as_object()?;
    let used = d.get("used_percent").and_then(Value::as_f64)?;
    let seconds = d
        .get("limit_window_seconds")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let resets_at = d
        .get("reset_at")
        .and_then(Value::as_f64)
        .and_then(|s| DateTime::from_timestamp(s as i64, 0));
    Some(UsageWindow {
        label: window_label(seconds),
        used_percent: used,
        resets_at,
    })
}

/// A compact label for a window duration: "5h", "7d", "30d".
fn window_label(seconds: i64) -> String {
    if seconds <= 0 {
        return String::new();
    }
    if seconds % 86400 == 0 {
        return format!("{}d", seconds / 86400);
    }
    if seconds % 3600 == 0 {
        return format!("{}h", seconds / 3600);
    }
    format!("{}m", seconds / 60)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_jwt(claims: serde_json::Value) -> String {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
        let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap());
        format!("{header}.{payload}.")
    }

    #[test]
    fn identity_from_id_token_reads_email_and_account() {
        let jwt = make_jwt(json!({
            "email": "me@example.com",
            "https://api.openai.com/auth": { "chatgpt_account_id": "acc_123" }
        }));
        let (email, acc) = identity_from_id_token(&jwt).unwrap();
        assert_eq!(email, "me@example.com");
        assert_eq!(acc.as_deref(), Some("acc_123"));
    }

    #[test]
    fn identity_from_id_token_falls_back_to_profile_email() {
        let jwt = make_jwt(json!({
            "https://api.openai.com/profile": { "email": "p@example.com" }
        }));
        let (email, acc) = identity_from_id_token(&jwt).unwrap();
        assert_eq!(email, "p@example.com");
        assert!(acc.is_none());
    }
}
