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

/// A per-model weekly limit ("Fable", …) from the `limits[]` array's
/// `weekly_scoped` entries. An independent cap: hitting it blocks only that
/// model, but per user preference it still counts toward the binding number.
#[derive(Clone)]
pub struct ScopedWindow {
    #[allow(dead_code)] // consumed by later tasks (tray display)
    pub label: String,
    pub window: UsageWindow,
}

#[derive(Clone)]
pub struct UsageReport {
    pub five_hour: Option<UsageWindow>,
    pub seven_day: Option<UsageWindow>,
    pub scoped: Vec<ScopedWindow>,
    pub extra_usage_enabled: bool,
    pub extra_usage_utilization: Option<f64>,
    pub fetched_at: DateTime<Local>,
}

impl UsageReport {
    /// The binding constraint — whichever window is closest to its limit,
    /// now including per-model scoped weekly limits (Fable).
    pub fn max_utilization(&self) -> f64 {
        self.binding_window()
            .and_then(|w| w.utilization)
            .unwrap_or(0.0)
    }

    /// The window driving `max_utilization`, for reset-time display.
    /// First-wins on ties, so 5h beats 7d beats scoped at equal utilization.
    pub fn binding_window(&self) -> Option<UsageWindow> {
        let mut best: Option<UsageWindow> = None;
        let candidates = [self.five_hour, self.seven_day]
            .into_iter()
            .flatten()
            .chain(self.scoped.iter().map(|s| s.window));
        for w in candidates {
            let is_better = match best {
                None => true,
                Some(b) => w.utilization.unwrap_or(0.0) > b.utilization.unwrap_or(0.0),
            };
            if is_better {
                best = Some(w);
            }
        }
        best
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
    let empty: Vec<Value> = Vec::new();
    let limits = root
        .get("limits")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    // 5h/7d keep coming from the legacy top-level fields (more precision);
    // fall back to the limits[] session / weekly_all entries when absent.
    let mut five_hour = window(root.get("five_hour"));
    if five_hour.is_none() {
        five_hour = limit_window_by_kind(limits, "session");
    }
    let mut seven_day = window(root.get("seven_day"));
    if seven_day.is_none() {
        seven_day = limit_window_by_kind(limits, "weekly_all");
    }
    let scoped: Vec<ScopedWindow> = limits
        .iter()
        .filter(|e| e.get("kind").and_then(Value::as_str) == Some("weekly_scoped"))
        .filter_map(|e| {
            let window = limit_window_entry(e)?;
            let label = e
                .get("scope")
                .and_then(|s| s.get("model"))
                .and_then(|m| m.get("display_name"))
                .and_then(Value::as_str)
                .unwrap_or("Scoped")
                .to_string();
            Some(ScopedWindow { label, window })
        })
        .collect();
    Ok(UsageReport {
        five_hour,
        seven_day,
        scoped,
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

/// A `UsageWindow` from a `limits[]` entry: reads `percent` (NOT `utilization`)
/// plus `resets_at`. Returns `None` when `percent` is absent.
fn limit_window_entry(entry: &Value) -> Option<UsageWindow> {
    let percent = entry.get("percent").and_then(Value::as_f64)?;
    let resets_at = entry
        .get("resets_at")
        .and_then(Value::as_str)
        .and_then(parse_iso8601);
    Some(UsageWindow {
        utilization: Some(percent),
        resets_at,
    })
}

/// The first `limits[]` entry whose `kind` matches, as a `UsageWindow`.
fn limit_window_by_kind(limits: &[Value], kind: &str) -> Option<UsageWindow> {
    limits
        .iter()
        .find(|e| e.get("kind").and_then(Value::as_str) == Some(kind))
        .and_then(limit_window_entry)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_scoped_weekly_limit() {
        let data = br#"{
          "five_hour": {"utilization": 64.0, "resets_at": "2026-07-02T00:50:00.818202+00:00"},
          "seven_day": {"utilization": 7.0, "resets_at": "2026-07-05T00:00:00+00:00"},
          "limits": [
            {"kind": "session", "group": "session", "percent": 64, "resets_at": "2026-07-02T00:50:00.818202+00:00"},
            {"kind": "weekly_all", "group": "weekly", "percent": 7, "resets_at": "2026-07-05T00:00:00+00:00"},
            {"kind": "weekly_scoped", "group": "weekly", "percent": 13,
             "resets_at": "2026-07-05T00:00:00+00:00",
             "scope": {"model": {"id": null, "display_name": "Fable"}, "surface": null}}
          ]
        }"#;
        let report = parse(data).expect("valid payload");
        assert_eq!(report.scoped.len(), 1);
        assert_eq!(report.scoped[0].label, "Fable");
        assert_eq!(report.scoped[0].window.utilization, Some(13.0));
        assert!(report.scoped[0].window.resets_at.is_some());
        // Legacy top-level fields are preferred over the limits[] fallback.
        assert_eq!(report.five_hour.and_then(|w| w.utilization), Some(64.0));
        // 6-digit fractional-second reset parses.
        assert!(report.five_hour.and_then(|w| w.resets_at).is_some());
    }

    #[test]
    fn scoped_label_falls_back_to_scoped() {
        let data = br#"{"limits": [{"kind": "weekly_scoped", "percent": 5}]}"#;
        let report = parse(data).expect("valid payload");
        assert_eq!(report.scoped.len(), 1);
        assert_eq!(report.scoped[0].label, "Scoped");
        assert_eq!(report.scoped[0].window.utilization, Some(5.0));
    }

    #[test]
    fn falls_back_to_limits_for_main_windows() {
        let data = br#"{"limits": [
          {"kind": "session", "percent": 42, "resets_at": "2026-07-02T00:50:00+00:00"},
          {"kind": "weekly_all", "percent": 24}
        ]}"#;
        let report = parse(data).expect("valid payload");
        assert_eq!(report.five_hour.and_then(|w| w.utilization), Some(42.0));
        assert!(report.five_hour.and_then(|w| w.resets_at).is_some());
        assert_eq!(report.seven_day.and_then(|w| w.utilization), Some(24.0));
    }

    #[test]
    fn unknown_limit_kinds_ignored() {
        let data = br#"{"limits": [{"kind": "hourly_lunar", "percent": 99}], "five_hour": {"utilization": 1}}"#;
        let report = parse(data).expect("valid payload");
        assert!(report.scoped.is_empty());
        assert_eq!(report.max_utilization(), 1.0);
    }

    #[test]
    fn binding_includes_scoped() {
        let data = br#"{"five_hour": {"utilization": 10}, "seven_day": {"utilization": 20},
         "limits": [{"kind": "weekly_scoped", "percent": 95,
                     "resets_at": "2026-07-05T00:00:00+00:00",
                     "scope": {"model": {"display_name": "Fable"}}}]}"#;
        let report = parse(data).expect("valid payload");
        assert_eq!(report.max_utilization(), 95.0);
        // Fable's reset stamp drives threshold notifications when it is binding.
        assert!(report.binding_window().and_then(|w| w.resets_at).is_some());
    }
}
