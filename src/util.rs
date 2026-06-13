use anyhow::Result;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// The user's home directory.
pub fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// PitStop's config directory — `$XDG_CONFIG_HOME/pitstop` or `~/.config/pitstop`.
/// Non-secret metadata (profiles.json, settings.json) lives here; secrets live
/// in the `accounts/` subdir as 0600 files (see `secret_store`).
pub fn config_dir() -> PathBuf {
    if let Some(x) = std::env::var_os("XDG_CONFIG_HOME") {
        if !x.is_empty() {
            return PathBuf::from(x).join("pitstop");
        }
    }
    home().join(".config").join("pitstop")
}

/// Wall-clock milliseconds since the Unix epoch (matches the credential blob's
/// `expiresAt`, which is epoch-ms).
pub fn now_ms() -> f64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64() * 1000.0)
        .unwrap_or(0.0)
}

/// Wall-clock seconds since the Unix epoch.
pub fn now_secs() -> f64 {
    now_ms() / 1000.0
}

/// Atomically write `data` to `path` (temp file in the same dir + rename), so a
/// crash can never leave a half-written file. When `mode` is set, the file is
/// created with — and forced to — those permissions (e.g. `0o600` for secrets).
pub fn write_atomic(path: &Path, data: &[u8], mode: Option<u32>) -> Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)?;
    let stem = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("pitstop");
    let tmp = dir.join(format!(".{stem}.tmp.{}", std::process::id()));

    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    if let Some(m) = mode {
        opts.mode(m);
    }
    {
        let mut f = opts.open(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    if let Some(m) = mode {
        // umask may have masked the create mode — force it.
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(m))?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}
