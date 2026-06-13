//! Parsing/patching the Claude Code credential blob and the `~/.claude.json`
//! identity — the Linux file equivalents of the macOS keychain item.
//!
//! On Linux, Claude Code stores the blob at `~/.claude/.credentials.json`
//! (or `$CLAUDE_CONFIG_DIR/.credentials.json`) as a 0600 file, with exactly the
//! shape PitStop reads on macOS: `{ "claudeAiOauth": { "accessToken", ... },
//! "mcpOAuth": { ... } }`. The whole blob is stored verbatim so switching an
//! account also carries that account's per-MCP OAuth tokens.

use crate::util::{home, now_ms, write_atomic};
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::path::PathBuf;

/// The decoded `claudeAiOauth` section of the credential blob.
#[derive(Clone)]
pub struct OAuthCredentials {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at_ms: f64,
    pub subscription_type: Option<String>,
    pub rate_limit_tier: Option<String>,
}

impl OAuthCredentials {
    /// Treat anything within 2 minutes of expiry as expired.
    pub fn is_expired(&self) -> bool {
        now_ms() >= self.expires_at_ms - 120_000.0
    }
}

pub const LIVE_PROVIDER: &str = "claude";

/// Parse the `claudeAiOauth` section out of a credential blob.
pub fn parse_blob(data: &[u8]) -> Result<OAuthCredentials> {
    let root: Value = serde_json::from_slice(data)?;
    let oauth = root
        .get("claudeAiOauth")
        .ok_or_else(|| anyhow!("credential blob is not in the expected format"))?;
    let access = oauth
        .get("accessToken")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("credential blob is not in the expected format"))?;
    Ok(OAuthCredentials {
        access_token: access.to_string(),
        refresh_token: oauth
            .get("refreshToken")
            .and_then(Value::as_str)
            .map(String::from),
        expires_at_ms: oauth.get("expiresAt").and_then(Value::as_f64).unwrap_or(0.0),
        subscription_type: oauth
            .get("subscriptionType")
            .and_then(Value::as_str)
            .map(String::from),
        rate_limit_tier: oauth
            .get("rateLimitTier")
            .and_then(Value::as_str)
            .map(String::from),
    })
}

/// Return a copy of `data` with fresh tokens patched into `claudeAiOauth`,
/// leaving every other section (e.g. `mcpOAuth`) untouched.
pub fn patch_blob(
    data: &[u8],
    access_token: &str,
    refresh_token: Option<&str>,
    expires_at_ms: f64,
) -> Result<Vec<u8>> {
    let mut root: Value = serde_json::from_slice(data)?;
    let oauth = root
        .get_mut("claudeAiOauth")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| anyhow!("credential blob is not in the expected format"))?;
    oauth.insert("accessToken".into(), json!(access_token));
    if let Some(rt) = refresh_token {
        oauth.insert("refreshToken".into(), json!(rt));
    }
    // expiresAt is epoch-ms, stored as an integer like Claude Code writes it.
    oauth.insert("expiresAt".into(), json!(expires_at_ms as i64));
    Ok(serde_json::to_vec(&root)?)
}

// MARK: - ~/.claude.json identity (oauthAccount)

fn claude_json_path() -> PathBuf {
    home().join(".claude.json")
}

/// The `oauthAccount` object Claude Code shows for the logged-in account.
pub fn oauth_account() -> Option<Value> {
    let data = std::fs::read(claude_json_path()).ok()?;
    let root: Value = serde_json::from_slice(&data).ok()?;
    root.get("oauthAccount").cloned()
}

pub fn active_email() -> Option<String> {
    oauth_account()?
        .get("emailAddress")?
        .as_str()
        .map(String::from)
}

/// Replace only the `oauthAccount` key of `~/.claude.json`, preserving the rest.
pub fn set_oauth_account(account: &Value) -> Result<()> {
    let path = claude_json_path();
    let data = std::fs::read(&path)?;
    let mut root: Value = serde_json::from_slice(&data)?;
    let obj = root
        .as_object_mut()
        .ok_or_else(|| anyhow!("~/.claude.json is missing or not valid JSON"))?;
    obj.insert("oauthAccount".into(), account.clone());
    write_atomic(&path, &serde_json::to_vec(&root)?, None)
}
