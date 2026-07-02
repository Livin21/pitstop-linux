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

/// Write the target's live blob, then apply its identity. If applying the
/// identity fails, restore `previous` so `~/.claude/.credentials.json` and
/// `~/.claude.json` can't disagree (a mismatched pair makes the next
/// `capture_current` file the new tokens under the old profile), then surface
/// the original error. Generic over the write/apply closures so the rollback is
/// testable without the real files.
fn write_then_set_identity<W, S>(
    previous: Option<Vec<u8>>,
    blob: &[u8],
    write_live: W,
    set_identity: S,
) -> Result<()>
where
    W: Fn(&[u8]) -> Result<()>,
    S: FnOnce() -> Result<()>,
{
    write_live(blob)?;
    if let Err(e) = set_identity() {
        if let Some(prev) = previous {
            let _ = write_live(&prev);
        }
        return Err(e);
    }
    Ok(())
}

/// Whether re-capturing the live account would store new bytes: true unless a
/// saved profile already exists with a byte-identical blob AND matching
/// identity. Pure so the change-detection is unit-testable.
fn capture_changed(has_profile: bool, stored_eq: bool, account_eq: bool) -> bool {
    !(has_profile && stored_eq && account_eq)
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
    /// Returns `(email, changed)`: `email` is `Some` when someone is logged in,
    /// `changed` is `true` when new bytes were actually written (i.e. credentials
    /// differed from the last snapshot). Callers use `changed` to heal a
    /// `needs_action` gate placed after an external re-login.
    pub fn capture_current(&mut self) -> Result<(Option<String>, bool)> {
        let Some(blob) = read_live() else {
            return Ok((None, false));
        };
        let Some(account) = credentials::oauth_account() else {
            return Ok((None, false));
        };
        let Some(email) = account
            .get("emailAddress")
            .and_then(Value::as_str)
            .map(String::from)
        else {
            return Ok((None, false));
        };

        let mut account_eq = false;
        let has_profile = if let Some(existing) = self.profiles.iter().find(|p| p.email == email) {
            account_eq = existing.oauth_account == account;
            true
        } else {
            false
        };
        let mut stored_eq = false;
        if has_profile {
            if let Ok(Some(stored)) = secret_store::read(PROVIDER, &email) {
                stored_eq = stored == blob;
            }
        }
        if !capture_changed(has_profile, stored_eq, account_eq) {
            return Ok((Some(email), false));
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
        Ok((Some(email), true))
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
        write_then_set_identity(
            read_live(),
            &blob,
            write_live,
            || credentials::set_oauth_account(&account),
        )
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn capture_changed_truth_table() {
        assert!(capture_changed(false, false, false)); // no profile yet → changed
        assert!(capture_changed(true, false, true));   // blob differs → changed
        assert!(capture_changed(true, true, false));   // identity differs → changed
        assert!(!capture_changed(true, true, true));   // all match → unchanged
    }

    #[test]
    fn switch_rollback_restores_previous_live_when_identity_fails() {
        let writes: RefCell<Vec<Vec<u8>>> = RefCell::new(Vec::new());
        let err = write_then_set_identity(
            Some(b"PREVIOUS".to_vec()),
            b"NEW",
            |d| {
                writes.borrow_mut().push(d.to_vec());
                Ok(())
            },
            || Err(anyhow!("identity write failed")),
        )
        .unwrap_err();
        assert!(err.to_string().contains("identity write failed"));
        // New blob written first, then the previous blob restored.
        assert_eq!(writes.borrow().len(), 2);
        assert_eq!(writes.borrow()[0], b"NEW");
        assert_eq!(writes.borrow()[1], b"PREVIOUS");
    }

    #[test]
    fn switch_commits_without_rollback_on_success() {
        let writes: RefCell<Vec<Vec<u8>>> = RefCell::new(Vec::new());
        write_then_set_identity(
            Some(b"PREVIOUS".to_vec()),
            b"NEW",
            |d| {
                writes.borrow_mut().push(d.to_vec());
                Ok(())
            },
            || Ok(()),
        )
        .unwrap();
        assert_eq!(writes.borrow().len(), 1);
        assert_eq!(writes.borrow()[0], b"NEW");
    }
}
