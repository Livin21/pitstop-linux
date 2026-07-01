//! Native OAuth `authorization_code` re-login (PKCE S256) for expired,
//! inactive Claude Code & Codex rows. Writes fresh tokens ONLY into the
//! saved-profile snapshot — never the live store.

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

#[cfg(test)]
mod tests {
    use super::*;

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
