//! Saved Claude Code accounts. The live store is the file
//! `~/.claude/.credentials.json` (a 0600 file Claude Code owns); saved
//! snapshots are 0600 files under `~/.config/pitstop/accounts/`. Switching
//! writes a snapshot back into the live file and restores its `oauthAccount`
//! identity in `~/.claude.json`. Non-secret metadata lives in `profiles.json`.

use crate::credentials;
use crate::secret_store;
use crate::util::{config_dir, home, now_secs, write_atomic};
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::path::PathBuf;

const PROVIDER: &str = credentials::LIVE_PROVIDER;

pub struct Profile {
    pub email: String,
    pub saved_at: f64,
    pub subscription_type: Option<String>,
    pub rate_limit_tier: Option<String>,
    /// The `oauthAccount` object, kept verbatim so it restores exactly on switch.
    pub oauth_account: Value,
}

impl Profile {
    fn organization_name(&self) -> Option<&str> {
        self.oauth_account
            .get("organizationName")
            .and_then(Value::as_str)
    }

    /// e.g. "Acme AI · Team · 5x" — drops auto "<email>'s Organization" names
    /// and the noisy `default_claude_` tier prefix.
    pub fn plan_label(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(org) = self.organization_name() {
            if !org.is_empty() && org != format!("{}'s Organization", self.email) {
                parts.push(org.to_string());
            }
        }
        if let Some(sub) = &self.subscription_type {
            if !sub.is_empty() {
                parts.push(capitalize(sub));
            }
        }
        if let Some(tier) = &self.rate_limit_tier {
            if let Some(idx) = tier.find("max_") {
                parts.push(tier[idx + 4..].to_string());
            }
        }
        parts.join(" · ")
    }

    fn from_dict(d: &Value) -> Option<Profile> {
        Some(Profile {
            email: d.get("email")?.as_str()?.to_string(),
            saved_at: d.get("savedAt").and_then(Value::as_f64).unwrap_or(0.0),
            subscription_type: d
                .get("subscriptionType")
                .and_then(Value::as_str)
                .map(String::from),
            rate_limit_tier: d
                .get("rateLimitTier")
                .and_then(Value::as_str)
                .map(String::from),
            oauth_account: d.get("oauthAccount").cloned().unwrap_or_else(|| json!({})),
        })
    }

    fn to_dict(&self) -> Value {
        let mut m = serde_json::Map::new();
        m.insert("email".into(), json!(self.email));
        m.insert("savedAt".into(), json!(self.saved_at));
        m.insert("oauthAccount".into(), self.oauth_account.clone());
        if let Some(s) = &self.subscription_type {
            m.insert("subscriptionType".into(), json!(s));
        }
        if let Some(t) = &self.rate_limit_tier {
            m.insert("rateLimitTier".into(), json!(t));
        }
        Value::Object(m)
    }
}

/// Swift's `String.capitalized` for a single word: first letter up, rest down.
fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(f) => f.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase(),
        None => String::new(),
    }
}

// MARK: - Live credential file (~/.claude/.credentials.json)

pub fn claude_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        if !d.is_empty() {
            return PathBuf::from(d);
        }
    }
    home().join(".claude")
}

pub fn live_creds_path() -> PathBuf {
    claude_dir().join(".credentials.json")
}

fn read_live() -> Option<Vec<u8>> {
    std::fs::read(live_creds_path()).ok()
}

fn write_live(data: &[u8]) -> Result<()> {
    write_atomic(&live_creds_path(), data, Some(0o600))
}

pub struct ProfileStore {
    pub profiles: Vec<Profile>,
}

impl ProfileStore {
    fn file() -> PathBuf {
        config_dir().join("profiles.json")
    }

    pub fn new() -> Self {
        let mut s = ProfileStore { profiles: vec![] };
        s.load();
        s
    }

    pub fn load(&mut self) {
        self.profiles = (|| -> Option<Vec<Profile>> {
            let data = std::fs::read(Self::file()).ok()?;
            let root: Value = serde_json::from_slice(&data).ok()?;
            let list = root.get("profiles")?.as_array()?;
            let mut v: Vec<Profile> = list.iter().filter_map(Profile::from_dict).collect();
            v.sort_by(|a, b| a.email.cmp(&b.email));
            Some(v)
        })()
        .unwrap_or_default();
    }

    fn save(&self) -> Result<()> {
        let arr: Vec<Value> = self.profiles.iter().map(Profile::to_dict).collect();
        let data = serde_json::to_vec_pretty(&json!({ "profiles": arr }))?;
        write_atomic(&Self::file(), &data, None)
    }

    /// Snapshot the live Claude Code credentials + identity into a profile.
    /// Returns the email when something is logged in (even if unchanged), or
    /// `None` when nobody is. Skips writes when nothing changed.
    pub fn capture_current(&mut self) -> Result<Option<String>> {
        let Some(blob) = read_live() else {
            return Ok(None);
        };
        let Some(account) = credentials::oauth_account() else {
            return Ok(None);
        };
        let Some(email) = account
            .get("emailAddress")
            .and_then(Value::as_str)
            .map(String::from)
        else {
            return Ok(None);
        };

        if let Some(existing) = self.profiles.iter().find(|p| p.email == email) {
            if let Ok(Some(stored)) = secret_store::read(PROVIDER, &email) {
                if stored == blob && existing.oauth_account == account {
                    return Ok(Some(email));
                }
            }
        }

        let creds = credentials::parse_blob(&blob)?;
        secret_store::write(PROVIDER, &email, &blob)?;
        self.profiles.retain(|p| p.email != email);
        self.profiles.push(Profile {
            email: email.clone(),
            saved_at: now_secs(),
            subscription_type: creds.subscription_type,
            rate_limit_tier: creds.rate_limit_tier,
            oauth_account: account,
        });
        self.profiles.sort_by(|a, b| a.email.cmp(&b.email));
        self.save()?;
        Ok(Some(email))
    }

    /// Make `email` live: snapshot whatever's current, then write the saved blob
    /// into the live file and its identity into `~/.claude.json`.
    pub fn switch_to(&mut self, email: &str) -> Result<()> {
        // A failed snapshot aborts the switch — overwriting the live file
        // without a fresh copy could strand the outgoing refresh token.
        let _ = self.capture_current()?;
        let account = self
            .profiles
            .iter()
            .find(|p| p.email == email)
            .map(|p| p.oauth_account.clone())
            .ok_or_else(|| anyhow!("No saved profile for {email}"))?;
        let Some(blob) = secret_store::read(PROVIDER, email)? else {
            return Err(anyhow!(
                "No saved credentials for {email} — log in once with `claude` and save again"
            ));
        };
        write_live(&blob)?;
        credentials::set_oauth_account(&account)
    }

    /// The blob to fetch usage with — the live file for the active account,
    /// the saved snapshot otherwise.
    pub fn blob(&self, email: &str, is_active: bool) -> Result<Option<Vec<u8>>> {
        if is_active {
            if let Some(live) = read_live() {
                return Ok(Some(live));
            }
        }
        secret_store::read(PROVIDER, email)
    }

    /// Persist a blob whose tokens we refreshed ourselves.
    pub fn store_refreshed_blob(&self, data: &[u8], email: &str, is_active: bool) -> Result<()> {
        secret_store::write(PROVIDER, email, data)?;
        if is_active {
            write_live(data)?;
        }
        Ok(())
    }

    pub fn remove(&mut self, email: &str) -> Result<()> {
        secret_store::delete(PROVIDER, email)?;
        self.profiles.retain(|p| p.email != email);
        self.save()
    }
}
