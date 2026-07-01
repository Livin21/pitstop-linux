# Gemini Provider (Antigravity surface) Implementation Plan
> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (- [ ]) syntax for tracking.

**Goal:** Add Google Gemini (Antigravity surface) as a third provider — live Code Assist usage, account switching, auto-switch, and a Login safety net — reading/writing the Antigravity OAuth token from the GNOME keyring.
**Architecture:** Mirror the Codex provider (`codex.rs` + `codex_store.rs`), but the live store is the GNOME keyring (`service=gemini`, `account=antigravity`, go-keyring value) instead of a file. `gemini.rs` owns credential/blob parsing, Google token refresh, `loadCodeAssist` + `retrieveUserQuota` + `userinfo`; `gemini_store.rs` snapshots the opaque keyring blob to `~/.config/pitstop/accounts/` and swaps it back on switch; `secret_service.rs` is a thin go-keyring-compatible Secret Service client. `app.rs` gains `refresh_gemini`/`perform_gemini_switch` and wires Gemini into the menu, auto-switch, and menu-bar pool.
**Tech Stack:** `secret-service` (tokio runtime feature), `reqwest` (async, form + json), `serde_json`, `chrono`, `base64` (STANDARD), `anyhow`. Consumes `oauth.rs` (Plan 3).
**Depends on:**
- **Plan 3** — `oauth.rs`: `LoginAdapter` trait, `Pkce`, `FreshTokens`, `LoginIdentity`, `run_login`, and `perform_login`/`Action::Login`/`login_in_flight` wiring in `app.rs` + the generic Login-affordance rendering in `tray.rs`/`build_row`. This plan adds `GeminiLoginAdapter` and one dispatch arm; it does NOT re-create the Login pill.
- **Plan 2** — `Provider::dashboard_url()` (this plan only adds the `Gemini` match arm).
- **Plan 1** — per-window `record_usage_samples()` (this plan feeds Gemini windows into it; a conformance note is provided).

## Global Constraints
- Rust 2021; single tokio task (Engine::run tokio::select! loop over an mpsc Action channel + 120s timer); ksni tray; no new threads/locks in the render path. The `secret-service` client is `async` and runs on the same tokio task (no blocking of the select loop).
- Secrets only in 0600 files or the GNOME keyring; never logged.
- reqwest (async) for HTTP; serde/serde_json for JSON; chrono for time; anyhow for errors.
- Each task ends green: cargo build clean, cargo test passes, cargo clippy clean, one commit.
- **Re-login and every Gemini snapshot write ONLY the profile-snapshot file `~/.config/pitstop/accounts/gemini-<sanitized-email>.json` — never the live keyring item.** The live keyring is written back ONLY by an explicit user/auto **switch** (`switch_to`).
- Keep the `[verify]` tags on the reverse-engineered Antigravity client secret and the `loadCodeAssist` platform metadata; each has an explicit verification step (the spike).
---

### Task 1: SPIKE — read the Antigravity keyring item and reach Code Assist (GATE)
**Files:** Create: `src/secret_service.rs`, `src/gemini.rs` / Modify: `Cargo.toml`, `src/main.rs` / Test: `src/gemini.rs` `#[cfg(test)]`

**Interfaces:** Produces: `secret_service::get(service:&str, account:&str) -> anyhow::Result<Option<String>>`; `gemini::{decode_go_keyring, encode_go_keyring, antigravity_creds, short_model_name, parse_quota, parse_load_code_assist, refresh, fetch_email, load_project, fetch_usage, expiry_iso, refresh_form, Creds, Usage, UsageWindow, Refreshed, ANTIGRAVITY_CLIENT_ID, ANTIGRAVITY_CLIENT_SECRET, SCOPES}`. Consumes: `usage_api::ApiError`, `usage_api::parse_iso8601`, `util::now_ms`.

> **GATE:** This task proves the single biggest unknown: that PitStop can read the Antigravity token from the GNOME keyring, decode it, resolve the email via `userinfo`, and drive `loadCodeAssist` + `retrieveUserQuota`. It is verified by a **manual run** of `pitstop --gemini-spike`. **If the spike prints any `FAIL:` line (keyring item absent/locked/schema mismatch, or an endpoint rejects the token), PAUSE this plan (Feature 4) and report the exact `FAIL` line. The other four plans still ship.** Only proceed to Task 2 after a `PASS:` line.

- [ ] **Step 1: Write the failing test** — pure parsers the spike relies on. Append to a new `src/gemini.rs`:
```rust
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
}
```

- [ ] **Step 2: Run test, verify it fails** \n Run: `cargo test --lib gemini::tests` \n Expected: FAIL (`gemini.rs` does not yet exist / `mod gemini` not declared → `error[E0432]`/compile error).

- [ ] **Step 3: Minimal implementation.**
  (3a) `Cargo.toml` — add under `[dependencies]` (feature name is **[verify]**: `rt-tokio-crypto-rust` selects tokio + RustCrypto; if `cargo build` errors on an unknown feature, run `cargo add secret-service` and read the printed feature list, then pick the tokio+rust-crypto variant):
```toml
secret-service = { version = "4", features = ["rt-tokio-crypto-rust"] }
```
  (3b) `src/main.rs` — add module declarations (keep the existing alphabetical block; `mod gemini_store;` is added in Task 5):
```rust
mod gemini;
mod secret_service;
```
  (3c) `src/secret_service.rs` — thin go-keyring-compatible client (only `get` here; `set` lands in Task 2). The go-keyring schema is: attributes `{"service": <service>, "username": <account>}`, label `"<service>/<account>"`. Secret Service search matches items whose attributes *contain* the query pairs, so a `{service,username}` search finds the Antigravity item even if go-keyring wrote extra attributes.
```rust
//! go-keyring-compatible GNOME keyring (Secret Service) client. Used only by
//! the Gemini/Antigravity surface, whose live token lives in the keyring rather
//! than a file. Matches zalando/go-keyring's schema so PitStop can read the
//! item Antigravity wrote (attributes service+username, value carrying the
//! `go-keyring-base64:` prefix) and, on switch, write it back in place.

use anyhow::Result;
use secret_service::{EncryptionType, SecretService};
use std::collections::HashMap;

fn attrs<'a>(service: &'a str, account: &'a str) -> HashMap<&'a str, &'a str> {
    HashMap::from([("service", service), ("username", account)])
}

/// The raw secret string for a go-keyring item, or `None` if no item matches.
/// The returned string is opaque (for Antigravity it is `"go-keyring-base64:" + base64(JSON)`).
pub async fn get(service: &str, account: &str) -> Result<Option<String>> {
    let ss = SecretService::connect(EncryptionType::Dh).await?;
    let found = ss.search_items(attrs(service, account)).await?;
    let item = found
        .unlocked
        .into_iter()
        .next()
        .or_else(|| found.locked.into_iter().next());
    let Some(item) = item else {
        return Ok(None);
    };
    item.unlock().await?;
    let secret = item.get_secret().await?;
    Ok(Some(String::from_utf8(secret)?))
}
```
> **[verify] secret-service API:** the exact call shapes (`SecretService::connect(EncryptionType::Dh)`, `search_items` returning `.unlocked`/`.locked`, `item.unlock()`, `item.get_secret()`) are v4.x. If `cargo build` reports a signature mismatch, consult `cargo doc -p secret-service --open` and adjust; the build compiling is the verification.

  (3d) `src/gemini.rs` — the network + parsing layer the spike exercises:
```rust
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

pub const PROVIDER: &str = "gemini";
/// The one surface on Linux; shown as the row's surface tag.
pub const SURFACE_TAG: &str = "Antigravity";

pub struct Creds {
    pub access_token: String,
    pub refresh_token: Option<String>,
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

impl Usage {
    pub fn max_utilization(&self) -> f64 {
        self.windows.iter().map(|w| w.used_percent).fold(0.0, f64::max)
    }
    /// The highest-utilization window — the row's main bar + menu-bar %.
    pub fn binding(&self) -> Option<&UsageWindow> {
        self.windows
            .iter()
            .max_by(|a, b| a.used_percent.partial_cmp(&b.used_percent).unwrap_or(std::cmp::Ordering::Equal))
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
/// from `userinfo`). Returns `None` for a non-go-keyring or tokenless blob.
pub fn antigravity_creds(blob: &[u8]) -> Option<Creds> {
    let raw = std::str::from_utf8(blob).ok()?;
    let json = decode_go_keyring(raw)?;
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
        refresh_token: tok.get("refresh_token").and_then(Value::as_str).map(String::from),
        id_token: tok.get("id_token").and_then(Value::as_str).map(String::from),
        expiry_ms,
    })
}

/// "gemini-3.1-pro-preview" -> "3.1-pro" (drop `gemini-` prefix + `-preview`).
pub fn short_model_name(model_id: &str) -> String {
    let s = model_id.strip_prefix("gemini-").unwrap_or(model_id);
    s.strip_suffix("-preview").unwrap_or(s).to_string()
}

/// Parse a retrieveUserQuota response into per-model windows. Buckets missing
/// `remainingFraction` are skipped.
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
            let resets_at = b.get("resetTime").and_then(Value::as_str).and_then(parse_iso8601);
            windows.push(UsageWindow {
                label: short_model_name(model),
                used_percent: used,
                resets_at,
            });
        }
    }
    Usage {
        windows,
        fetched_at: Local::now(),
    }
}

/// Parse loadCodeAssist -> (cloudaicompanionProject, short plan label).
pub fn parse_load_code_assist(data: &[u8]) -> (Option<String>, String) {
    let root: Value = serde_json::from_slice(data).unwrap_or(Value::Null);
    let project = root.get("cloudaicompanionProject").and_then(Value::as_str).map(String::from);
    let paid = root.get("paidTier").and_then(|t| t.get("name")).and_then(Value::as_str);
    let current = root.get("currentTier").and_then(|t| t.get("name")).and_then(Value::as_str);
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
    let access = root.get("access_token").and_then(Value::as_str).ok_or(ApiError::Malformed)?;
    let expires_in = root.get("expires_in").and_then(Value::as_f64).unwrap_or(3600.0);
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
    root.get("email").and_then(Value::as_str).map(String::from).ok_or(ApiError::Malformed)
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
```
  (3e) `src/main.rs` — add the spike subcommand near the existing `--check` handling (inside `main`, before `run_tray().await`):
```rust
    if args.iter().any(|a| a == "--gemini-spike") {
        gemini_spike().await;
        return Ok(());
    }
```
  and the function (place beside `check`):
```rust
/// `pitstop --gemini-spike` — GATE for Feature 4. Prove we can read the
/// Antigravity keyring item, decode it, resolve the email, and drive Code
/// Assist. Any `FAIL:` line means PAUSE Feature 4 (the other plans still ship).
async fn gemini_spike() {
    println!("== Gemini/Antigravity spike (Feature 4 gate) ==");
    let raw = match secret_service::get("gemini", "antigravity").await {
        Ok(Some(s)) => s,
        Ok(None) => {
            println!("FAIL: no keyring item service=gemini account=antigravity — PAUSE Feature 4");
            return;
        }
        Err(e) => {
            println!("FAIL: keyring read error: {e} — PAUSE Feature 4");
            return;
        }
    };
    println!("keyring value has go-keyring prefix: {}", raw.starts_with("go-keyring-base64:"));
    let Some(creds) = gemini::antigravity_creds(raw.as_bytes()) else {
        println!("FAIL: could not decode go-keyring blob — PAUSE Feature 4");
        return;
    };
    let client = reqwest::Client::new();
    let access = if creds.is_expired() {
        match &creds.refresh_token {
            Some(rt) => match gemini::refresh(&client, rt).await {
                Ok(r) => r.access_token,
                Err(e) => {
                    println!("FAIL: refresh: {e} — PAUSE Feature 4");
                    return;
                }
            },
            None => {
                println!("FAIL: token expired and no refresh_token — PAUSE Feature 4");
                return;
            }
        }
    } else {
        creds.access_token.clone()
    };
    match gemini::fetch_email(&client, &access).await {
        Ok(email) => println!("userinfo email: {email}"),
        Err(e) => {
            println!("FAIL: userinfo: {e} — PAUSE Feature 4");
            return;
        }
    }
    match gemini::load_project(&client, &access).await {
        Ok((Some(project), plan)) => {
            println!("loadCodeAssist project: {project}  plan: {plan}");
            match gemini::fetch_usage(&client, &access, &project).await {
                Ok(u) => {
                    println!("PASS: retrieveUserQuota returned {} buckets", u.windows.len());
                    for w in &u.windows {
                        println!("  {} {}%", w.label, w.used_percent.round());
                    }
                }
                Err(e) => println!("FAIL: retrieveUserQuota: {e} — PAUSE Feature 4"),
            }
        }
        Ok((None, plan)) => {
            println!("PARTIAL: no cloudaicompanionProject (plan {plan}) — presence-only; proceed with caution")
        }
        Err(e) => println!("FAIL: loadCodeAssist: {e} — PAUSE Feature 4"),
    }
}
```

- [ ] **Step 4: Run test, verify it passes** \n Run: `cargo test --lib gemini::tests` \n Expected: PASS (5 tests). \n Then the **manual GATE**: `cargo run -- --gemini-spike` — Expected: a `PASS: retrieveUserQuota returned N buckets` line. If any `FAIL:` appears, **stop and report; do not continue this plan**.

- [ ] **Step 5: Commit** \n `git add -A && git commit -m "gemini: keyring read + Code Assist spike (Feature 4 gate)"`

---

### Task 2: `secret_service::set` + live keyring round-trip
**Files:** Modify: `src/secret_service.rs` / Test: `src/secret_service.rs` `#[cfg(test)]` (ignored, live keyring)
**Interfaces:** Produces: `secret_service::set(service:&str, account:&str, value:&str) -> anyhow::Result<()>`. Consumes: `secret_service::get`.

> DBus/keyring calls cannot run in unit-test CI (no daemon). The round-trip is an `#[ignore]`d integration test, run manually with a live keyring; it proves write-back preserves the go-keyring encoding (attributes + prefixed value).

- [ ] **Step 1: Write the failing test** — append to `src/secret_service.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;

    // Run manually against a live GNOME keyring: `cargo test secret_service -- --ignored`.
    #[tokio::test]
    #[ignore = "needs a live GNOME keyring/Secret Service daemon"]
    async fn set_get_round_trip_preserves_prefix() {
        let value = "go-keyring-base64:eyJhIjoxfQ=="; // opaque go-keyring string
        set("pitstop-selftest", "rt", value).await.unwrap();
        let got = get("pitstop-selftest", "rt").await.unwrap();
        assert_eq!(got.as_deref(), Some(value));
    }
}
```

- [ ] **Step 2: Run test, verify it fails** \n Run: `cargo test --lib secret_service` \n Expected: FAIL (`error[E0425]: cannot find function \`set\``).

- [ ] **Step 3: Minimal implementation** — add to `src/secret_service.rs`:
```rust
/// Create-or-replace a go-keyring item (matched by attributes service+username),
/// storing `value` verbatim (Antigravity's `go-keyring-base64:` string). Label
/// `"<service>/<account>"` matches go-keyring's schema.
pub async fn set(service: &str, account: &str, value: &str) -> Result<()> {
    let ss = SecretService::connect(EncryptionType::Dh).await?;
    let collection = ss.get_default_collection().await?;
    collection.ensure_unlocked().await?;
    collection
        .create_item(
            &format!("{service}/{account}"),
            attrs(service, account),
            value.as_bytes(),
            true, // replace an existing item with the same attributes
            "text/plain",
        )
        .await?;
    Ok(())
}
```
> **[verify] write-back schema (spike-adjacent):** after the first real switch (Task 8), confirm Antigravity still reads the item and that `secret-tool search service gemini username antigravity` (or D-Feet) shows exactly one item — i.e. `replace: true` updated the existing item rather than creating a duplicate. If a duplicate appears, go-keyring wrote extra attributes; add them to `attrs()` to match.

- [ ] **Step 4: Run test, verify it passes** \n Run: `cargo build && cargo test --lib secret_service -- --ignored` (with a live keyring) \n Expected: PASS. In CI (no keyring) the test is skipped; `cargo build` clean is the gate.

- [ ] **Step 5: Commit** \n `git add -A && git commit -m "secret_service: go-keyring set + round-trip test"`

---

### Task 3: `model.rs` — add `Provider::Gemini` + `Source::Gemini`
**Files:** Modify: `src/model.rs:8-61` (Provider, Source, MenuAccount), plus the `dashboard_url` arm / Test: `src/model.rs` `#[cfg(test)]`
**Interfaces:** Produces: `Provider::Gemini`, `Provider::ALL` (len 3), `Provider::title()=="Gemini"`, `Provider::dashboard_url()` Gemini arm, `Source::Gemini`, `MenuAccount::is_gemini()`, `MenuAccount::provider()`→Gemini, `MenuAccount::key()`→`"gemini:<email>"`.

- [ ] **Step 1: Write the failing test** — add to `src/model.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gemini_provider_and_account_key() {
        assert_eq!(Provider::Gemini.title(), "Gemini");
        assert_eq!(Provider::ALL.len(), 3);
        assert_eq!(Provider::Gemini.dashboard_url(), Some("https://gemini.google.com/usage"));
        let a = MenuAccount {
            email: "me@x".into(),
            source: Source::Gemini,
            plan_label: "AI Pro".into(),
            is_active: false,
        };
        assert!(a.is_gemini());
        assert!(!a.is_codex());
        assert_eq!(a.key(), "gemini:me@x");
        assert_eq!(a.provider().title(), "Gemini");
    }
}
```

- [ ] **Step 2: Run test, verify it fails** \n Run: `cargo test --lib model::tests` \n Expected: FAIL (`no variant \`Gemini\``, `no method \`is_gemini\``).

- [ ] **Step 3: Minimal implementation** — in `src/model.rs`:
  - `Provider` enum (line 9-12): add `Gemini` after `Codex`.
  - `Provider::title` (line 15-20): add arm `Provider::Gemini => "Gemini",`.
  - `Provider::ALL` (line 21): `pub const ALL: [Provider; 3] = [Provider::Claude, Provider::Codex, Provider::Gemini];`.
  - `Provider::dashboard_url` — **added by Plan 2** with Claude/Codex arms; add the Gemini arm so the method reads exactly:
```rust
    pub fn dashboard_url(&self) -> Option<&'static str> {
        Some(match self {
            Provider::Claude => "https://claude.ai/new#settings/usage",
            Provider::Codex => "https://chatgpt.com/codex/cloud/settings/analytics#usage",
            Provider::Gemini => "https://gemini.google.com/usage",
        })
    }
```
> If Plan 2 has not landed and `dashboard_url` does not exist yet, add the whole method above.
  - `Source` enum (line 25-28): add `Gemini` after `Codex`.
  - `MenuAccount` impl (line 41-61): rewrite `is_codex`/`provider`/`key` and add `is_gemini`:
```rust
    pub fn is_codex(&self) -> bool {
        self.source == Source::Codex
    }
    pub fn is_gemini(&self) -> bool {
        self.source == Source::Gemini
    }
    pub fn provider(&self) -> Provider {
        match self.source {
            Source::Codex => Provider::Codex,
            Source::Gemini => Provider::Gemini,
            Source::Code => Provider::Claude,
        }
    }
    pub fn key(&self) -> String {
        match self.source {
            Source::Codex => format!("codex:{}", self.email),
            Source::Gemini => format!("gemini:{}", self.email),
            Source::Code => self.email.clone(),
        }
    }
```

- [ ] **Step 4: Run test, verify it passes** \n Run: `cargo test --lib model::tests` \n Expected: PASS. (`cargo build` also confirms `grouped_view`'s `for provider in Provider::ALL` still compiles — the empty Gemini group is skipped by its existing `if accounts.is_empty()` guard.)

- [ ] **Step 5: Commit** \n `git add -A && git commit -m "model: add Provider::Gemini + Source::Gemini (key gemini:<email>)"`

---

### Task 4: `gemini.rs` — blob builders + extras line
**Files:** Modify: `src/gemini.rs` / Test: `src/gemini.rs` `#[cfg(test)]`
**Interfaces:** Produces: `gemini::build_antigravity_blob(access:&str, refresh:Option<&str>, id_token:Option<&str>, expiry_iso:&str) -> Vec<u8>`; `gemini::patch_antigravity_blob(old:&[u8], access:&str, id_token:Option<&str>, expiry_iso:&str) -> Option<Vec<u8>>`; `Usage::extras_line(&self) -> Option<String>`.

- [ ] **Step 1: Write the failing test** — add to `gemini.rs`'s `tests` module:
```rust
    #[test]
    fn build_and_patch_antigravity_blob_preserve_prefix_and_fields() {
        let built = build_antigravity_blob("acc", Some("rt"), None, "2026-07-01T20:00:00.000Z");
        let s = String::from_utf8(built.clone()).unwrap();
        assert!(s.starts_with("go-keyring-base64:"));
        let c = antigravity_creds(&built).unwrap();
        assert_eq!(c.access_token, "acc");
        assert_eq!(c.refresh_token.as_deref(), Some("rt"));

        let patched = patch_antigravity_blob(&built, "newacc", Some("idt"), "2026-08-01T00:00:00.000Z").unwrap();
        let ps = String::from_utf8(patched.clone()).unwrap();
        assert!(ps.starts_with("go-keyring-base64:"));
        let pc = antigravity_creds(&patched).unwrap();
        assert_eq!(pc.access_token, "newacc");
        assert_eq!(pc.refresh_token.as_deref(), Some("rt")); // preserved from old blob
        assert_eq!(pc.id_token.as_deref(), Some("idt"));
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
```

- [ ] **Step 2: Run test, verify it fails** \n Run: `cargo test --lib gemini::tests` \n Expected: FAIL (`cannot find function \`build_antigravity_blob\``, `no method \`extras_line\``).

- [ ] **Step 3: Minimal implementation** — add to `src/gemini.rs` (blob builders after `parse_load_code_assist`; `extras_line` inside `impl Usage`):
```rust
/// Build a fresh Antigravity go-keyring blob (used by re-login persist).
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
/// (notably `refresh_token`) and the `go-keyring-base64:` prefix.
pub fn patch_antigravity_blob(
    old: &[u8],
    access: &str,
    id_token: Option<&str>,
    expiry_iso: &str,
) -> Option<Vec<u8>> {
    let raw = std::str::from_utf8(old).ok()?;
    let inner = decode_go_keyring(raw)?;
    let mut root: Value = serde_json::from_slice(&inner).ok()?;
    {
        let tok = root.get_mut("token")?.as_object_mut()?;
        tok.insert("access_token".into(), json!(access));
        tok.insert("expiry".into(), json!(expiry_iso));
        if let Some(i) = id_token {
            tok.insert("id_token".into(), json!(i));
        }
    }
    Some(encode_go_keyring(&serde_json::to_vec(&root).ok()?).into_bytes())
}
```
  and inside `impl Usage`:
```rust
    /// Up-to-2 most-used models after the binding one, dropping <0.5%.
    pub fn extras_line(&self) -> Option<String> {
        let mut sorted: Vec<&UsageWindow> = self.windows.iter().collect();
        sorted.sort_by(|a, b| b.used_percent.partial_cmp(&a.used_percent).unwrap_or(std::cmp::Ordering::Equal));
        let extras: Vec<String> = sorted
            .into_iter()
            .skip(1)
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
```

- [ ] **Step 4: Run test, verify it passes** \n Run: `cargo test --lib gemini::tests` \n Expected: PASS.

- [ ] **Step 5: Commit** \n `git add -A && git commit -m "gemini: antigravity blob build/patch + extras line"`

---

### Task 5: `gemini_store.rs` — keyring-backed store (snapshots + switch)
**Files:** Create: `src/gemini_store.rs` / Modify: `src/main.rs` (add `mod gemini_store;`) / Test: `src/gemini_store.rs` `#[cfg(test)]`
**Interfaces:** Produces: `gemini_store::PROVIDER`; `GeminiStore::{new, load, profiles, live_blob (async, assoc), write_live (async, assoc), snapshot, saved_blob, store_refreshed_blob, switch_to (async), remove}`; `GeminiProfile{email, saved_at, plan_label}`. Consumes: `secret_service::{get,set}`, `secret_store::{read,write,delete}`, `gemini::PROVIDER`, `util::{config_dir, now_secs, write_atomic}`.

- [ ] **Step 1: Write the failing test** — profile round-trip (pure, no keyring):
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_dict_round_trip() {
        let p = GeminiProfile { email: "me@x".into(), saved_at: 42.0, plan_label: "AI Pro".into() };
        let back = GeminiProfile::from_dict(&p.to_dict()).unwrap();
        assert_eq!(back.email, "me@x");
        assert_eq!(back.plan_label, "AI Pro");
        assert!((back.saved_at - 42.0).abs() < f64::EPSILON);
    }
}
```

- [ ] **Step 2: Run test, verify it fails** \n Run: `cargo test --lib gemini_store::tests` \n Expected: FAIL (module does not exist).

- [ ] **Step 3: Minimal implementation.** `src/main.rs` — add `mod gemini_store;` to the module block. Create `src/gemini_store.rs`:
```rust
//! Saved Gemini (Antigravity) accounts. The live store is the GNOME keyring
//! item `service=gemini, account=antigravity`; saved snapshots are 0600 files
//! under `~/.config/pitstop/accounts/` (the opaque `go-keyring-base64:` string,
//! stored via `secret_store`). Switching writes a saved snapshot back into the
//! keyring in place. Analogous to `codex_store`, but keyring- rather than
//! file-backed for the live surface.

use crate::gemini;
use crate::secret_service;
use crate::secret_store;
use crate::util::{config_dir, now_secs, write_atomic};
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::path::PathBuf;

/// secret_store namespace for Gemini snapshot files (`gemini-<email>.json`).
pub const PROVIDER: &str = "gemini";
const LIVE_SERVICE: &str = "gemini";
const LIVE_ACCOUNT: &str = "antigravity";

pub struct GeminiProfile {
    pub email: String,
    pub saved_at: f64,
    pub plan_label: String,
}

impl GeminiProfile {
    fn from_dict(d: &Value) -> Option<GeminiProfile> {
        Some(GeminiProfile {
            email: d.get("email")?.as_str()?.to_string(),
            saved_at: d.get("savedAt").and_then(Value::as_f64).unwrap_or(0.0),
            plan_label: d.get("planLabel").and_then(Value::as_str).unwrap_or_default().to_string(),
        })
    }
    fn to_dict(&self) -> Value {
        json!({ "email": self.email, "savedAt": self.saved_at, "planLabel": self.plan_label })
    }
}

pub struct GeminiStore {
    pub profiles: Vec<GeminiProfile>,
}

impl GeminiStore {
    fn file() -> PathBuf {
        config_dir().join("gemini-profiles.json")
    }

    pub fn new() -> Self {
        let mut s = GeminiStore { profiles: vec![] };
        s.load();
        s
    }

    pub fn load(&mut self) {
        self.profiles = (|| -> Option<Vec<GeminiProfile>> {
            let data = std::fs::read(Self::file()).ok()?;
            let root: Value = serde_json::from_slice(&data).ok()?;
            let list = root.get("profiles")?.as_array()?;
            let mut v: Vec<GeminiProfile> = list.iter().filter_map(GeminiProfile::from_dict).collect();
            v.sort_by(|a, b| a.email.cmp(&b.email));
            Some(v)
        })()
        .unwrap_or_default();
    }

    fn save(&self) -> Result<()> {
        let arr: Vec<Value> = self.profiles.iter().map(GeminiProfile::to_dict).collect();
        let data = serde_json::to_vec_pretty(&json!({ "profiles": arr }))?;
        write_atomic(&Self::file(), &data, None)
    }

    /// The live Antigravity keyring blob (opaque go-keyring string), or `None`.
    pub async fn live_blob() -> Result<Option<Vec<u8>>> {
        Ok(secret_service::get(LIVE_SERVICE, LIVE_ACCOUNT).await?.map(String::into_bytes))
    }

    /// Write an opaque blob back into the live keyring item in place.
    pub async fn write_live(blob: &[u8]) -> Result<()> {
        let s = String::from_utf8(blob.to_vec())?;
        secret_service::set(LIVE_SERVICE, LIVE_ACCOUNT, &s).await
    }

    /// Snapshot `blob` under `email` to the profile file (skip if byte-identical),
    /// and upsert non-secret metadata. Never touches the live keyring.
    pub fn snapshot(&mut self, email: &str, blob: &[u8], plan_label: &str) -> Result<()> {
        if let Ok(Some(stored)) = secret_store::read(PROVIDER, email) {
            if stored == blob {
                return Ok(());
            }
        }
        secret_store::write(PROVIDER, email, blob)?;
        self.profiles.retain(|p| p.email != email);
        self.profiles.push(GeminiProfile {
            email: email.to_string(),
            saved_at: now_secs(),
            plan_label: plan_label.to_string(),
        });
        self.profiles.sort_by(|a, b| a.email.cmp(&b.email));
        self.save()
    }

    /// The saved snapshot blob for `email`.
    pub fn saved_blob(&self, email: &str) -> Result<Option<Vec<u8>>> {
        secret_store::read(PROVIDER, email)
    }

    /// Persist a snapshot whose token PitStop refreshed itself (inactive only).
    pub fn store_refreshed_blob(&self, data: &[u8], email: &str) -> Result<()> {
        secret_store::write(PROVIDER, email, data)
    }

    /// Make `email` the live Antigravity account by writing its saved blob into
    /// the keyring. The caller MUST snapshot the outgoing live account first
    /// (via `snapshot`) so its refresh token isn't stranded.
    pub async fn switch_to(&self, email: &str) -> Result<()> {
        let Some(blob) = self.saved_blob(email)? else {
            return Err(anyhow!(
                "No saved Gemini credentials for {email} — sign in once with Antigravity and save again"
            ));
        };
        Self::write_live(&blob).await
    }

    pub fn remove(&mut self, email: &str) -> Result<()> {
        secret_store::delete(PROVIDER, email)?;
        self.profiles.retain(|p| p.email != email);
        self.save()
    }
}
```
> `snapshot` is `&mut self` (updates `profiles`); the outgoing-account snapshot before a switch is done in `app.rs::perform_gemini_switch` (Task 8), mirroring how `codex_store::switch_to` calls `capture_current` internally but here needs the app-resolved live email.

- [ ] **Step 4: Run test, verify it passes** \n Run: `cargo test --lib gemini_store::tests` \n Expected: PASS. (`cargo build` confirms the async assoc fns compile.)

- [ ] **Step 5: Commit** \n `git add -A && git commit -m "gemini_store: keyring-backed snapshots + switch"`

---

### Task 6: `app.rs` — refresh_gemini + fetch_gemini_usage + menu-bar pool
**Files:** Modify: `src/app.rs` (Engine struct `57-80`, `new` `83-105`, `fetch_pass` `233-261`, `record_usage_samples` `401-424`, `menu_bar_reading` MostUrgent `847-878`) / Test: `src/app.rs` `#[cfg(test)]`
**Interfaces:** Produces: `Engine::refresh_gemini`, `Engine::fetch_gemini_usage`, `Engine::gemini_email`, free fn `menu_label(&str)->String`. Consumes: `gemini::*`, `gemini_store::GeminiStore`, `Provider::Gemini`.

- [ ] **Step 1: Write the failing test** — add a `tests` module at the end of `src/app.rs` (or extend it if Plan 3 added one):
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_label_namespaces_providers() {
        assert_eq!(menu_label("me@x"), "me@x"); // Claude (bare email)
        assert_eq!(menu_label("codex:me@x"), "me@x (Codex)");
        assert_eq!(menu_label("gemini:me@x"), "me@x (Gemini)");
    }
}
```

- [ ] **Step 2: Run test, verify it fails** \n Run: `cargo test --lib app::tests::menu_label_namespaces_providers` \n Expected: FAIL (`cannot find function \`menu_label\``).

- [ ] **Step 3: Minimal implementation** — in `src/app.rs`:
  (6a) Imports: add `use crate::gemini;` and `use crate::gemini_store::GeminiStore;` beside the existing `use crate::codex;` / `use crate::codex_store::CodexStore;`.
  (6b) `Engine` struct: add fields after `codex_store: CodexStore,` and the usage maps:
```rust
    gemini_store: GeminiStore,
    gemini_live_email: Option<String>,
    gemini_usage: HashMap<String, gemini::Usage>, // key: "gemini:<email>"
    gemini_plan: HashMap<String, String>,         // key: email -> plan chip
    gemini_email_cache: HashMap<String, String>,  // access_token -> email
```
  (6c) `Engine::new`: initialize them:
```rust
            gemini_store: GeminiStore::new(),
            gemini_live_email: None,
            gemini_usage: HashMap::new(),
            gemini_plan: HashMap::new(),
            gemini_email_cache: HashMap::new(),
```
  (6d) `fetch_pass` (after `self.refresh_codex().await;`, before `self.last_refresh = ...`): add `self.refresh_gemini().await;`.
  (6e) Add the refresh methods (place after `fetch_codex_usage`, ~line 397):
```rust
    async fn refresh_gemini(&mut self) {
        // Resolve + snapshot the live Antigravity account (keyring).
        let live_blob = match GeminiStore::live_blob().await {
            Ok(b) => b,
            Err(e) => {
                self.last_top_level_error = Some(e.to_string());
                None
            }
        };
        self.gemini_live_email = None;
        if let Some(blob) = &live_blob {
            if let Some(creds) = gemini::antigravity_creds(blob) {
                if let Ok(email) = self.gemini_email(&creds).await {
                    self.gemini_live_email = Some(email.clone());
                    let plan = self.gemini_plan.get(&email).cloned().unwrap_or_default();
                    if let Err(e) = self.gemini_store.snapshot(&email, blob, &plan) {
                        self.last_top_level_error = Some(e.to_string());
                    }
                }
            }
        }
        self.gemini_store.load();

        let emails: Vec<String> = self.gemini_store.profiles.iter().map(|p| p.email.clone()).collect();
        for email in emails {
            let key = format!("gemini:{email}");
            if !self.passed_backoff_gate(&key) {
                continue;
            }
            let is_live = Some(&email) == self.gemini_live_email.as_ref();
            match self.fetch_gemini_usage(&email, is_live).await {
                Ok((usage, plan)) => {
                    self.gemini_usage.insert(key.clone(), usage);
                    if !plan.is_empty() {
                        self.gemini_plan.insert(email.clone(), plan);
                    }
                    self.clear_fetch_error(&key);
                }
                Err(e) => self.record_fetch_error(e, &key),
            }
        }
    }

    /// Resolve the email for a credential blob, caching by access token. Refreshes
    /// in memory first if the token has aged out (never persists the live token).
    async fn gemini_email(&mut self, creds: &gemini::Creds) -> Result<String, ApiError> {
        if let Some(e) = self.gemini_email_cache.get(&creds.access_token) {
            return Ok(e.clone());
        }
        let token = if creds.is_expired() {
            match &creds.refresh_token {
                Some(rt) => gemini::refresh(&self.client, rt).await?.access_token,
                None => creds.access_token.clone(),
            }
        } else {
            creds.access_token.clone()
        };
        let email = gemini::fetch_email(&self.client, &token).await?;
        self.gemini_email_cache.insert(creds.access_token.clone(), email.clone());
        Ok(email)
    }

    /// Usage + plan chip for one account. Refreshes an expired token in memory;
    /// persists the rotated token to the snapshot ONLY for inactive accounts.
    async fn fetch_gemini_usage(
        &self,
        email: &str,
        is_live: bool,
    ) -> Result<(gemini::Usage, String), ApiError> {
        let blob = if is_live {
            GeminiStore::live_blob().await.map_err(|e| ApiError::Network(e.to_string()))?
        } else {
            self.gemini_store.saved_blob(email).map_err(|e| ApiError::Network(e.to_string()))?
        }
        .ok_or(ApiError::Unauthorized)?;
        let creds = gemini::antigravity_creds(&blob).ok_or(ApiError::Unauthorized)?;
        let access = if creds.is_expired() {
            let rt = creds.refresh_token.clone().ok_or(ApiError::Unauthorized)?;
            let refreshed = gemini::refresh(&self.client, &rt).await?;
            if !is_live {
                if let Some(patched) = gemini::patch_antigravity_blob(
                    &blob,
                    &refreshed.access_token,
                    refreshed.id_token.as_deref(),
                    &gemini::expiry_iso(refreshed.expires_at_ms),
                ) {
                    self.gemini_store
                        .store_refreshed_blob(&patched, email)
                        .map_err(|e| ApiError::Network(e.to_string()))?;
                }
            }
            refreshed.access_token
        } else {
            creds.access_token.clone()
        };
        let (project, plan) = gemini::load_project(&self.client, &access).await?;
        let Some(project) = project else {
            // Signed in, no Code Assist project → presence-only row (no bar).
            return Ok((gemini::Usage { windows: vec![], fetched_at: chrono::Local::now() }, plan));
        };
        let usage = gemini::fetch_usage(&self.client, &access, &project).await?;
        Ok((usage, plan))
    }
```
  (6f) `record_usage_samples` (loop that pushes codex windows, ~line 409): add a Gemini block right after it:
```rust
        for (k, gu) in &self.gemini_usage {
            if !self.fetch_error.contains_key(k) {
                samples.push((k.clone(), gu.max_utilization()));
            }
        }
```
> **Plan 1 conformance:** if Plan 1's per-window sampling has landed (samples keyed `"{key}#{label}"`), instead push one sample per window: `for w in &gu.windows { samples.push((format!("{k}#{}", w.label), w.used_percent)); }` — mirror exactly how Plan 1 records `codex_usage` windows.
  (6g) `menu_bar_reading` MostUrgent arm: refactor the inline labels to a free fn and add Gemini. Add the free fn near `window_line` (bottom of file):
```rust
fn menu_label(key: &str) -> String {
    if let Some(e) = key.strip_prefix("codex:") {
        format!("{e} (Codex)")
    } else if let Some(e) = key.strip_prefix("gemini:") {
        format!("{e} (Gemini)")
    } else {
        key.to_string()
    }
}
```
  and in the MostUrgent arm replace the two `consider(...)` loops' label expressions with `menu_label(key)` and add a Gemini loop after the codex one:
```rust
                for (key, report) in &self.usage {
                    consider(menu_label(key), report.max_utilization(), self.fetch_error.contains_key(key));
                }
                for (key, cu) in &self.codex_usage {
                    consider(menu_label(key), cu.max_utilization(), self.fetch_error.contains_key(key));
                }
                for (key, gu) in &self.gemini_usage {
                    consider(menu_label(key), gu.max_utilization(), self.fetch_error.contains_key(key));
                }
```
> The default `ActiveClaudeCode` menu-bar source arm is unchanged (stays Claude-only).

- [ ] **Step 4: Run test, verify it passes** \n Run: `cargo test --lib app::tests::menu_label_namespaces_providers && cargo build` \n Expected: PASS + clean build. \n Manual: `cargo run` and confirm Gemini usage appears in the tray (or add a Gemini section to `--check` if convenient); the row-render itself lands in Task 7.

- [ ] **Step 5: Commit** \n `git add -A && git commit -m "app: refresh_gemini + fetch_gemini_usage + menu-bar pool"`

---

### Task 7: `app.rs` — Gemini rows (menu, headroom, summary, removable)
**Files:** Modify: `src/app.rs` (`accounts_for_menu` `652-673`, `headroom` `675-681`, `build_row` `709-780`, `summary_text` `895-937`, `build_view` removable `625-635`, `handle_action` Remove `182-194`) / Test: `src/app.rs` `#[cfg(test)]`
**Interfaces:** Produces: free fn `gemini_detail_lines(&gemini::Usage) -> Vec<String>`; Gemini rows in `accounts_for_menu`; `headroom` Gemini arm; `build_row` Gemini branch (windows + extras + projection + surface tag `Antigravity`).

- [ ] **Step 1: Write the failing test** — add to `app::tests`:
```rust
    #[test]
    fn gemini_detail_lines_include_windows_and_extras() {
        let u = gemini::Usage {
            windows: vec![
                gemini::UsageWindow { label: "3-pro".into(), used_percent: 22.0, resets_at: None },
                gemini::UsageWindow { label: "2.5-flash".into(), used_percent: 5.0, resets_at: None },
            ],
            fetched_at: chrono::Local::now(),
        };
        let lines = gemini_detail_lines(&u);
        // one line per window + one extras line
        assert_eq!(lines.len(), 3);
        assert!(lines.last().unwrap().contains("2.5-flash 5%"));
    }
```

- [ ] **Step 2: Run test, verify it fails** \n Run: `cargo test --lib app::tests::gemini_detail_lines_include_windows_and_extras` \n Expected: FAIL (`cannot find function \`gemini_detail_lines\``).

- [ ] **Step 3: Minimal implementation** — in `src/app.rs`:
  (7a) Add the free fn near `window_line`:
```rust
fn gemini_detail_lines(usage: &gemini::Usage) -> Vec<String> {
    let mut lines: Vec<String> = usage
        .windows
        .iter()
        .map(|w| window_line(&w.label, Some(w.used_percent), w.resets_at))
        .collect();
    if let Some(extras) = usage.extras_line() {
        lines.push(format!("       {extras}"));
    }
    lines
}
```
  (7b) `accounts_for_menu`: after the codex `for c in &self.codex_store.profiles` loop, add:
```rust
        for c in &self.gemini_store.profiles {
            let plan = self.gemini_plan.get(&c.email).cloned().unwrap_or_else(|| c.plan_label.clone());
            let plan_label = if plan.is_empty() {
                gemini::SURFACE_TAG.to_string()
            } else {
                format!("{plan} · {}", gemini::SURFACE_TAG)
            };
            rows.push(MenuAccount {
                email: c.email.clone(),
                source: Source::Gemini,
                plan_label,
                is_active: Some(&c.email) == self.gemini_live_email.as_ref(),
            });
        }
```
  (7c) `headroom`: add a Gemini arm (turn the `if/else` into a three-way):
```rust
    fn headroom(&self, a: &MenuAccount) -> f64 {
        if a.is_codex() {
            self.codex_usage.get(&a.key()).map(|u| u.max_utilization()).unwrap_or(999.0)
        } else if a.is_gemini() {
            self.gemini_usage.get(&a.key()).map(|u| u.max_utilization()).unwrap_or(999.0)
        } else {
            self.usage.get(&a.email).map(|r| r.max_utilization()).unwrap_or(999.0)
        }
    }
```
  (7d) `build_row`: insert a Gemini branch between the codex `if account.is_codex()` block and the `else if let Some(report) = self.usage.get(&key)` Claude block:
```rust
        } else if account.is_gemini() {
            if let Some(gu) = self.gemini_usage.get(&key) {
                data_date = Some(gu.fetched_at);
                for line in gemini_detail_lines(gu) {
                    detail.push(line);
                }
                if let Some(b) = gu.binding() {
                    binding_util = Some(b.used_percent);
                    binding_reset = b.resets_at;
                }
            }
        } else if let Some(report) = self.usage.get(&key) {
```
> The projection line (`self.projected_full(&key, util, binding_reset)`) and `row_status` below the branches already run generically off `binding_util`/`data_date`, so the Gemini row gets projection + status/backoff/Login handling for free. The **Login pill** (Plan 3) renders generically when `key ∈ needs_action && inactive && switchable`; a 401/403 sets `needs_action` for gemini keys via the existing `record_fetch_error` — no build_row change needed.
  (7e) `summary_text`: add a Gemini branch mirroring the codex one (compute `detail` when `acct.is_gemini()`):
```rust
            let detail = if acct.is_gemini() {
                self.gemini_usage
                    .get(&key)
                    .map(|gu| {
                        if gu.windows.is_empty() {
                            "—".to_string()
                        } else {
                            gu.windows
                                .iter()
                                .map(|w| format!("{} {}", w.label, format::percent(Some(w.used_percent))))
                                .collect::<Vec<_>>()
                                .join(" · ")
                        }
                    })
                    .or_else(|| self.fetch_error.get(&key).cloned())
                    .unwrap_or_else(|| "…".into())
            } else if acct.is_codex() {
```
  and extend the provider suffix: `let provider = if acct.is_codex() { " (Codex)" } else if acct.is_gemini() { " (Gemini)" } else { "" };`.
  (7f) `build_view` removable list: after the codex loop add:
```rust
        for p in &self.gemini_store.profiles {
            if Some(&p.email) != self.gemini_live_email.as_ref() {
                removable.push((format!("{} · Gemini", p.email), format!("gemini:{}", p.email)));
            }
        }
```
  (7g) `handle_action` `Action::Remove`: add a Gemini branch before the codex one:
```rust
                if let Some(email) = key.strip_prefix("gemini:") {
                    let _ = self.gemini_store.remove(email);
                    self.gemini_usage.remove(&key);
                } else if let Some(email) = key.strip_prefix("codex:") {
```

- [ ] **Step 4: Run test, verify it passes** \n Run: `cargo test --lib app::tests && cargo build && cargo clippy` \n Expected: PASS + clean. \n Manual: `cargo run` — a **Gemini** section shows the account with its binding bar, `3-pro 22% · 2.5-flash 5%` extras, plan `[AI Pro · Antigravity]`, and (with a saved second account) a `⮂ switch`.

- [ ] **Step 5: Commit** \n `git add -A && git commit -m "app: render Gemini rows (windows, extras, surface tag, remove)"`

---

### Task 8: `app.rs` — perform_gemini_switch + auto-switch + ToS caveat
**Files:** Modify: `src/app.rs` (`handle_action` Switch `158-166`, `evaluate_auto_switch` `507-564`, add `perform_gemini_switch`) / Test: `src/app.rs` `#[cfg(test)]`
**Interfaces:** Produces: `Engine::perform_gemini_switch`, free fn `gemini_switch_body(Option<String>)->String`. Consumes: `pick_auto_switch`, `Provider::Gemini`, `GeminiStore::{live_blob, switch_to}`, `gemini::antigravity_creds`.

- [ ] **Step 1: Write the failing test** — add to `app::tests`:
```rust
    #[test]
    fn gemini_switch_body_carries_tos_caveat() {
        let body = gemini_switch_body(None);
        assert!(body.contains("Antigravity"));
        assert!(body.to_lowercase().contains("terms") || body.to_lowercase().contains("discourage"));
        let custom = gemini_switch_body(Some("me@a hit 92% — moved to me@b (10% used).".into()));
        assert!(custom.contains("moved to me@b"));
        assert!(custom.to_lowercase().contains("discourage")); // caveat still appended
    }
```

- [ ] **Step 2: Run test, verify it fails** \n Run: `cargo test --lib app::tests::gemini_switch_body_carries_tos_caveat` \n Expected: FAIL (`cannot find function \`gemini_switch_body\``).

- [ ] **Step 3: Minimal implementation** — in `src/app.rs`:
  (8a) Add the caveat + body helper near `pick_auto_switch`:
```rust
const GEMINI_TOS_CAVEAT: &str =
    "Note: Antigravity's terms discourage rotating this token — switch sparingly.";

fn gemini_switch_body(reason: Option<String>) -> String {
    let base = reason.unwrap_or_else(|| {
        "New Antigravity sessions use this account. Quit and reopen Antigravity to pick it up.".into()
    });
    format!("{base}\n{GEMINI_TOS_CAVEAT}")
}
```
  (8b) Add the switch method (after `perform_codex_switch`, ~line 607):
```rust
    async fn perform_gemini_switch(&mut self, email: &str, auto: bool, reason: Option<String>) {
        // Snapshot the outgoing live account first so its refresh token isn't stranded.
        if let Some(live) = self.gemini_live_email.clone() {
            if let Ok(Some(blob)) = GeminiStore::live_blob().await {
                let plan = self.gemini_plan.get(&live).cloned().unwrap_or_default();
                let _ = self.gemini_store.snapshot(&live, &blob, &plan);
            }
        }
        match self.gemini_store.switch_to(email).await {
            Ok(()) => {
                self.gemini_live_email = Some(email.to_string());
                let title = if auto {
                    format!("Auto-switched Gemini to {email}")
                } else {
                    format!("Switched Gemini to {email}")
                };
                notify::post(&title, &gemini_switch_body(reason));
            }
            Err(e) => {
                self.last_top_level_error = Some(format!("Couldn't switch Gemini account: {e}"));
                notify::post("Couldn't switch Gemini account", &e.to_string());
            }
        }
    }
```
  (8c) `handle_action` `Action::Switch`: add a Gemini branch before codex:
```rust
                if let Some(email) = key.strip_prefix("gemini:") {
                    self.perform_gemini_switch(&email.to_string(), false, None).await;
                } else if let Some(email) = key.strip_prefix("codex:") {
                    self.perform_codex_switch(&email.to_string(), false, None).await;
                } else {
                    self.perform_switch(&key.clone(), false, None).await;
                }
```
  (8d) `evaluate_auto_switch`: after the codex block (before `switched`), add a symmetric Gemini block:
```rust
        let gemini_utils: Vec<(String, Option<f64>)> = self
            .gemini_store
            .profiles
            .iter()
            .map(|p| {
                let key = format!("gemini:{}", p.email);
                let u = if self.fetch_error.contains_key(&key) {
                    None
                } else {
                    self.gemini_usage.get(&key).map(|r| r.max_utilization())
                };
                (p.email.clone(), u)
            })
            .collect();
        if let Some((target, reason)) = pick_auto_switch(
            self.gemini_live_email.as_deref(),
            threshold,
            self.last_auto_switch.get(&Provider::Gemini).copied(),
            &gemini_utils,
        ) {
            self.last_auto_switch.insert(Provider::Gemini, Instant::now());
            self.perform_gemini_switch(&target, true, Some(reason)).await;
            switched = true;
        }
```

- [ ] **Step 4: Run test, verify it passes** \n Run: `cargo test --lib app::tests && cargo build && cargo clippy` \n Expected: PASS + clean. \n Manual E2E: with two saved Gemini accounts, click `⮂ switch` on the inactive row → notification shows the switch + the ToS caveat; confirm the keyring now holds the target's blob (spike/`secret-tool`), that Antigravity reads it, and that the previous account's saved snapshot round-trips back on a switch-back.

- [ ] **Step 5: Commit** \n `git add -A && git commit -m "app: Gemini switch + auto-switch + ToS caveat notification"`

---

### Task 9: `GeminiLoginAdapter` (conform to Plan 3's `LoginAdapter`) + dispatch
**Files:** Modify: `src/oauth.rs` (add `GeminiLoginAdapter`; owned by Plan 3), `src/app.rs` (`perform_login` dispatch; added by Plan 3), `src/gemini.rs` (add `exchange_form`) / Test: `src/oauth.rs` `#[cfg(test)]` and `src/gemini.rs` `#[cfg(test)]`
**Interfaces:** Consumes (Plan 3): `oauth::{LoginAdapter, Pkce, FreshTokens, LoginIdentity, run_login}`. Produces: `oauth::GeminiLoginAdapter`; `gemini::exchange_form(code, verifier, redirect_uri) -> Vec<(&'static str, String)>`.

> **Plan 3 conformance ([verify]):** Plan 3 finalizes whether `LoginAdapter` uses the `async-trait` crate or enum dispatch. Read `src/oauth.rs` first. If it uses `#[async_trait::async_trait]`, use the impl below verbatim. If Plan 3 chose enum dispatch, add a `Gemini` variant to its adapter enum and route each method to the same bodies. The trait method signatures are fixed by the SHARED CONTRACT (`authorize_url`, `fixed_loopback_port`, `redirect_path`, `supports_paste`, `exchange`, `identity`, `persist`).

- [ ] **Step 1: Write the failing test** — add to `gemini.rs` tests (pure exchange-form) and `oauth.rs` tests (authorize URL):
```rust
// in src/gemini.rs tests
    #[test]
    fn exchange_form_has_auth_code_grant() {
        let f = exchange_form("thecode", "theverifier", "http://127.0.0.1:5123/oauth2callback");
        assert!(f.contains(&("grant_type", "authorization_code".to_string())));
        assert!(f.contains(&("code", "thecode".to_string())));
        assert!(f.contains(&("code_verifier", "theverifier".to_string())));
        assert!(f.iter().any(|(k, v)| *k == "client_id" && v == ANTIGRAVITY_CLIENT_ID));
    }
```
```rust
// in src/oauth.rs tests
    #[test]
    fn gemini_authorize_url_has_google_params() {
        let pkce = Pkce { verifier: "v".into(), challenge: "chal".into(), state: "st".into() };
        let url = GeminiLoginAdapter.authorize_url(&pkce, "http://127.0.0.1:5123/oauth2callback", false);
        assert!(url.starts_with("https://accounts.google.com/o/oauth2/v2/auth?"));
        assert!(url.contains("code_challenge=chal"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("state=st"));
        assert!(url.contains("access_type=offline"));
        // client_id chars (`.` `-`) are unreserved, so it appears literally.
        assert!(url.contains("client_id=1071006060591-"));
        assert_eq!(GeminiLoginAdapter.redirect_path(), "/oauth2callback");
        assert_eq!(GeminiLoginAdapter.fixed_loopback_port(), None);
        assert!(!GeminiLoginAdapter.supports_paste());
    }
```

- [ ] **Step 2: Run test, verify it fails** \n Run: `cargo test --lib exchange_form_has_auth_code_grant gemini_authorize_url_has_google_params` \n Expected: FAIL (`cannot find function \`exchange_form\``, `cannot find type \`GeminiLoginAdapter\``).

- [ ] **Step 3: Minimal implementation.**
  (9a) `src/gemini.rs` — add the exchange form (beside `refresh_form`):
```rust
/// OAuth authorization_code exchange fields (form-urlencoded) for Google.
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
```
  (9b) `src/oauth.rs` — add `GeminiLoginAdapter` (adjust the trait-dispatch style to Plan 3's):
```rust
/// Google installed-app login for Gemini/Antigravity. Google accepts an
/// arbitrary loopback port → fully automatic (no paste fallback). Persist writes
/// ONLY the profile snapshot file, never the live keyring (shared-contract rule).
pub struct GeminiLoginAdapter;

#[async_trait::async_trait]
impl LoginAdapter for GeminiLoginAdapter {
    fn authorize_url(&self, pkce: &Pkce, redirect_uri: &str, _paste_mode: bool) -> String {
        let mut url = reqwest::Url::parse("https://accounts.google.com/o/oauth2/v2/auth").unwrap();
        url.query_pairs_mut()
            .append_pair("client_id", crate::gemini::ANTIGRAVITY_CLIENT_ID)
            .append_pair("response_type", "code")
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("scope", crate::gemini::SCOPES)
            .append_pair("code_challenge", &pkce.challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", &pkce.state)
            .append_pair("access_type", "offline")
            .append_pair("prompt", "consent");
        url.to_string()
    }

    fn fixed_loopback_port(&self) -> Option<u16> {
        None // ephemeral port; Google accepts any loopback
    }

    fn redirect_path(&self) -> &'static str {
        "/oauth2callback"
    }

    fn supports_paste(&self) -> bool {
        false
    }

    async fn exchange(
        &self,
        http: &reqwest::Client,
        code: &str,
        pkce: &Pkce,
        redirect_uri: &str,
    ) -> anyhow::Result<FreshTokens> {
        let resp = http
            .post("https://oauth2.googleapis.com/token")
            .form(&crate::gemini::exchange_form(code, &pkce.verifier, redirect_uri))
            .send()
            .await?;
        if !resp.status().is_success() {
            anyhow::bail!("Google token exchange failed: {}", resp.status());
        }
        let root: serde_json::Value = resp.json().await?;
        let access = root
            .get("access_token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("token exchange returned no access_token"))?;
        let expires_in = root.get("expires_in").and_then(|v| v.as_f64()).unwrap_or(3600.0);
        Ok(FreshTokens {
            access_token: access.to_string(),
            refresh_token: root.get("refresh_token").and_then(|v| v.as_str()).map(String::from),
            id_token: root.get("id_token").and_then(|v| v.as_str()).map(String::from),
            expires_at_ms: (crate::util::now_ms() + expires_in * 1000.0) as i64,
        })
    }

    async fn identity(&self, http: &reqwest::Client, t: &FreshTokens) -> anyhow::Result<LoginIdentity> {
        // The Antigravity blob has no email, so identity comes from `userinfo`.
        let email = crate::gemini::fetch_email(http, &t.access_token)
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        Ok(LoginIdentity { email, account_id: None })
    }

    async fn persist(&self, email: &str, t: &FreshTokens) -> anyhow::Result<()> {
        // Profile snapshot ONLY (never the live keyring).
        let iso = crate::gemini::expiry_iso(t.expires_at_ms as f64);
        let blob = crate::gemini::build_antigravity_blob(
            &t.access_token,
            t.refresh_token.as_deref(),
            t.id_token.as_deref(),
            &iso,
        );
        crate::secret_store::write(crate::gemini_store::PROVIDER, email, &blob)?;
        Ok(())
    }
}
```
  (9c) `src/app.rs` — in `perform_login` (added by Plan 3), add a Gemini dispatch arm alongside the Claude/Codex arms. Match Plan 3's existing shape; the effect must be:
```rust
        } else if let Some(email) = key.strip_prefix("gemini:") {
            let adapter = oauth::GeminiLoginAdapter;
            oauth::run_login(&self.client, &adapter, email).await
        }
```
> If Plan 3's `perform_login` selects the adapter via `key`/provider before calling `run_login` once, insert the `gemini:` case into that selector instead of duplicating the call. `run_login`'s persist heals the row on the next `refresh_all()` (401 clears → Login pill disappears).

- [ ] **Step 4: Run test, verify it passes** \n Run: `cargo test --lib exchange_form_has_auth_code_grant gemini_authorize_url_has_google_params && cargo build && cargo clippy` \n Expected: PASS + clean. \n Manual: force a Gemini 401 (revoke the token), confirm the Login pill appears on the inactive Gemini row, click it, complete the Google sign-in, and confirm the snapshot heals the row on the next refresh — and that the live keyring is unchanged.

- [ ] **Step 5: Commit** \n `git add -A && git commit -m "oauth: GeminiLoginAdapter (Google PKCE) + Login dispatch"`

---

### Task 10: Docs — README + CHANGELOG
**Files:** Modify: `README.md`; Create/Modify: `CHANGELOG.md` / Test: `docs presence` (grep assertions)
**Interfaces:** none (documentation).

- [ ] **Step 1: Write the failing test** — a small shell check (record it as the task's verification, not a Rust test):
```bash
grep -q "Antigravity" README.md && grep -qi "terms discourage" README.md && grep -q "Gemini" CHANGELOG.md
```

- [ ] **Step 2: Run test, verify it fails** \n Run: the grep above \n Expected: FAIL (non-zero exit — README lacks the Antigravity/ToS notes; CHANGELOG lacks the Gemini entry or does not exist).

- [ ] **Step 3: Minimal implementation.**
  (10a) `README.md` — add a **Gemini (Antigravity)** subsection under the providers list documenting: usage read from the GNOME keyring (`service=gemini, account=antigravity`, go-keyring blob); switching writes the token back into the keyring; **the Antigravity terms discourage rotating this token, so switch sparingly and keep auto-switch off unless you accept that**; identity is resolved via Google `userinfo`; Login is a safety net (Google tokens rarely die). Note the diagnostic `pitstop --gemini-spike`.
  (10b) `CHANGELOG.md` — add under an unreleased/next section (mirroring the Mac entries):
```markdown
### Added
- **Gemini provider (Antigravity surface).** Live Code Assist usage, account
  switching, auto-switch participation, and an in-app Login safety net. The
  Antigravity OAuth token is read from and written back to the GNOME keyring
  (`service=gemini, account=antigravity`). Note: Antigravity's terms discourage
  rotating this token — switching is surfaced with that caveat and auto-switch
  stays opt-in.
```

- [ ] **Step 4: Run test, verify it passes** \n Run: the grep above \n Expected: exit 0.

- [ ] **Step 5: Commit** \n `git add -A && git commit -m "docs: Gemini/Antigravity provider + ToS caveat (README, CHANGELOG)"`
