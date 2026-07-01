//! Daily GitHub-release update check, semver comparison, and rebuild-and-relaunch
//! for source installs. Called from app.rs at the end of each refresh_all cycle.

#[cfg(test)]
mod tests {
    #[test]
    fn cargo_pkg_version_is_0_3_1() {
        assert_eq!(env!("CARGO_PKG_VERSION"), "0.3.1");
    }
}
