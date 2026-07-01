//! Saved Gemini (Antigravity) accounts. The live store is the GNOME keyring
//! item `service=gemini, account=antigravity`; saved snapshots are 0600 files
//! under `~/.config/pitstop/accounts/` (via `secret_store`). Analogous to
//! `codex_store`, but keyring- rather than file-backed for the live surface.
//!
//! # Format preservation (spike finding — the #1 risk)
//! On this machine Antigravity wrote the keyring value as **raw JSON with no
//! `go-keyring-base64:` prefix**, but other installs use the wrapped form.
//! Snapshots therefore store the **opaque original string verbatim** (whatever
//! form the keyring held) — never normalized or re-wrapped. When switching, the
//! saved snapshot is rewritten to match the form the live keyring currently
//! holds (Antigravity may reject a form it didn't write) before it is set.

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
/// The go-keyring wrapper prefix, mirrored from `gemini` for form detection.
const GO_KEYRING_PREFIX: &str = "go-keyring-base64:";

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

/// True when `blob` is the `go-keyring-base64:`-wrapped form (vs raw JSON).
fn is_wrapped(blob: &[u8]) -> bool {
    std::str::from_utf8(blob)
        .map(|s| s.trim_start().starts_with(GO_KEYRING_PREFIX))
        .unwrap_or(false)
}

/// Rewrite `snapshot` into the form the live keyring currently holds so
/// Antigravity can read it back after a switch (it may reject a form it didn't
/// write). When there is no current live value (or it can't be read), the
/// snapshot is returned verbatim. Purely a form conversion — the token payload
/// is never altered.
fn to_keyring_form(snapshot: &[u8], current_live: Option<&[u8]>) -> Vec<u8> {
    let Some(current) = current_live else {
        return snapshot.to_vec();
    };
    let want_wrapped = is_wrapped(current);
    if want_wrapped == is_wrapped(snapshot) {
        return snapshot.to_vec(); // already the right form
    }
    if want_wrapped {
        // snapshot is raw JSON; wrap it as go-keyring-base64.
        gemini::encode_go_keyring(snapshot).into_bytes()
    } else {
        // snapshot is wrapped; unwrap back to raw JSON. Fall back to verbatim if
        // it somehow doesn't decode (shouldn't happen — is_wrapped was true).
        std::str::from_utf8(snapshot)
            .ok()
            .and_then(gemini::decode_go_keyring)
            .unwrap_or_else(|| snapshot.to_vec())
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
            let mut v: Vec<GeminiProfile> =
                list.iter().filter_map(GeminiProfile::from_dict).collect();
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
        Ok(secret_service::get(LIVE_SERVICE, LIVE_ACCOUNT)
            .await?
            .map(String::into_bytes))
    }

    /// Write an opaque blob back into the live keyring item in place, verbatim.
    /// `switch_to` is the caller that matches the keyring's current form first.
    pub async fn write_live(blob: &[u8]) -> Result<()> {
        let s = String::from_utf8(blob.to_vec())?;
        secret_service::set(LIVE_SERVICE, LIVE_ACCOUNT, &s).await
    }

    /// Snapshot `blob` under `email` to the profile file (skip if byte-identical),
    /// and upsert non-secret metadata. Stores the **opaque original bytes
    /// verbatim** — no normalization or re-wrapping. Never touches the keyring.
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

    /// The saved snapshot blob for `email` (opaque, as stored).
    pub fn saved_blob(&self, email: &str) -> Result<Option<Vec<u8>>> {
        secret_store::read(PROVIDER, email)
    }

    /// Persist a snapshot whose access token PitStop refreshed itself. Only
    /// inactive accounts are refreshed, so this never touches the live keyring.
    /// Stored verbatim to preserve the opaque form (the refresh path patches the
    /// blob form-preservingly via `gemini::patch_antigravity_blob`).
    pub fn store_refreshed_blob(&self, data: &[u8], email: &str) -> Result<()> {
        secret_store::write(PROVIDER, email, data)
    }

    /// Make `email` the live Antigravity account by writing its saved snapshot
    /// into the keyring, first rewriting it to the form the keyring currently
    /// holds (see `to_keyring_form`).
    ///
    /// The caller MUST snapshot the outgoing live account first (via `snapshot`,
    /// with an app-resolved email) so its refresh token isn't stranded — the
    /// Gemini blob carries no email, so unlike `codex_store::switch_to` that
    /// capture can't happen here; it lives in `app.rs::perform_gemini_switch`.
    pub async fn switch_to(&self, email: &str) -> Result<()> {
        let Some(blob) = self.saved_blob(email)? else {
            return Err(anyhow!(
                "No saved Gemini credentials for {email} — sign in once with Antigravity and save again"
            ));
        };
        // Match the form Antigravity currently has in the keyring. A read error
        // (or no existing item) means "write the snapshot as-is".
        let current = Self::live_blob().await.ok().flatten();
        let to_write = to_keyring_form(&blob, current.as_deref());
        Self::write_live(&to_write).await
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

    #[test]
    fn profile_dict_round_trip() {
        let p = GeminiProfile {
            email: "me@x".into(),
            saved_at: 42.0,
            plan_label: "AI Pro".into(),
        };
        let back = GeminiProfile::from_dict(&p.to_dict()).unwrap();
        assert_eq!(back.email, "me@x");
        assert_eq!(back.plan_label, "AI Pro");
        assert!((back.saved_at - 42.0).abs() < f64::EPSILON);
    }

    // A minimal well-formed Antigravity token JSON (no real secrets).
    const RAW_JSON: &[u8] =
        br#"{"token":{"access_token":"ya29.AAA","refresh_token":"1//rt","token_type":"Bearer"},"auth_method":"consumer"}"#;

    #[test]
    fn is_wrapped_detects_form() {
        assert!(!is_wrapped(RAW_JSON));
        let wrapped = gemini::encode_go_keyring(RAW_JSON).into_bytes();
        assert!(is_wrapped(&wrapped));
    }

    #[test]
    fn to_keyring_form_no_current_writes_verbatim() {
        // No existing keyring item → snapshot written exactly as saved.
        let wrapped = gemini::encode_go_keyring(RAW_JSON).into_bytes();
        assert_eq!(to_keyring_form(RAW_JSON, None), RAW_JSON.to_vec());
        assert_eq!(to_keyring_form(&wrapped, None), wrapped);
    }

    #[test]
    fn to_keyring_form_same_form_is_verbatim() {
        // Live is raw, snapshot is raw → unchanged (byte-for-byte).
        assert_eq!(to_keyring_form(RAW_JSON, Some(RAW_JSON)), RAW_JSON.to_vec());
        // Live is wrapped, snapshot is wrapped → unchanged.
        let w = gemini::encode_go_keyring(RAW_JSON).into_bytes();
        assert_eq!(to_keyring_form(&w, Some(&w)), w);
    }

    #[test]
    fn to_keyring_form_wraps_raw_snapshot_for_wrapped_keyring() {
        // Live keyring holds the wrapped form; snapshot is raw JSON → wrap it.
        let live_wrapped = gemini::encode_go_keyring(RAW_JSON).into_bytes();
        let out = to_keyring_form(RAW_JSON, Some(&live_wrapped));
        assert!(is_wrapped(&out), "output must be wrapped to match the keyring");
        // Round-trips back to the exact raw JSON payload.
        let inner = gemini::decode_go_keyring(std::str::from_utf8(&out).unwrap()).unwrap();
        assert_eq!(inner, RAW_JSON.to_vec());
        // Tokens survive the conversion.
        assert_eq!(
            gemini::antigravity_creds(&out).unwrap().refresh_token.as_deref(),
            Some("1//rt")
        );
    }

    #[test]
    fn to_keyring_form_unwraps_wrapped_snapshot_for_raw_keyring() {
        // Live keyring holds raw JSON (this machine); snapshot is wrapped → unwrap.
        let snapshot_wrapped = gemini::encode_go_keyring(RAW_JSON).into_bytes();
        let out = to_keyring_form(&snapshot_wrapped, Some(RAW_JSON));
        assert!(!is_wrapped(&out), "output must be raw JSON to match the keyring");
        assert_eq!(out, RAW_JSON.to_vec());
        assert_eq!(
            gemini::antigravity_creds(&out).unwrap().refresh_token.as_deref(),
            Some("1//rt")
        );
    }

    #[test]
    fn snapshot_round_trip_preserves_opaque_form() {
        // Filesystem-only isolation (never the live keyring). Uses the same shared
        // temp XDG_CONFIG_HOME as the oauth persist tests so parallel interleaving
        // is harmless; unique emails keep the account files distinct.
        let dir = std::env::temp_dir().join("pitstop-oauth-relogin-tests");
        std::env::set_var("XDG_CONFIG_HOME", &dir);

        let mut store = GeminiStore { profiles: vec![] };

        // Raw-JSON snapshot (this machine's live form) stored byte-for-byte.
        let raw = br#"{"token":{"access_token":"ya29.RAW","refresh_token":"1//r"},"auth_method":"consumer"}"#;
        store.snapshot("gemini-store-raw@x", raw, "AI Pro").unwrap();
        assert_eq!(
            store.saved_blob("gemini-store-raw@x").unwrap().as_deref(),
            Some(&raw[..])
        );

        // Wrapped snapshot ALSO stored byte-for-byte (no unwrap/normalize).
        let wrapped = gemini::encode_go_keyring(RAW_JSON).into_bytes();
        store
            .snapshot("gemini-store-wrap@x", &wrapped, "Ultra")
            .unwrap();
        assert_eq!(
            store.saved_blob("gemini-store-wrap@x").unwrap(),
            Some(wrapped.clone())
        );

        // Metadata persisted and reloadable.
        let reloaded = GeminiStore::new();
        assert!(reloaded
            .profiles
            .iter()
            .any(|p| p.email == "gemini-store-raw@x" && p.plan_label == "AI Pro"));
        assert!(reloaded
            .profiles
            .iter()
            .any(|p| p.email == "gemini-store-wrap@x" && p.plan_label == "Ultra"));

        // Cleanup snapshots (leave XDG set for other parallel tests).
        store.remove("gemini-store-raw@x").unwrap();
        store.remove("gemini-store-wrap@x").unwrap();
    }
}
