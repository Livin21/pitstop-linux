//! Native OAuth `authorization_code` re-login (PKCE S256) for expired,
//! inactive Claude Code & Codex rows. Writes fresh tokens ONLY into the
//! saved-profile snapshot — never the live store.

use anyhow::Result;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use rand::RngCore;
use sha2::{Digest, Sha256};

pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
    pub state: String,
}

fn random_b64url(n: usize) -> String {
    let mut bytes = vec![0u8; n];
    rand::thread_rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// code_challenge = base64url(SHA256(code_verifier)) — RFC 7636 S256.
pub fn challenge_for(verifier: &str) -> String {
    let mut h = Sha256::new();
    h.update(verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(h.finalize())
}

impl Pkce {
    pub fn generate() -> Pkce {
        let verifier = random_b64url(64);
        let challenge = challenge_for(&verifier);
        Pkce { verifier, challenge, state: random_b64url(32) }
    }
}

/// Fresh tokens from an authorization_code exchange, provider-neutral.
/// NOTE: secret-bearing — must NOT derive Debug.
#[allow(dead_code)]
pub struct FreshTokens {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub id_token: Option<String>,
    pub expires_at_ms: i64,
}

/// The authenticated identity, for matching against the target row.
#[allow(dead_code)]
pub struct LoginIdentity {
    pub email: String,
    pub account_id: Option<String>,
}

/// The provider-varying surface of the OAuth login flow.
#[async_trait::async_trait]
#[allow(dead_code)]
pub trait LoginAdapter: Send + Sync {
    fn authorize_url(&self, pkce: &Pkce, redirect_uri: &str, paste_mode: bool) -> String;
    /// Codex `Some(1455)` (falls back to 1457); Claude `None` (ephemeral).
    fn fixed_loopback_port(&self) -> Option<u16>;
    fn redirect_path(&self) -> &'static str;
    fn supports_paste(&self) -> bool;
    /// Hosted redirect used in paste mode; default "" when unsupported.
    fn paste_redirect_uri(&self) -> &'static str {
        ""
    }
    async fn exchange(
        &self,
        http: &reqwest::Client,
        code: &str,
        pkce: &Pkce,
        redirect_uri: &str,
    ) -> Result<FreshTokens>;
    async fn identity(&self, http: &reqwest::Client, t: &FreshTokens) -> Result<LoginIdentity>;
    /// Patch the saved profile blob with fresh tokens and write it back to the
    /// profile slot ONLY. Never touches the live store.
    async fn persist(&self, email: &str, t: &FreshTokens) -> Result<()>;
}

/// Identity match against the row's email (case- and whitespace-insensitive).
/// Email is unique per account, so this alone gates persistence.
pub fn email_matches(expected: &str, got: &str) -> bool {
    expected.trim().to_lowercase() == got.trim().to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn email_match_is_case_and_space_insensitive() {
        assert!(email_matches("  Me@Example.com ", "me@example.com"));
        assert!(!email_matches("me@example.com", "other@example.com"));
    }

    // RFC 7636 Appendix B test vector.
    #[test]
    fn challenge_matches_rfc7636_vector() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(challenge_for(verifier), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn generate_is_url_safe_and_sized() {
        let p = Pkce::generate();
        let ok = |s: &str| s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        assert!(ok(&p.verifier) && ok(&p.challenge) && ok(&p.state));
        assert!(p.verifier.len() >= 43); // 64 random bytes -> ~86 chars base64url
        assert_eq!(p.challenge, challenge_for(&p.verifier));
        assert_ne!(p.state, p.verifier);
    }
}
