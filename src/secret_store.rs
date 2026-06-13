//! Per-account credential storage.
//!
//! On macOS PitStop routed everything through `/usr/bin/security` because the
//! keychain ACL re-prompted on every rebuild. On Linux there is no such prompt
//! and Claude Code itself just keeps `~/.claude/.credentials.json` as a 0600
//! file — so PitStop does the same: each saved account's blob is a 0600 file
//! under `~/.config/pitstop/accounts/`, written atomically. (gnome-keyring via
//! the Secret Service could be a future opt-in; files match Claude Code's own
//! posture and need no daemon.)

use crate::util::{config_dir, write_atomic};
use anyhow::Result;
use std::path::PathBuf;

pub fn accounts_dir() -> PathBuf {
    config_dir().join("accounts")
}

/// Encode an email into a safe, collision-free filename component: keep the
/// unreserved set, percent-escape everything else (injective, so distinct
/// emails never share a file).
fn sanitize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'-' | b'_' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn path_for(provider: &str, account: &str) -> PathBuf {
    accounts_dir().join(format!("{provider}-{}.json", sanitize(account)))
}

/// Read a saved blob, or `None` if it doesn't exist.
pub fn read(provider: &str, account: &str) -> Result<Option<Vec<u8>>> {
    match std::fs::read(path_for(provider, account)) {
        Ok(d) => Ok(Some(d)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Write (create or replace) a saved blob with 0600 permissions, atomically.
pub fn write(provider: &str, account: &str, data: &[u8]) -> Result<()> {
    write_atomic(&path_for(provider, account), data, Some(0o600))
}

/// Delete a saved blob; a missing file is not an error.
pub fn delete(provider: &str, account: &str) -> Result<()> {
    match std::fs::remove_file(path_for(provider, account)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}
