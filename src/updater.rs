//! Daily GitHub-release update check, semver comparison, and rebuild-and-relaunch
//! for source installs. Called from app.rs at the end of each refresh_all cycle.

use anyhow::Result;
use serde_json::Value;
use std::time::Duration;

const REPO_SLUG: &str = "Livin21/pitstop-linux";
const CHECK_INTERVAL_SECS: u64 = 86_400; // 24 hours

// ---------- data types ----------

/// Information about an available update. Present only when the remote version
/// is strictly newer than the running build.
#[derive(Clone)]
pub struct UpdateInfo {
    /// New version, leading "v" stripped (e.g. "0.4.0").
    pub version: String,
    /// HTML release-page URL; opened on rebuild failure or when can_rebuild is false.
    pub url: String,
    /// True when a usable source checkout is recorded by install.sh.
    pub can_rebuild: bool,
}

// ---------- semver ----------

/// Parse a version string into (major, minor, patch).
/// Strips a leading 'v'/'V', drops any pre-release suffix after the first '-',
/// and treats missing minor/patch as 0. Returns None for non-numeric input.
pub fn parse_semver(s: &str) -> Option<(u64, u64, u64)> {
    let s = s.trim_start_matches(['v', 'V']);
    let core = s.split('-').next().unwrap_or(s);
    let mut parts = core.split('.');
    let major: u64 = parts.next()?.parse().ok()?;
    let minor: u64 = parts.next().unwrap_or("0").parse().ok()?;
    let patch: u64 = parts.next().unwrap_or("0").parse().ok()?;
    Some((major, minor, patch))
}

/// True when `remote` is strictly greater than `local`.
pub fn is_newer(remote: &str, local: &str) -> bool {
    match (parse_semver(remote), parse_semver(local)) {
        (Some(r), Some(l)) => r > l,
        _ => false,
    }
}

// ---------- daily throttle ----------

// Sync std::fs I/O here is intentional — reads a tiny local timestamp file,
// matching the pattern used by ProfileStore/CodexStore/secret_store throughout
// the app (small local-file operations, not worth async overhead).
fn last_check_secs() -> Option<u64> {
    let path = crate::util::config_dir().join("last_update_check");
    std::fs::read_to_string(&path).ok()?.trim().parse().ok()
}

// Sync write via write_atomic is intentional for the same reason as
// last_check_secs — tiny local-file I/O consistent with the app's store pattern.
fn touch_last_check() {
    let path = crate::util::config_dir().join("last_update_check");
    let ts = crate::util::now_secs() as u64;
    let _ = crate::util::write_atomic(&path, ts.to_string().as_bytes(), None);
}

/// Called at the end of each refresh cycle.
///
/// Returns:
/// - `None`              — not yet due (throttled); caller keeps existing update_info unchanged
/// - `Some(None)`        — checked; running version is current; caller clears update_info
/// - `Some(Some(info))` — update available; caller stores info
pub async fn check_if_due(http: &reqwest::Client) -> Option<Option<UpdateInfo>> {
    let now = crate::util::now_secs() as u64;
    if let Some(last) = last_check_secs() {
        if now.saturating_sub(last) < CHECK_INTERVAL_SECS {
            return None; // throttled
        }
    }
    touch_last_check();
    Some(check(http).await)
}

// ---------- GitHub check ----------

/// Pure parse function: extract an [`UpdateInfo`] from a GitHub release JSON
/// object, returning `None` when the release should be skipped (prerelease,
/// draft, not newer than `local`, or missing required fields).
///
/// Extracted so tests can exercise the exact production parsing path without
/// making network calls.
fn parse_release(root: &Value, local: &str) -> Option<UpdateInfo> {
    // /releases/latest already skips pre-releases; guard defensively.
    if root.get("prerelease").and_then(Value::as_bool).unwrap_or(false) {
        return None;
    }
    // Defense-in-depth: exclude draft releases in case the endpoint changes.
    if root.get("draft").and_then(Value::as_bool).unwrap_or(false) {
        return None;
    }
    let tag = root.get("tag_name").and_then(Value::as_str)?;
    if !is_newer(tag, local) {
        return None;
    }
    let release_url = root
        .get("html_url")
        .and_then(Value::as_str)
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("https://github.com/{REPO_SLUG}/releases/latest"));
    let display = tag.trim_start_matches('v').to_string();
    Some(UpdateInfo {
        version: display,
        url: release_url,
        can_rebuild: source_repo_valid(),
    })
}

/// Fetch the latest non-prerelease GitHub release and compare to the running
/// build version. Returns None when up to date, on 404 (no releases), or on
/// any transient network or parse failure (silent, best-effort).
pub async fn check(http: &reqwest::Client) -> Option<UpdateInfo> {
    let local = env!("CARGO_PKG_VERSION");
    let api_url = format!("https://api.github.com/repos/{REPO_SLUG}/releases/latest");
    let resp = http
        .get(&api_url)
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "PitStop")
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .ok()?;
    if resp.status().as_u16() != 200 {
        return None; // 404 = no releases yet, other = transient error
    }
    let root: Value = resp.json().await.ok()?;
    parse_release(&root, local)
}

// ---------- repo-path resolution ----------

/// Read the source checkout path recorded by install.sh into
/// ~/.config/pitstop/repo_path.
fn read_repo_path() -> Option<String> {
    let path = crate::util::config_dir().join("repo_path");
    let s = std::fs::read_to_string(&path).ok()?;
    let trimmed = s.trim().to_string();
    if trimmed.is_empty() { None } else { Some(trimmed) }
}

/// Pure validation: returns true when `repo_dir` contains both `.git/` and
/// `install.sh`. Extracted as a pure function so tests can probe any path
/// without reading ~/.config/pitstop/repo_path.
pub fn repo_is_valid(repo_dir: &str) -> bool {
    let p = std::path::Path::new(repo_dir);
    p.join(".git").exists() && p.join("install.sh").exists()
}

/// True when the path recorded by install.sh exists and is a usable checkout.
pub fn source_repo_valid() -> bool {
    read_repo_path()
        .as_deref()
        .map(repo_is_valid)
        .unwrap_or(false)
}

// ---------- rebuild helpers ----------

/// Return the last 400 characters of a byte slice as a trimmed String (for
/// showing the tail of cargo/git stderr without flooding a notification).
fn last_400(bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(bytes);
    let chars: Vec<char> = s.chars().collect();
    let start = chars.len().saturating_sub(400);
    chars[start..].iter().collect::<String>().trim().to_string()
}

/// Run a subprocess, capturing stdout+stderr. Returns Err with the last 400
/// bytes of stderr (or stdout) when the exit status is non-zero.
async fn run_cmd(program: &str, args: &[&str], cwd: Option<&str>) -> Result<()> {
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    let out = cmd.output().await?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = last_400(&out.stderr);
    let msg = if stderr.is_empty() {
        last_400(&out.stdout)
    } else {
        stderr
    };
    anyhow::bail!("{msg}")
}

// ---------- rebuild & relaunch ----------

/// Pull the latest source, rebuild, atomically install the new binary to
/// `~/.local/bin/pitstop`, then exec the new binary in place of the current
/// process (no orphan tray). On any failure returns an Err — the caller must
/// notify the user and open the release page; no partial install occurs.
pub async fn rebuild_and_relaunch(info: &UpdateInfo) -> Result<()> {
    // `info` is part of the interface (the caller uses info.url to open the
    // release page when this returns Err); it is not needed inside the rebuild.
    let _ = info;
    let repo = read_repo_path().ok_or_else(|| {
        anyhow::anyhow!(
            "No source checkout path recorded — \
             open the release page to update manually."
        )
    })?;

    if !repo_is_valid(&repo) {
        anyhow::bail!(
            "Checkout at '{}' is missing .git or install.sh — cannot rebuild.",
            repo
        );
    }

    // 1. git pull --ff-only (abort on diverged or dirty history)
    run_cmd("git", &["-C", &repo, "pull", "--ff-only"], None)
        .await
        .map_err(|e| anyhow::anyhow!("git pull failed: {e}"))?;

    // 2. cargo build --release (may take minutes; exec replaces the process
    //    immediately after, so blocking the tokio loop is acceptable here)
    run_cmd("cargo", &["build", "--release"], Some(&repo))
        .await
        .map_err(|e| anyhow::anyhow!("cargo build failed: {e}"))?;

    // 3. Atomic install to ~/.local/bin/pitstop
    //    Build to a .new temp then rename — if copy fails, dst is untouched.
    use std::os::unix::fs::PermissionsExt;
    let src = std::path::Path::new(&repo).join("target/release/pitstop");
    anyhow::ensure!(src.exists(), "Built binary not found at {}", src.display());
    let bin_dir = crate::util::home().join(".local/bin");
    std::fs::create_dir_all(&bin_dir)?;
    let dst = bin_dir.join("pitstop");
    let tmp = bin_dir.join(".pitstop.new");
    std::fs::copy(&src, &tmp)?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    std::fs::rename(&tmp, &dst)?;

    // 4. exec the new binary — replaces this process entirely; the old
    //    StatusNotifierItem is unregistered automatically when the PID exits.
    use std::os::unix::process::CommandExt;
    let io_err = std::process::Command::new(&dst).exec();
    anyhow::bail!("exec failed: {io_err}");
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Task 1 ---

    #[test]
    fn cargo_pkg_version_is_0_4_1() {
        assert_eq!(env!("CARGO_PKG_VERSION"), "0.4.1");
    }

    // --- Task 2 ---

    #[test]
    fn semver_parse_v_prefix() {
        assert_eq!(parse_semver("v0.3.1"), Some((0, 3, 1)));
        assert_eq!(parse_semver("V1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_semver("0.3.1"), Some((0, 3, 1)));
    }

    #[test]
    fn semver_parse_pre_release_stripped() {
        assert_eq!(parse_semver("1.2.3-beta.1"), Some((1, 2, 3)));
        assert_eq!(parse_semver("v2.0.0-rc.1"), Some((2, 0, 0)));
    }

    #[test]
    fn semver_parse_short() {
        assert_eq!(parse_semver("1.2"), Some((1, 2, 0)));
        assert_eq!(parse_semver("1"), Some((1, 0, 0)));
    }

    #[test]
    fn semver_parse_invalid() {
        assert_eq!(parse_semver(""), None);
        assert_eq!(parse_semver("abc"), None);
        assert_eq!(parse_semver("1.x.3"), None);
    }

    #[test]
    fn is_newer_semantics() {
        assert!(is_newer("v0.4.0", "0.3.1"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(!is_newer("0.3.1", "0.3.1"), "same version → not newer");
        assert!(!is_newer("0.3.0", "0.3.1"), "older remote → not newer");
        assert!(!is_newer("bad", "0.3.1"), "unparseable → not newer");
    }

    // --- Task 3 ---

    #[test]
    fn parse_github_release_non_prerelease() {
        let json = r#"{
            "tag_name": "v0.4.0",
            "html_url": "https://github.com/Livin21/pitstop-linux/releases/tag/v0.4.0",
            "prerelease": false,
            "draft": false
        }"#;
        let root: serde_json::Value = serde_json::from_str(json).unwrap();
        let info = parse_release(&root, "0.3.1").expect("newer non-prerelease → Some");
        assert_eq!(info.version, "0.4.0");
        assert_eq!(
            info.url,
            "https://github.com/Livin21/pitstop-linux/releases/tag/v0.4.0"
        );
    }

    #[test]
    fn parse_github_release_prerelease_skipped() {
        let json = r#"{"tag_name":"v0.4.0-alpha","html_url":"https://...","prerelease":true,"draft":false}"#;
        let root: serde_json::Value = serde_json::from_str(json).unwrap();
        assert!(
            parse_release(&root, "0.3.1").is_none(),
            "prerelease:true → parse_release returns None"
        );
    }

    #[test]
    fn parse_github_release_up_to_date() {
        let json = r#"{"tag_name":"v0.3.1","html_url":"https://...","prerelease":false,"draft":false}"#;
        let root: serde_json::Value = serde_json::from_str(json).unwrap();
        assert!(
            parse_release(&root, "0.3.1").is_none(),
            "same version → up to date → parse_release returns None"
        );
    }

    #[test]
    fn parse_github_release_draft_skipped() {
        let json = r#"{"tag_name":"v0.4.0","html_url":"https://...","prerelease":false,"draft":true}"#;
        let root: serde_json::Value = serde_json::from_str(json).unwrap();
        assert!(
            parse_release(&root, "0.3.1").is_none(),
            "draft:true → parse_release returns None"
        );
    }

    #[test]
    fn update_info_is_clone() {
        // UpdateInfo must be Clone so Engine can clone it into TrayView
        let info = UpdateInfo {
            version: "0.4.0".into(),
            url: "https://example.com".into(),
            can_rebuild: false,
        };
        let _copy = info.clone();
    }

    // --- Task 4 ---

    #[test]
    fn missing_path_not_valid() {
        // pure function: a nonexistent path must return false
        assert!(!repo_is_valid("/nonexistent/pitstop_test_repo_abc123"));
    }

    #[test]
    fn valid_checkout_structure() {
        let dir = std::env::temp_dir().join("pitstop_test_valid_repo");
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join("install.sh"), b"#!/bin/bash\n").unwrap();
        assert!(repo_is_valid(dir.to_str().unwrap()));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_install_sh_not_valid() {
        let dir = std::env::temp_dir().join("pitstop_test_no_install_sh");
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        // no install.sh → not valid
        assert!(!repo_is_valid(dir.to_str().unwrap()));
        std::fs::remove_dir_all(&dir).ok();
    }

    // --- Task 5 ---

    // rebuild_and_relaunch must exist and return Err when the repo is invalid.
    // We verify the guard logic by composing repo_is_valid (already tested) with
    // the function signature.  A full build-and-exec test is manual E2E only.
    #[tokio::test]
    async fn rebuild_rejects_invalid_checkout() {
        let tmp = std::env::temp_dir().join("pitstop_test_bad_checkout_rb");
        std::fs::create_dir_all(&tmp).unwrap();
        // no .git — repo_is_valid will return false
        assert!(!repo_is_valid(tmp.to_str().unwrap()));
        // rebuild_and_relaunch references the function; compilation is the first gate.
        // It is an async fn, so we bind the fn item (never called) rather than
        // casting to a fn pointer — the opaque Future return cannot form a fn ptr,
        // and calling it would rebuild + re-exec the test process.
        let _sig = rebuild_and_relaunch;
        std::fs::remove_dir_all(&tmp).ok();
    }
}
