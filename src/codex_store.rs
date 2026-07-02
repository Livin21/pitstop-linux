//! Saved Codex accounts. The live store is `~/.codex/auth.json` (or
//! `$CODEX_HOME/auth.json`); saved snapshots are 0600 files like the Claude
//! ones. Switching writes a saved snapshot back into the live file.

use crate::codex;
use crate::secret_store;
use crate::util::{config_dir, home, now_secs, write_atomic};
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::path::PathBuf;

pub fn codex_home() -> PathBuf {
    if let Some(d) = std::env::var_os("CODEX_HOME") {
        if !d.is_empty() {
            return PathBuf::from(d);
        }
    }
    home().join(".codex")
}

pub fn auth_path() -> PathBuf {
    codex_home().join("auth.json")
}

fn write_live(blob: &[u8]) -> Result<()> {
    write_atomic(&auth_path(), blob, Some(0o600))
}

pub struct CodexProfile {
    pub email: String,
    pub saved_at: f64,
    pub plan_label: String,
}

impl CodexProfile {
    fn from_dict(d: &Value) -> Option<CodexProfile> {
        Some(CodexProfile {
            email: d.get("email")?.as_str()?.to_string(),
            saved_at: d.get("savedAt").and_then(Value::as_f64).unwrap_or(0.0),
            plan_label: d
                .get("planLabel")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        })
    }

    fn to_dict(&self) -> Value {
        json!({
            "email": self.email,
            "savedAt": self.saved_at,
            "planLabel": self.plan_label,
        })
    }
}

/// Whether re-capturing the live account would store new bytes: true unless a
/// saved profile already exists with a byte-identical blob. Pure for testing.
fn capture_changed(has_profile: bool, stored_eq: bool) -> bool {
    !(has_profile && stored_eq)
}

pub struct CodexStore {
    pub profiles: Vec<CodexProfile>,
}

impl CodexStore {
    fn file() -> PathBuf {
        config_dir().join("codex-profiles.json")
    }

    pub fn new() -> Self {
        let mut s = CodexStore { profiles: vec![] };
        s.load();
        s
    }

    pub fn load(&mut self) {
        self.profiles = (|| -> Option<Vec<CodexProfile>> {
            let data = std::fs::read(Self::file()).ok()?;
            let root: Value = serde_json::from_slice(&data).ok()?;
            let list = root.get("profiles")?.as_array()?;
            let mut v: Vec<CodexProfile> = list.iter().filter_map(CodexProfile::from_dict).collect();
            v.sort_by(|a, b| a.email.cmp(&b.email));
            Some(v)
        })()
        .unwrap_or_default();
    }

    fn save(&self) -> Result<()> {
        let arr: Vec<Value> = self.profiles.iter().map(CodexProfile::to_dict).collect();
        let data = serde_json::to_vec_pretty(&json!({ "profiles": arr }))?;
        write_atomic(&Self::file(), &data, None)
    }

    /// The email currently live in `~/.codex/auth.json`.
    pub fn live_email(&self) -> Option<String> {
        codex::live_blob()
            .as_deref()
            .and_then(codex::credentials)
            .map(|c| c.email)
    }

    /// Snapshot the live Codex account. Returns `(email, changed)`: `email` is
    /// `Some` when signed in, `changed` is `true` when new bytes were actually
    /// written. Callers use `changed` to heal a `needs_action` gate placed after
    /// an external `codex login`.
    pub fn capture_current(&mut self) -> Result<(Option<String>, bool)> {
        let Some(live) = codex::live_blob() else {
            return Ok((None, false));
        };
        let Some(creds) = codex::credentials(&live) else {
            return Ok((None, false));
        };
        let email = creds.email.clone();
        let blob = codex::normalized_blob(&live);

        let has_profile = self.profiles.iter().any(|p| p.email == email);
        let mut stored_eq = false;
        if has_profile {
            if let Ok(Some(stored)) = secret_store::read(codex::PROVIDER, &email) {
                stored_eq = stored == blob;
            }
        }
        if !capture_changed(has_profile, stored_eq) {
            return Ok((Some(email), false));
        }

        secret_store::write(codex::PROVIDER, &email, &blob)?;
        self.profiles.retain(|p| p.email != email);
        self.profiles.push(CodexProfile {
            email: email.clone(),
            saved_at: now_secs(),
            plan_label: creds.plan_label,
        });
        self.profiles.sort_by(|a, b| a.email.cmp(&b.email));
        self.save()?;
        Ok((Some(email), true))
    }

    /// Make `email` the live Codex account: snapshot whatever's live, then write
    /// the saved blob into `~/.codex/auth.json`.
    pub fn switch_to(&mut self, email: &str) -> Result<()> {
        let _ = self.capture_current()?;
        let Some(blob) = secret_store::read(codex::PROVIDER, email)? else {
            return Err(anyhow!(
                "No saved credentials for {email} — sign in once with `codex` and save again"
            ));
        };
        write_live(&codex::preserving_api_key(codex::live_blob().as_deref(), &blob))
    }

    /// The blob to fetch usage with — the live file for the active account,
    /// the saved snapshot otherwise.
    pub fn blob(&self, email: &str, is_active: bool) -> Result<Option<Vec<u8>>> {
        if is_active {
            if let Some(live) = codex::live_blob() {
                return Ok(Some(live));
            }
        }
        secret_store::read(codex::PROVIDER, email)
    }

    /// Persist a saved snapshot whose tokens PitStop refreshed itself. Only
    /// inactive accounts are refreshed, so this never touches the live file.
    pub fn store_refreshed_blob(&self, data: &[u8], email: &str) -> Result<()> {
        secret_store::write(codex::PROVIDER, email, &codex::normalized_blob(data))
    }

    pub fn remove(&mut self, email: &str) -> Result<()> {
        secret_store::delete(codex::PROVIDER, email)?;
        self.profiles.retain(|p| p.email != email);
        self.save()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_changed_truth_table() {
        assert!(capture_changed(false, false)); // no profile yet → changed
        assert!(capture_changed(true, false));  // blob differs → changed
        assert!(!capture_changed(true, true));  // identical → unchanged
    }
}
