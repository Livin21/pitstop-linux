//! Google Gemini provider, Antigravity surface. The live token lives in the
//! GNOME keyring (service=gemini, account=antigravity) as a go-keyring blob:
//! `"go-keyring-base64:" + base64(JSON)` where JSON =
//! `{"token":{access_token,token_type:"Bearer",refresh_token,expiry(ISO8601)},"auth_method":"consumer"}`.
//! Usage comes from cloudcode-pa.googleapis.com Code Assist; identity from
//! Google `userinfo` (the blob carries no email).

use crate::usage_api::{parse_iso8601, ApiError};
use crate::util::now_ms;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use chrono::{DateTime, Local, SecondsFormat, Utc};
use serde_json::{json, Value};
use std::time::Duration;

const GO_KEYRING_PREFIX: &str = "go-keyring-base64:";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const USERINFO_URL: &str = "https://openidconnect.googleapis.com/v1/userinfo";
const CODE_ASSIST_BASE: &str = "https://cloudcode-pa.googleapis.com/v1internal";
/// loadCodeAssist platform metadata. Mac sent DARWIN_ARM64; a neutral value is
/// used on Linux. **[verify]** the field does not gate the response (see spike).
const PLATFORM: &str = "PLATFORM_UNSPECIFIED";

/// Antigravity's public installed-app OAuth client (reverse-engineered).
pub const ANTIGRAVITY_CLIENT_ID: &str =
    "1071006060591-tmhssin2h21lcre235vtolojh4g403ep.apps.googleusercontent.com";
/// **[verify]** reverse-engineered; the spike confirms it refreshes/exchanges.
pub const ANTIGRAVITY_CLIENT_SECRET: &str = "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";
pub const SCOPES: &str = "https://www.googleapis.com/auth/cloud-platform \
https://www.googleapis.com/auth/userinfo.email \
https://www.googleapis.com/auth/userinfo.profile \
https://www.googleapis.com/auth/cclog \
https://www.googleapis.com/auth/experimentsandconfigs";

#[allow(dead_code)]
pub const PROVIDER: &str = "gemini";
/// The one surface on Linux; shown as the row's surface tag.
pub const SURFACE_TAG: &str = "Antigravity";

pub struct Creds {
    pub access_token: String,
    pub refresh_token: Option<String>,
    #[allow(dead_code)] // consumed when persisting refreshed blobs (Task 2)
    pub id_token: Option<String>,
    pub expiry_ms: f64, // ms epoch; 0 = unknown
}

impl Creds {
    /// Expired (with a 60s safety margin) when we have a known expiry in the past.
    pub fn is_expired(&self) -> bool {
        self.expiry_ms > 0.0 && now_ms() >= self.expiry_ms - 60_000.0
    }
}

pub struct Refreshed {
    pub access_token: String,
    pub id_token: Option<String>,
    pub expires_at_ms: f64,
}

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

#[allow(dead_code)] // aggregates consumed by the tray/menu-bar in Task 6
impl Usage {
    pub fn max_utilization(&self) -> f64 {
        self.windows
            .iter()
            .map(|w| w.used_percent)
            .fold(0.0, f64::max)
    }
    /// The highest-utilization window — the row's main bar + menu-bar %.
    pub fn binding(&self) -> Option<&UsageWindow> {
        self.windows.iter().max_by(|a, b| {
            a.used_percent
                .partial_cmp(&b.used_percent)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
    }

    /// Up-to-2 most-used models after the binding one (highest), dropping <0.5%.
    /// Returns `None` when there are no qualifying extras.
    pub fn extras_line(&self) -> Option<String> {
        let mut sorted: Vec<&UsageWindow> = self.windows.iter().collect();
        sorted.sort_by(|a, b| {
            b.used_percent
                .partial_cmp(&a.used_percent)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let extras: Vec<String> = sorted
            .into_iter()
            .skip(1) // drop the binding (highest)
            .filter(|w| w.used_percent >= 0.5)
            .take(2)
            .map(|w| format!("{} {}%", w.label, w.used_percent.round() as i64))
            .collect();
        if extras.is_empty() {
            None
        } else {
            Some(extras.join(" · "))
        }
    }
}

/// The Antigravity keyring value is `"go-keyring-base64:" + base64(JSON)`.
pub fn decode_go_keyring(raw: &str) -> Option<Vec<u8>> {
    let b64 = raw.trim().strip_prefix(GO_KEYRING_PREFIX)?;
    STANDARD.decode(b64).ok()
}

pub fn encode_go_keyring(json: &[u8]) -> String {
    format!("{GO_KEYRING_PREFIX}{}", STANDARD.encode(json))
}

/// Parse the opaque Antigravity keyring blob into tokens (no email — that comes
/// from `userinfo`). Returns `None` for a tokenless blob.
///
/// go-keyring wraps secrets it can't store directly as `go-keyring-base64:` +
/// base64(JSON); but on this machine Antigravity wrote the JSON **unwrapped**
/// (confirmed live — see the Task 1 spike). Accept both: strip the wrapper when
/// present, otherwise treat the raw bytes as the JSON.
pub fn antigravity_creds(blob: &[u8]) -> Option<Creds> {
    let raw = std::str::from_utf8(blob).ok()?;
    let json =
        decode_go_keyring(raw).unwrap_or_else(|| raw.trim().as_bytes().to_vec());
    let root: Value = serde_json::from_slice(&json).ok()?;
    let tok = root.get("token")?;
    let access = tok.get("access_token")?.as_str()?;
    if access.is_empty() {
        return None;
    }
    let expiry_ms = tok
        .get("expiry")
        .and_then(Value::as_str)
        .and_then(parse_iso8601)
        .map(|d| d.timestamp_millis() as f64)
        .unwrap_or(0.0);
    Some(Creds {
        access_token: access.to_string(),
        refresh_token: tok
            .get("refresh_token")
            .and_then(Value::as_str)
            .map(String::from),
        id_token: tok.get("id_token").and_then(Value::as_str).map(String::from),
        expiry_ms,
    })
}

/// "gemini-3.1-pro-preview" -> "3.1-pro" (drop `gemini-` prefix + `-preview`).
pub fn short_model_name(model_id: &str) -> String {
    let s = model_id.strip_prefix("gemini-").unwrap_or(model_id);
    s.strip_suffix("-preview").unwrap_or(s).to_string()
}

/// Collapse windows that share the same (shortened) label, keeping the one with
/// the highest `used_percent`. Antigravity can report two quota buckets whose
/// model ids shorten to the same label (spike finding — e.g. both
/// `gemini-3.1-flash-lite-preview` and `gemini-3.1-flash-lite` → `3.1-flash-lite`);
/// rendering both yields a duplicate label in the tray/menu-bar. Order is
/// preserved by first appearance of each label. Pure so it is unit-testable.
fn dedupe_windows(windows: Vec<UsageWindow>) -> Vec<UsageWindow> {
    let mut out: Vec<UsageWindow> = Vec::new();
    for w in windows {
        match out.iter_mut().find(|e| e.label == w.label) {
            Some(existing) if w.used_percent > existing.used_percent => *existing = w,
            Some(_) => {} // keep the already-recorded higher-usage window
            None => out.push(w),
        }
    }
    out
}

/// Parse a retrieveUserQuota response into per-model windows. Buckets missing
/// `remainingFraction` are skipped; windows colliding on the shortened label
/// are deduped (highest usage wins) so the UI never shows a duplicate label.
pub fn parse_quota(data: &[u8]) -> Usage {
    let root: Value = serde_json::from_slice(data).unwrap_or(Value::Null);
    let mut windows = Vec::new();
    if let Some(buckets) = root.get("buckets").and_then(Value::as_array) {
        for b in buckets {
            let Some(model) = b.get("modelId").and_then(Value::as_str) else {
                continue;
            };
            let Some(frac) = b.get("remainingFraction").and_then(Value::as_f64) else {
                continue;
            };
            let used = ((1.0 - frac) * 100.0).clamp(0.0, 100.0);
            let resets_at = b
                .get("resetTime")
                .and_then(Value::as_str)
                .and_then(parse_iso8601);
            windows.push(UsageWindow {
                label: short_model_name(model),
                used_percent: used,
                resets_at,
            });
        }
    }
    Usage {
        windows: dedupe_windows(windows),
        fetched_at: Local::now(),
    }
}

/// Build a fresh Antigravity go-keyring blob (used by re-login persist).
/// Always produces the `go-keyring-base64:`-wrapped form (the portable form).
#[allow(dead_code)] // consumed by the re-login flow (Task 2+)
pub fn build_antigravity_blob(
    access: &str,
    refresh: Option<&str>,
    id_token: Option<&str>,
    expiry_iso: &str,
) -> Vec<u8> {
    let mut token = serde_json::Map::new();
    token.insert("access_token".into(), json!(access));
    token.insert("token_type".into(), json!("Bearer"));
    token.insert("expiry".into(), json!(expiry_iso));
    if let Some(r) = refresh {
        token.insert("refresh_token".into(), json!(r));
    }
    if let Some(i) = id_token {
        token.insert("id_token".into(), json!(i));
    }
    let inner = json!({ "token": Value::Object(token), "auth_method": "consumer" });
    encode_go_keyring(&serde_json::to_vec(&inner).unwrap_or_default()).into_bytes()
}

/// Patch an existing Antigravity blob in place, preserving every other field
/// (notably `refresh_token`) and the blob's original form:
///
/// - if `old` started with `go-keyring-base64:`, the result is wrapped the same way.
/// - if `old` was raw JSON (the live form on this machine), the result stays raw JSON.
///
/// Never log the blob or token fields.
#[allow(dead_code)] // consumed by the re-login flow (Task 2+)
pub fn patch_antigravity_blob(
    old: &[u8],
    access: &str,
    refresh: Option<&str>,
    id_token: Option<&str>,
    expiry_iso: &str,
) -> Option<Vec<u8>> {
    let raw = std::str::from_utf8(old).ok()?;
    let is_wrapped = raw.trim().starts_with(GO_KEYRING_PREFIX);
    let inner = if is_wrapped {
        decode_go_keyring(raw)?
    } else {
        raw.trim().as_bytes().to_vec()
    };
    let mut root: Value = serde_json::from_slice(&inner).ok()?;
    {
        let tok = root.get_mut("token")?.as_object_mut()?;
        tok.insert("access_token".into(), json!(access));
        tok.insert("expiry".into(), json!(expiry_iso));
        if let Some(r) = refresh {
            tok.insert("refresh_token".into(), json!(r));
        }
        if let Some(i) = id_token {
            tok.insert("id_token".into(), json!(i));
        }
    }
    let serialized = serde_json::to_vec(&root).ok()?;
    if is_wrapped {
        Some(encode_go_keyring(&serialized).into_bytes())
    } else {
        Some(serialized)
    }
}

/// Parse loadCodeAssist -> (cloudaicompanionProject, short plan label).
pub fn parse_load_code_assist(data: &[u8]) -> (Option<String>, String) {
    let root: Value = serde_json::from_slice(data).unwrap_or(Value::Null);
    let project = root
        .get("cloudaicompanionProject")
        .and_then(Value::as_str)
        .map(String::from);
    let paid = root
        .get("paidTier")
        .and_then(|t| t.get("name"))
        .and_then(Value::as_str);
    let current = root
        .get("currentTier")
        .and_then(|t| t.get("name"))
        .and_then(Value::as_str);
    (project, plan_label(paid, current))
}

fn plan_label(paid: Option<&str>, current: Option<&str>) -> String {
    if let Some(p) = paid {
        if p.contains("Ultra") {
            return "Ultra".into();
        }
        if p.contains("Pro") {
            return "AI Pro".into();
        }
    }
    if let Some(c) = current {
        return c.replace("Gemini ", "");
    }
    "Code Assist".into()
}

/// ms-epoch -> RFC3339 string (`…Z`), for writing an Antigravity `expiry` field.
#[allow(dead_code)] // used to write the refreshed blob back to the keyring (Task 2)
pub fn expiry_iso(expires_at_ms: f64) -> String {
    DateTime::<Utc>::from_timestamp_millis(expires_at_ms as i64)
        .unwrap_or_else(Utc::now)
        .to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// Google refresh-token grant fields (form-urlencoded). Google does NOT rotate
/// the refresh token, so the caller keeps the existing one.
pub fn refresh_form(refresh_token: &str) -> Vec<(&'static str, String)> {
    vec![
        ("grant_type", "refresh_token".into()),
        ("refresh_token", refresh_token.to_string()),
        ("client_id", ANTIGRAVITY_CLIENT_ID.into()),
        ("client_secret", ANTIGRAVITY_CLIENT_SECRET.into()),
    ]
}

/// OAuth `authorization_code` exchange fields (form-urlencoded) for Google.
/// Consumed by `oauth::GeminiLoginAdapter::exchange` during re-login.
pub fn exchange_form(code: &str, verifier: &str, redirect_uri: &str) -> Vec<(&'static str, String)> {
    vec![
        ("grant_type", "authorization_code".into()),
        ("code", code.to_string()),
        ("redirect_uri", redirect_uri.to_string()),
        ("client_id", ANTIGRAVITY_CLIENT_ID.into()),
        ("client_secret", ANTIGRAVITY_CLIENT_SECRET.into()),
        ("code_verifier", verifier.to_string()),
    ]
}

fn retry_after(resp: &reqwest::Response) -> Option<f64> {
    resp.headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<f64>().ok())
}

/// Refresh the access token in memory (never rotates the stored refresh token).
pub async fn refresh(client: &reqwest::Client, refresh_token: &str) -> Result<Refreshed, ApiError> {
    let resp = client
        .post(TOKEN_URL)
        .form(&refresh_form(refresh_token))
        .timeout(Duration::from_secs(15))
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
        .unwrap_or(3600.0);
    Ok(Refreshed {
        access_token: access.to_string(),
        id_token: root.get("id_token").and_then(Value::as_str).map(String::from),
        expires_at_ms: now_ms() + expires_in * 1000.0,
    })
}

/// Resolve the account email for an access token (the blob carries none).
pub async fn fetch_email(client: &reqwest::Client, access_token: &str) -> Result<String, ApiError> {
    let resp = client
        .get(USERINFO_URL)
        .header("Authorization", format!("Bearer {access_token}"))
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| ApiError::Network(e.to_string()))?;
    let status = resp.status().as_u16();
    if status == 401 || status == 403 {
        return Err(ApiError::Unauthorized);
    }
    if status != 200 {
        return Err(ApiError::Http(status));
    }
    let root: Value = resp.json().await.map_err(|_| ApiError::Malformed)?;
    root.get("email")
        .and_then(Value::as_str)
        .map(String::from)
        .ok_or(ApiError::Malformed)
}

/// loadCodeAssist -> (project, plan chip). 429 -> RateLimited; 401/403 -> Unauthorized.
pub async fn load_project(
    client: &reqwest::Client,
    access_token: &str,
) -> Result<(Option<String>, String), ApiError> {
    let body = json!({"metadata":{"ideType":"IDE_UNSPECIFIED","platform":PLATFORM,"pluginType":"GEMINI"}});
    let data = code_assist(client, access_token, "loadCodeAssist", &body).await?;
    Ok(parse_load_code_assist(&data))
}

/// retrieveUserQuota -> per-model windows.
pub async fn fetch_usage(
    client: &reqwest::Client,
    access_token: &str,
    project: &str,
) -> Result<Usage, ApiError> {
    let body = json!({ "project": project });
    let data = code_assist(client, access_token, "retrieveUserQuota", &body).await?;
    Ok(parse_quota(&data))
}

async fn code_assist(
    client: &reqwest::Client,
    access_token: &str,
    method: &str,
    body: &Value,
) -> Result<Vec<u8>, ApiError> {
    let resp = client
        .post(format!("{CODE_ASSIST_BASE}:{method}"))
        .header("Authorization", format!("Bearer {access_token}"))
        .header("Content-Type", "application/json")
        .timeout(Duration::from_secs(15))
        .json(body)
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
    resp.bytes()
        .await
        .map(|b| b.to_vec())
        .map_err(|e| ApiError::Network(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn go_keyring_round_trip() {
        let inner = br#"{"token":{"access_token":"ya29.AAA"}}"#;
        let enc = encode_go_keyring(inner);
        assert!(enc.starts_with("go-keyring-base64:"));
        assert_eq!(decode_go_keyring(&enc).unwrap(), inner);
        assert!(decode_go_keyring("not-a-keyring-value").is_none());
    }

    #[test]
    fn antigravity_creds_parses_token() {
        let inner = br#"{"token":{"access_token":"ya29.AAA","refresh_token":"1//rt","expiry":"2026-07-01T20:00:00Z","token_type":"Bearer"},"auth_method":"consumer"}"#;
        let blob = encode_go_keyring(inner).into_bytes();
        let c = antigravity_creds(&blob).unwrap();
        assert_eq!(c.access_token, "ya29.AAA");
        assert_eq!(c.refresh_token.as_deref(), Some("1//rt"));
        assert!(c.expiry_ms > 0.0);
    }

    #[test]
    fn antigravity_creds_parses_unwrapped_json() {
        // On this machine the item is stored WITHOUT the go-keyring-base64 wrapper
        // (raw JSON) — confirmed by the Task 1 live spike. Must parse that too.
        let raw = br#"{"token":{"access_token":"ya29.BBB","refresh_token":"1//rt2","expiry":"2026-07-01T20:00:00Z","token_type":"Bearer"},"auth_method":"consumer"}"#;
        let c = antigravity_creds(raw).unwrap();
        assert_eq!(c.access_token, "ya29.BBB");
        assert_eq!(c.refresh_token.as_deref(), Some("1//rt2"));
        assert!(c.expiry_ms > 0.0);
    }

    #[test]
    fn short_model_name_drops_affixes() {
        assert_eq!(short_model_name("gemini-3.1-pro-preview"), "3.1-pro");
        assert_eq!(short_model_name("gemini-2.5-flash"), "2.5-flash");
        assert_eq!(short_model_name("plain"), "plain");
    }

    #[test]
    fn parse_quota_maps_buckets_and_skips_partial() {
        let data = br#"{"buckets":[
            {"modelId":"gemini-3.1-pro-preview","remainingFraction":0.78,"resetTime":"2026-07-01T20:00:00Z"},
            {"modelId":"gemini-2.5-flash","remainingFraction":0.95},
            {"modelId":"gemini-nano"}
        ]}"#;
        let u = parse_quota(data);
        assert_eq!(u.windows.len(), 2); // nano skipped (no remainingFraction)
        assert_eq!(u.windows[0].label, "3.1-pro");
        assert!((u.windows[0].used_percent - 22.0).abs() < 0.01);
        assert!(u.windows[0].resets_at.is_some());
        assert!((u.max_utilization() - 22.0).abs() < 0.01);
    }

    #[test]
    fn parse_quota_dedupes_colliding_short_labels() {
        // Spike finding: two quota buckets whose model ids both shorten to
        // "3.1-flash-lite" must collapse to a single window (highest usage wins),
        // otherwise the tray shows "3.1-flash-lite 0% · 3.1-flash-lite 0%".
        let data = br#"{"buckets":[
            {"modelId":"gemini-3.1-flash-lite-preview","remainingFraction":0.90,"resetTime":"2026-07-01T20:00:00Z"},
            {"modelId":"gemini-3.1-flash-lite","remainingFraction":0.50}
        ]}"#;
        let u = parse_quota(data);
        assert_eq!(u.windows.len(), 1, "colliding labels must collapse to one window");
        assert_eq!(u.windows[0].label, "3.1-flash-lite");
        // 1 - 0.50 = 50% (higher) beats 1 - 0.90 = 10%.
        assert!(
            (u.windows[0].used_percent - 50.0).abs() < 0.01,
            "deduped window must keep the higher usage, got {}",
            u.windows[0].used_percent
        );
    }

    #[test]
    fn parse_load_code_assist_project_and_plan() {
        let d = br#"{"cloudaicompanionProject":"proj-123","paidTier":{"name":"Google One AI Pro"}}"#;
        let (p, plan) = parse_load_code_assist(d);
        assert_eq!(p.as_deref(), Some("proj-123"));
        assert_eq!(plan, "AI Pro");
        let d2 = br#"{"currentTier":{"name":"Gemini Code Assist"}}"#;
        let (proj2, plan2) = parse_load_code_assist(d2);
        assert!(proj2.is_none());
        assert_eq!(plan2, "Code Assist");
    }

    #[test]
    fn build_and_patch_antigravity_blob_preserve_prefix_and_fields() {
        let built = build_antigravity_blob("acc", Some("rt"), None, "2026-07-01T20:00:00.000Z");
        let s = String::from_utf8(built.clone()).unwrap();
        assert!(s.starts_with("go-keyring-base64:"));
        let c = antigravity_creds(&built).unwrap();
        assert_eq!(c.access_token, "acc");
        assert_eq!(c.refresh_token.as_deref(), Some("rt"));

        let patched = patch_antigravity_blob(&built, "newacc", None, Some("idt"), "2026-08-01T00:00:00.000Z").unwrap();
        let ps = String::from_utf8(patched.clone()).unwrap();
        assert!(ps.starts_with("go-keyring-base64:"));
        let pc = antigravity_creds(&patched).unwrap();
        assert_eq!(pc.access_token, "newacc");
        assert_eq!(pc.refresh_token.as_deref(), Some("rt")); // preserved from old blob
        assert_eq!(pc.id_token.as_deref(), Some("idt"));
    }

    #[test]
    fn patch_antigravity_blob_raw_json_preserves_form() {
        // On this machine the real blob is stored as raw JSON (no go-keyring-base64: prefix).
        // patch must return raw JSON, NOT wrap it.
        let raw = br#"{"token":{"access_token":"old_acc","refresh_token":"1//rt","expiry":"2026-07-01T20:00:00Z","token_type":"Bearer"},"auth_method":"consumer"}"#;
        let patched = patch_antigravity_blob(raw, "new_acc", None, Some("idt"), "2026-08-01T00:00:00.000Z").unwrap();
        let s = String::from_utf8(patched.clone()).unwrap();
        assert!(!s.starts_with("go-keyring-base64:"), "raw JSON input must NOT be wrapped");
        let pc = antigravity_creds(&patched).unwrap();
        assert_eq!(pc.access_token, "new_acc");
        assert_eq!(pc.refresh_token.as_deref(), Some("1//rt")); // preserved
        assert_eq!(pc.id_token.as_deref(), Some("idt"));
    }

    #[test]
    fn patch_antigravity_blob_updates_or_preserves_refresh_token() {
        let built = build_antigravity_blob("acc", Some("OLD-RT"), None, "2026-07-01T20:00:00.000Z");
        // Some(refresh) updates it.
        let updated =
            patch_antigravity_blob(&built, "acc2", Some("NEW-RT"), None, "2026-08-01T00:00:00.000Z").unwrap();
        assert_eq!(antigravity_creds(&updated).unwrap().refresh_token.as_deref(), Some("NEW-RT"));
        // None preserves the stored one (Google omits refresh_token on re-consent).
        let preserved =
            patch_antigravity_blob(&built, "acc3", None, None, "2026-08-01T00:00:00.000Z").unwrap();
        assert_eq!(antigravity_creds(&preserved).unwrap().refresh_token.as_deref(), Some("OLD-RT"));
    }

    #[test]
    fn exchange_form_has_auth_code_grant() {
        let f = exchange_form("thecode", "theverifier", "http://127.0.0.1:5123/oauth2callback");
        assert!(f.contains(&("grant_type", "authorization_code".to_string())));
        assert!(f.contains(&("code", "thecode".to_string())));
        assert!(f.contains(&("code_verifier", "theverifier".to_string())));
        assert!(f.contains(&("redirect_uri", "http://127.0.0.1:5123/oauth2callback".to_string())));
        assert!(f.iter().any(|(k, v)| *k == "client_id" && v == ANTIGRAVITY_CLIENT_ID));
        assert!(f.iter().any(|(k, v)| *k == "client_secret" && v == ANTIGRAVITY_CLIENT_SECRET));
    }

    #[test]
    fn extras_line_drops_binding_and_zero() {
        let u = Usage {
            windows: vec![
                UsageWindow { label: "3-pro".into(), used_percent: 22.0, resets_at: None },
                UsageWindow { label: "2.5-flash".into(), used_percent: 5.0, resets_at: None },
                UsageWindow { label: "nano".into(), used_percent: 0.0, resets_at: None },
            ],
            fetched_at: Local::now(),
        };
        assert_eq!(u.extras_line().as_deref(), Some("2.5-flash 5%"));
        let empty = Usage { windows: vec![], fetched_at: Local::now() };
        assert_eq!(empty.extras_line(), None);
    }
}
