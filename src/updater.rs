//! Daily GitHub-release update check, semver comparison, and rebuild-and-relaunch
//! for source installs. Called from app.rs at the end of each refresh_all cycle.

/// Parse a version string into (major, minor, patch).
/// Strips a leading 'v'/'V', drops any pre-release suffix after the first '-',
/// and treats missing minor/patch as 0. Returns None for non-numeric input.
#[allow(dead_code)]
pub fn parse_semver(s: &str) -> Option<(u64, u64, u64)> {
    let s = s.trim_start_matches(['v', 'V']);
    let core = s.split('-').next().unwrap_or(s);
    let mut parts = core.split('.');
    let major: u64 = parts.next()?.parse().ok()?;
    let minor: u64 = parts.next().unwrap_or("0").parse().ok()?;
    let patch: u64 = parts.next().unwrap_or("0").parse().ok()?;
    Some((major, minor, patch))
}

/// True when `remote` is strictly greater than `local` (tuple comparison).
#[allow(dead_code)]
pub fn is_newer(remote: &str, local: &str) -> bool {
    match (parse_semver(remote), parse_semver(local)) {
        (Some(r), Some(l)) => r > l,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cargo_pkg_version_is_0_3_1() {
        assert_eq!(env!("CARGO_PKG_VERSION"), "0.3.1");
    }

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
}
