//! Anthropic's OAuth usage endpoint + token refresh — the same unofficial
//! surface Claude Code itself uses. Ported verbatim from `UsageAPI.swift`.

use crate::util::now_secs;
use chrono::{DateTime, Local, Utc};
use serde_json::{json, Value};
use std::fmt;
use std::time::Duration;

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const TOKEN_URL: &str = "https://console.anthropic.com/v1/oauth/token";
/// Claude Code's public OAuth client ID (PKCE public client — no secret).
const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";

#[derive(Debug, Clone)]
pub enum ApiError {
    Unauthorized,
    /// Rate limited; carries the `Retry-After` seconds if the server sent one.
    RateLimited(Option<f64>),
    Http(u16),
    Malformed,
    Network(String),
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApiError::Unauthorized => write!(f, "Token rejected — re-login needed"),
            ApiError::RateLimited(_) => write!(f, "Rate limited by Anthropic"),
            ApiError::Http(code) => write!(f, "HTTP {code} from Anthropic"),
            ApiError::Malformed => write!(f, "Unexpected response format"),
            ApiError::Network(why) => write!(f, "{why}"),
        }
    }
}
impl std::error::Error for ApiError {}

#[derive(Clone, Copy)]
pub struct UsageWindow {
    pub utilization: Option<f64>,
    pub resets_at: Option<DateTime<Utc>>,
}

#[derive(Clone)]
pub struct UsageReport {
    pub five_hour: Option<UsageWindow>,
    pub seven_day: Option<UsageWindow>,
    pub seven_day_opus: Option<UsageWindow>,
    pub seven_day_sonnet: Option<UsageWindow>,
    pub extra_usage_enabled: bool,
    pub extra_usage_utilization: Option<f64>,
    pub fetched_at: DateTime<Local>,
}

impl UsageReport {
    /// The binding constraint — whichever window is closest to its limit.
    pub fn max_utilization(&self) -> f64 {
        let a = self.five_hour.and_then(|w| w.utilization).unwrap_or(0.0);
        let b = self.seven_day.and_then(|w| w.utilization).unwrap_or(0.0);
        a.max(b)
    }

    /// The window driving `max_utilization`, for reset-time display.
    pub fn binding_window(&self) -> Option<UsageWindow> {
        let a = self.five_hour.and_then(|w| w.utilization).unwrap_or(0.0);
        let b = self.seven_day.and_then(|w| w.utilization).unwrap_or(0.0);
        if a >= b {
            self.five_hour
        } else {
            self.seven_day
        }
    }
}

pub struct Refreshed {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at_ms: f64,
}

pub async fn fetch_usage(
    client: &reqwest::Client,
    access_token: &str,
) -> Result<UsageReport, ApiError> {
    let resp = client
        .get(USAGE_URL)
        .header("Authorization", format!("Bearer {access_token}"))
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("Content-Type", "application/json")
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| ApiError::Network(e.to_string()))?;
    let status = resp.status().as_u16();
    if status == 401 || status == 403 {
        return Err(ApiError::Unauthorized);
    }
    if status == 429 {
        return Err(ApiError::RateLimited(retry_after(&resp)));
    }
    if status != 200 {
        return Err(ApiError::Http(status));
    }
    let data = resp
        .bytes()
        .await
        .map_err(|e| ApiError::Network(e.to_string()))?;
    parse(&data)
}

fn retry_after(resp: &reqwest::Response) -> Option<f64> {
    resp.headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<f64>().ok())
}

/// Parse a usage payload. The OAuth endpoint and claude.ai's web endpoint
/// return the same shape, so both fetch paths share this.
pub fn parse(data: &[u8]) -> Result<UsageReport, ApiError> {
    let root: Value = serde_json::from_slice(data).map_err(|_| ApiError::Malformed)?;
    if !root.is_object() {
        return Err(ApiError::Malformed);
    }
    let mut extra_enabled = false;
    let mut extra_util = None;
    if let Some(extra) = root.get("extra_usage").and_then(Value::as_object) {
        extra_enabled = extra.get("is_enabled").and_then(Value::as_bool).unwrap_or(false);
        extra_util = extra.get("utilization").and_then(Value::as_f64);
    }
    Ok(UsageReport {
        five_hour: window(root.get("five_hour")),
        seven_day: window(root.get("seven_day")),
        seven_day_opus: window(root.get("seven_day_opus")),
        seven_day_sonnet: window(root.get("seven_day_sonnet")),
        extra_usage_enabled: extra_enabled,
        extra_usage_utilization: extra_util,
        fetched_at: Local::now(),
    })
}

fn window(any: Option<&Value>) -> Option<UsageWindow> {
    let d = any?.as_object()?;
    let utilization = d.get("utilization").and_then(Value::as_f64);
    let resets_at = d
        .get("resets_at")
        .and_then(Value::as_str)
        .and_then(parse_iso8601);
    Some(UsageWindow {
        utilization,
        resets_at,
    })
}

/// Parse an ISO-8601 / RFC-3339 timestamp (with or without fractional seconds).
pub fn parse_iso8601(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

/// Standard OAuth refresh-token grant against Claude Code's public client.
/// Used only for saved (inactive) profiles whose tokens have gone stale.
pub async fn refresh(client: &reqwest::Client, refresh_token: &str) -> Result<Refreshed, ApiError> {
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
        .map_err(|e| ApiError::Network(e.to_string()))?;
    let status = resp.status().as_u16();
    if status == 400 || status == 401 || status == 403 {
        return Err(ApiError::Unauthorized);
    }
    if status != 200 {
        return Err(ApiError::Http(status));
    }
    let root: Value = resp.json().await.map_err(|_| ApiError::Malformed)?;
    let access = root
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or(ApiError::Malformed)?;
    let expires_in = root
        .get("expires_in")
        .and_then(Value::as_f64)
        .ok_or(ApiError::Malformed)?;
    Ok(Refreshed {
        access_token: access.to_string(),
        refresh_token: root
            .get("refresh_token")
            .and_then(Value::as_str)
            .map(String::from),
        expires_at_ms: (now_secs() + expires_in) * 1000.0,
    })
}
