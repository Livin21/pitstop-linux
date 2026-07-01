# In-app OAuth Re-login (Claude + Codex) Implementation Plan
> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (- [ ]) syntax for tracking.

**Goal:** Add a coral **Login** action to expired (token-rejected), inactive Claude Code & Codex rows that runs a native PKCE `authorization_code` flow and writes fresh tokens **only** into the saved-profile snapshot file — never the live store.
**Architecture:** New `oauth.rs` (PKCE + `LoginAdapter` trait + `ClaudeLoginAdapter`/`CodexLoginAdapter` + `run_login` coordinator) and `loopback.rs` (one-shot `tokio::net::TcpListener` capturing the browser callback). Login runs in a detached `tokio::spawn`ed task launched from `app.rs::perform_login`; on completion it posts an `Action::LoginFinished` back over the engine's mpsc channel to clear backoff and `refresh_all()`. Persisting reuses `secret_store` + `credentials::patch_blob` / `codex::patching`.
**Tech Stack:** reqwest (async HTTP + `reqwest::Url` for URL assembly/parsing), sha2 + rand (PKCE S256), async-trait (object-safe `dyn LoginAdapter`), serde_json, anyhow, tokio (net/process). Browser via `xdg-open`, paste via `zenity`, clipboard via `xclip`.
**Depends on:** none. **PRODUCES** the `oauth.rs` interface consumed by Plan 4 (Gemini) — documented below.

## Global Constraints
- Rust 2021; single tokio task (Engine::run tokio::select! loop over an mpsc Action channel + 120s timer); ksni tray; no new threads/locks in the render path. The login flow is the one exception: it runs in a detached `tokio::spawn` so the 90-180s browser wait never blocks the select loop; it reports back via an `Action`.
- Secrets only in 0600 files or the GNOME keyring; never logged. Re-login writes ONLY `~/.config/pitstop/accounts/<provider>-<sanitized-email>.json`; never `~/.claude/.credentials.json`, `~/.claude.json`, or `~/.codex/auth.json`.
- reqwest (async) for HTTP; serde/serde_json for JSON; chrono for time; anyhow for errors. Reuse the existing `ApiError` (usage_api.rs) / `CodexError` (codex.rs) — do NOT invent a parallel error type.
- Each task ends green: cargo build clean, cargo test passes, cargo clippy clean, one commit.

## Produced interface (`oauth.rs`) — Plan 4 conforms to this exact shape
```rust
pub struct Pkce { pub verifier: String, pub challenge: String, pub state: String }
impl Pkce { pub fn generate() -> Pkce; }
pub struct FreshTokens {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub id_token: Option<String>,
    pub expires_at_ms: i64,
}
pub struct LoginIdentity { pub email: String, pub account_id: Option<String> }

#[async_trait::async_trait]
pub trait LoginAdapter: Send + Sync {
    fn authorize_url(&self, pkce: &Pkce, redirect_uri: &str, paste_mode: bool) -> String;
    fn fixed_loopback_port(&self) -> Option<u16>;   // Codex Some(1455)->1457; Claude None (ephemeral)
    fn redirect_path(&self) -> &'static str;         // "/callback" (Claude) | "/auth/callback" (Codex)
    fn supports_paste(&self) -> bool;                // Claude true; Codex false
    fn paste_redirect_uri(&self) -> &'static str { "" } // Claude hosted callback; default "" for others
    async fn exchange(&self, http: &reqwest::Client, code: &str, pkce: &Pkce, redirect_uri: &str)
        -> anyhow::Result<FreshTokens>;
    async fn identity(&self, http: &reqwest::Client, t: &FreshTokens) -> anyhow::Result<LoginIdentity>;
    async fn persist(&self, email: &str, t: &FreshTokens) -> anyhow::Result<()>; // profile file only
}

pub struct ClaudeLoginAdapter;   // unit struct
pub struct CodexLoginAdapter;    // unit struct
pub fn email_matches(expected: &str, got: &str) -> bool;
pub async fn run_login(http: &reqwest::Client, adapter: &dyn LoginAdapter, target_email: &str)
    -> anyhow::Result<()>;
```
> Note: `paste_redirect_uri` is an addition to the contract sketch (with a default impl) so the coordinator can byte-match the paste-mode `redirect_uri`. Plan 4's `GeminiLoginAdapter` (`supports_paste()==false`) inherits the default and is unaffected.

---

### Task 1: Cargo deps + `oauth.rs` PKCE
**Files:** Modify: `Cargo.toml:7-17`, `src/main.rs:1-14` / Create: `src/oauth.rs` / Test: `src/oauth.rs` `#[cfg(test)]`
**Interfaces:** Produces: `oauth::Pkce { verifier, challenge, state }`, `Pkce::generate()`, `oauth::challenge_for(&str) -> String`.

- [ ] **Step 1: Write the failing test** — create `src/oauth.rs` containing only:
```rust
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
```
- [ ] **Step 2: Run test, verify it fails** \n Run: `cargo test --lib challenge_matches_rfc7636_vector` (or `cargo test challenge_matches`) \n Expected: FAIL — compile error `error[E0432]: unresolved import` for `rand`/`sha2`, and `error[E0583]: file not found for module 'oauth'` until the module is declared.
- [ ] **Step 3: Minimal implementation** — add deps to `Cargo.toml` under `[dependencies]` (after `png = "0.17"`):
```toml
sha2 = "0.10"
rand = "0.8"
async-trait = "0.1"
```
and declare the module in `src/main.rs` after `mod notify;` (line 9). Add `oauth` and `loopback` (created in Task 2) with a temporary dead-code allow that Task 8 removes once they are wired into the engine:
```rust
#[allow(dead_code)] // wired into the engine in the login task
mod oauth;
#[allow(dead_code)]
mod loopback;
```
(The `oauth.rs` body from Step 1 is the implementation.)
- [ ] **Step 4: Run test, verify it passes** \n Run: `cargo test challenge_matches && cargo test generate_is_url_safe` \n Expected: PASS
- [ ] **Step 5: Commit** \n `git add -A && git commit -m "oauth: PKCE S256 + sha2/rand/async-trait deps"`

---

### Task 2: `loopback.rs` callback/paste parsing (pure)
**Files:** Create: `src/loopback.rs` / Test: `src/loopback.rs` `#[cfg(test)]`
**Interfaces:** Produces: `loopback::Callback { code: String, state: String }`, `loopback::parse_query(&str) -> Option<Callback>`, `loopback::parse_pasted(&str) -> Option<Callback>`.

- [ ] **Step 1: Write the failing test** — create `src/loopback.rs`:
```rust
//! One-shot loopback HTTP callback server on 127.0.0.1 for the OAuth flow —
//! the Linux stand-in for macOS's raw-socket LoopbackServer. Captures the first
//! `GET /<path>?code=…&state=…`, replies 200, yields (code, state). Also parses
//! a value the user pastes from a hosted callback page.

pub struct Callback {
    pub code: String,
    pub state: String,
}

/// Parse a URL query string (`code=…&state=…`) with percent-decoding, by
/// reusing reqwest's URL parser.
pub fn parse_query(query: &str) -> Option<Callback> {
    let url = reqwest::Url::parse(&format!("http://127.0.0.1/?{query}")).ok()?;
    let mut code = None;
    let mut state = None;
    for (k, v) in url.query_pairs() {
        match k.as_ref() {
            "code" => code = Some(v.into_owned()),
            "state" => state = Some(v.into_owned()),
            _ => {}
        }
    }
    Some(Callback { code: code?, state: state? })
}

/// Parse a value pasted from a hosted callback page: a full redirect URL, a
/// `CODE#STATE` string, or a bare `code=…&state=…` query.
pub fn parse_pasted(input: &str) -> Option<Callback> {
    let s = input.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(url) = reqwest::Url::parse(s) {
        if let Some(q) = url.query() {
            if let Some(c) = parse_query(q) {
                return Some(c);
            }
        }
    }
    if !s.contains('=') {
        if let Some((code, state)) = s.split_once('#') {
            if !code.is_empty() && !state.is_empty() {
                return parse_query(&format!("code={code}&state={state}"));
            }
        }
    }
    parse_query(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_query_percent_decodes() {
        let c = parse_query("code=A%2FB&state=xyz").unwrap();
        assert_eq!(c.code, "A/B");
        assert_eq!(c.state, "xyz");
    }

    #[test]
    fn parse_pasted_full_url() {
        let c = parse_pasted(
            "https://platform.claude.com/oauth/code/callback?code=abc&state=xyz",
        )
        .unwrap();
        assert_eq!(c.code, "abc");
        assert_eq!(c.state, "xyz");
    }

    #[test]
    fn parse_pasted_code_hash_state() {
        let c = parse_pasted("theCode#theState").unwrap();
        assert_eq!(c.code, "theCode");
        assert_eq!(c.state, "theState");
    }

    #[test]
    fn parse_pasted_raw_query() {
        let c = parse_pasted("code=abc&state=xyz").unwrap();
        assert_eq!(c.code, "abc");
        assert_eq!(c.state, "xyz");
    }

    #[test]
    fn parse_query_missing_state_is_none() {
        assert!(parse_query("code=abc").is_none());
    }
}
```
(`mod loopback;` was already declared in Task 1 Step 3.)
- [ ] **Step 2: Run test, verify it fails** \n Run: `cargo test parse_pasted_full_url` \n Expected: FAIL — before the file exists this is `error[E0583]: file not found for module 'loopback'`; write the file, then it compiles and the four parse tests are the acceptance target.
- [ ] **Step 3: Minimal implementation** — the file body above IS the implementation (pure functions, no server yet).
- [ ] **Step 4: Run test, verify it passes** \n Run: `cargo test --lib parse_` \n Expected: PASS (all five `parse_*` tests)
- [ ] **Step 5: Commit** \n `git add -A && git commit -m "loopback: callback/paste query parsing"`

---

### Task 3: `loopback.rs` one-shot TcpListener (bind + wait)
**Files:** Modify: `src/loopback.rs` / Test: `src/loopback.rs` `#[cfg(test)]`
**Interfaces:** Consumes: `parse_query` (Task 2). Produces: `loopback::Loopback { port: u16 }`, `Loopback::bind(fixed_port: Option<u16>) -> anyhow::Result<Loopback>`, `Loopback::wait(self, timeout: Duration) -> anyhow::Result<Callback>`.

- [ ] **Step 1: Write the failing test** — add to `src/loopback.rs` `#[cfg(test)] mod tests`:
```rust
    use std::time::Duration;

    #[tokio::test]
    async fn wait_captures_first_callback() {
        let server = Loopback::bind(None).await.unwrap();
        let port = server.port;
        assert_ne!(port, 0);
        let client = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            s.write_all(b"GET /callback?code=abc&state=xyz HTTP/1.1\r\nHost: localhost\r\n\r\n")
                .await
                .unwrap();
            let mut buf = Vec::new();
            let _ = s.read_to_end(&mut buf).await;
            String::from_utf8_lossy(&buf).contains("200 OK")
        });
        let cap = server.wait(Duration::from_secs(5)).await.unwrap();
        assert!(client.await.unwrap());
        assert_eq!(cap.code, "abc");
        assert_eq!(cap.state, "xyz");
    }

    #[tokio::test]
    async fn wait_times_out_with_no_client() {
        let server = Loopback::bind(None).await.unwrap();
        let err = server.wait(Duration::from_millis(150)).await.unwrap_err();
        assert!(err.to_string().contains("timed out"));
    }
```
- [ ] **Step 2: Run test, verify it fails** \n Run: `cargo test wait_captures_first_callback` \n Expected: FAIL — `error[E0433]: failed to resolve: use of undeclared type 'Loopback'`.
- [ ] **Step 3: Minimal implementation** — add to the top of `src/loopback.rs` (imports) and the type below `parse_pasted`:
```rust
use anyhow::{bail, Result};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

pub struct Loopback {
    listener: TcpListener,
    pub port: u16,
}

impl Loopback {
    /// Bind 127.0.0.1. `Some(p)` tries the fixed port then `p + 2` (Codex
    /// 1455 -> 1457); `None` binds an OS-assigned ephemeral port (Claude).
    pub async fn bind(fixed_port: Option<u16>) -> Result<Loopback> {
        let ports: Vec<u16> = match fixed_port {
            Some(p) => vec![p, p + 2],
            None => vec![0],
        };
        for p in ports {
            if let Ok(listener) = TcpListener::bind(("127.0.0.1", p)).await {
                let port = listener.local_addr()?.port();
                return Ok(Loopback { listener, port });
            }
        }
        bail!("no free loopback port (a sign-in may already be in progress)")
    }

    /// Await the first `GET …?code=…&state=…`, reply 200, and return it.
    /// Requests without a parseable code (probes) are answered and ignored.
    pub async fn wait(self, timeout: Duration) -> Result<Callback> {
        let accept = async {
            loop {
                let (mut stream, _) = self.listener.accept().await?;
                let mut buf = vec![0u8; 8192];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let text = String::from_utf8_lossy(&buf[..n]);
                let first = text.lines().next().unwrap_or("");
                let cap = first
                    .split_whitespace()
                    .nth(1)
                    .and_then(|path| path.split_once('?'))
                    .and_then(|(_, q)| parse_query(q));
                let body = "You can close this tab and return to PitStop.";
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(resp.as_bytes()).await;
                let _ = stream.shutdown().await;
                if let Some(c) = cap {
                    return Ok::<Callback, anyhow::Error>(c);
                }
            }
        };
        match tokio::time::timeout(timeout, accept).await {
            Ok(r) => r,
            Err(_) => bail!("timed out waiting for the browser callback"),
        }
    }
}
```
- [ ] **Step 4: Run test, verify it passes** \n Run: `cargo test wait_captures_first_callback && cargo test wait_times_out` \n Expected: PASS
- [ ] **Step 5: Commit** \n `git add -A && git commit -m "loopback: one-shot 127.0.0.1 callback server"`

---

### Task 4: `oauth.rs` — `LoginAdapter` trait, `email_matches`, `FreshTokens`/`LoginIdentity`
**Files:** Modify: `src/oauth.rs` / Test: `src/oauth.rs` `#[cfg(test)]`
**Interfaces:** Produces: `FreshTokens`, `LoginIdentity`, `LoginAdapter` (async-trait), `email_matches(&str,&str) -> bool`. Consumes: `Pkce` (Task 1).

- [ ] **Step 1: Write the failing test** — add to `src/oauth.rs` `#[cfg(test)] mod tests`:
```rust
    #[test]
    fn email_match_is_case_and_space_insensitive() {
        assert!(email_matches("  Me@Example.com ", "me@example.com"));
        assert!(!email_matches("me@example.com", "other@example.com"));
    }
```
- [ ] **Step 2: Run test, verify it fails** \n Run: `cargo test email_match_is_case_and_space_insensitive` \n Expected: FAIL — `error[E0425]: cannot find function 'email_matches'`.
- [ ] **Step 3: Minimal implementation** — add to `src/oauth.rs` (below the `Pkce` impl). Extend the top-of-file imports with `use anyhow::Result;`:
```rust
/// Fresh tokens from an authorization_code exchange, provider-neutral.
pub struct FreshTokens {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub id_token: Option<String>,
    pub expires_at_ms: i64,
}

/// The authenticated identity, for matching against the target row.
pub struct LoginIdentity {
    pub email: String,
    pub account_id: Option<String>,
}

/// The provider-varying surface of the OAuth login flow.
#[async_trait::async_trait]
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
```
- [ ] **Step 4: Run test, verify it passes** \n Run: `cargo test email_match_is_case_and_space_insensitive` \n Expected: PASS
- [ ] **Step 5: Commit** \n `git add -A && git commit -m "oauth: LoginAdapter trait + FreshTokens/LoginIdentity + email_matches"`

---

### Task 5: `ClaudeLoginAdapter` (authorize URL, exchange, identity, persist)
**Files:** Modify: `src/oauth.rs` / Test: `src/oauth.rs` `#[cfg(test)]`
**Interfaces:** Consumes: `credentials::{LIVE_PROVIDER, patch_blob}`, `secret_store::{read, write}`, `usage_api::ApiError`, `util::now_secs`. Produces: `oauth::ClaudeLoginAdapter` (impl `LoginAdapter`).
**[verify]:** the token host (`platform.claude.com` primary, `console.anthropic.com` fallback) and the `/api/oauth/profile` identity endpoint are `[verify]` in the spec. Unit tests below cover the pure URL/persist parts; the network paths are validated by the Task 8 manual E2E (heal an expired Claude row). Keep the fallback loop so a 404 on one host tries the other.

- [ ] **Step 1: Write the failing test** — add to `src/oauth.rs` `#[cfg(test)] mod tests`:
```rust
    use std::collections::HashMap;

    fn qmap(url: &str) -> HashMap<String, String> {
        reqwest::Url::parse(url).unwrap().query_pairs().into_owned().collect()
    }

    #[test]
    fn claude_authorize_url_params() {
        let pkce = Pkce { verifier: "v".into(), challenge: "CH".into(), state: "ST".into() };
        let url = ClaudeLoginAdapter.authorize_url(&pkce, "http://localhost:5000/callback", false);
        assert!(url.starts_with("https://claude.ai/oauth/authorize?"));
        let q = qmap(&url);
        assert_eq!(q["client_id"], "9d1c250a-e61b-44d9-88ed-5944d1962f5e");
        assert_eq!(q["response_type"], "code");
        assert_eq!(q["redirect_uri"], "http://localhost:5000/callback");
        assert_eq!(q["code_challenge"], "CH");
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(q["state"], "ST");
        assert!(q.get("code").is_none());
    }

    #[test]
    fn claude_authorize_url_paste_mode_adds_code_true() {
        let pkce = Pkce { verifier: "v".into(), challenge: "CH".into(), state: "ST".into() };
        let url = ClaudeLoginAdapter.authorize_url(
            &pkce,
            "https://platform.claude.com/oauth/code/callback",
            true,
        );
        assert_eq!(qmap(&url)["code"], "true");
    }

    #[tokio::test]
    async fn claude_persist_patches_and_preserves_plan_fields() {
        // Shared fixed dir across the persist tests: both set the SAME env value
        // so parallel interleaving is harmless; unique emails keep files distinct.
        let dir = std::env::temp_dir().join("pitstop-oauth-relogin-tests");
        std::env::set_var("XDG_CONFIG_HOME", &dir);
        let email = "persist-claude@example.com";
        let old = br#"{"claudeAiOauth":{"accessToken":"old","refreshToken":"oldR","expiresAt":1,"subscriptionType":"max","rateLimitTier":"default_claude_max_20x"},"mcpOAuth":{"keep":"me"}}"#;
        crate::secret_store::write(crate::credentials::LIVE_PROVIDER, email, old).unwrap();
        let tokens = FreshTokens {
            access_token: "newA".into(),
            refresh_token: Some("newR".into()),
            id_token: None,
            expires_at_ms: 999000,
        };
        ClaudeLoginAdapter.persist(email, &tokens).await.unwrap();
        let saved = crate::secret_store::read(crate::credentials::LIVE_PROVIDER, email)
            .unwrap()
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&saved).unwrap();
        assert_eq!(v["claudeAiOauth"]["accessToken"], "newA");
        assert_eq!(v["claudeAiOauth"]["refreshToken"], "newR");
        assert_eq!(v["claudeAiOauth"]["expiresAt"], 999000);
        assert_eq!(v["claudeAiOauth"]["subscriptionType"], "max");
        assert_eq!(v["claudeAiOauth"]["rateLimitTier"], "default_claude_max_20x");
        assert_eq!(v["mcpOAuth"]["keep"], "me"); // untouched
    }
```
- [ ] **Step 2: Run test, verify it fails** \n Run: `cargo test claude_authorize_url_params` \n Expected: FAIL — `error[E0433]: failed to resolve: use of undeclared type 'ClaudeLoginAdapter'`.
- [ ] **Step 3: Minimal implementation** — add to `src/oauth.rs`. Extend imports with:
```rust
use crate::credentials;
use crate::secret_store;
use crate::usage_api::ApiError;
use crate::util::now_secs;
use serde_json::{json, Value};
use std::time::Duration;
```
Then add:
```rust
const CLAUDE_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const CLAUDE_AUTHORIZE: &str = "https://claude.ai/oauth/authorize";
const CLAUDE_TOKEN_HOSTS: [&str; 2] = [
    "https://platform.claude.com/v1/oauth/token",
    "https://console.anthropic.com/v1/oauth/token",
];
const CLAUDE_PROFILE_URL: &str = "https://api.anthropic.com/api/oauth/profile";
const CLAUDE_PASTE_REDIRECT: &str = "https://platform.claude.com/oauth/code/callback";
const CLAUDE_SCOPES: &str = "org:create_api_key user:profile user:inference \
user:sessions:claude_code user:mcp_servers user:file_upload";

pub struct ClaudeLoginAdapter;

#[async_trait::async_trait]
impl LoginAdapter for ClaudeLoginAdapter {
    fn authorize_url(&self, pkce: &Pkce, redirect_uri: &str, paste_mode: bool) -> String {
        let mut url = reqwest::Url::parse(CLAUDE_AUTHORIZE).unwrap();
        {
            let mut q = url.query_pairs_mut();
            if paste_mode {
                q.append_pair("code", "true");
            }
            q.append_pair("client_id", CLAUDE_CLIENT_ID)
                .append_pair("response_type", "code")
                .append_pair("redirect_uri", redirect_uri)
                .append_pair("scope", CLAUDE_SCOPES)
                .append_pair("code_challenge", &pkce.challenge)
                .append_pair("code_challenge_method", "S256")
                .append_pair("state", &pkce.state);
        }
        url.to_string()
    }

    fn fixed_loopback_port(&self) -> Option<u16> {
        None
    }
    fn redirect_path(&self) -> &'static str {
        "/callback"
    }
    fn supports_paste(&self) -> bool {
        true
    }
    fn paste_redirect_uri(&self) -> &'static str {
        CLAUDE_PASTE_REDIRECT
    }

    async fn exchange(
        &self,
        http: &reqwest::Client,
        code: &str,
        pkce: &Pkce,
        redirect_uri: &str,
    ) -> Result<FreshTokens> {
        // Try platform host first, fall through to console ONLY on a 404
        // (endpoint not on this host) or a transport error — never replay a
        // single-use code against a host that already answered definitively.
        let mut last: anyhow::Error = ApiError::Malformed.into();
        for host in CLAUDE_TOKEN_HOSTS {
            let body = json!({
                "grant_type": "authorization_code",
                "code": code,
                "state": pkce.state,
                "client_id": CLAUDE_CLIENT_ID,
                "redirect_uri": redirect_uri,
                "code_verifier": pkce.verifier,
            });
            let resp = match http
                .post(host)
                .header("Content-Type", "application/json")
                .timeout(Duration::from_secs(15))
                .json(&body)
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    last = ApiError::Network(e.to_string()).into();
                    continue;
                }
            };
            let status = resp.status().as_u16();
            if status == 404 {
                last = ApiError::Http(404).into();
                continue;
            }
            if status == 400 || status == 401 || status == 403 {
                return Err(ApiError::Unauthorized.into());
            }
            if status != 200 {
                return Err(ApiError::Http(status).into());
            }
            let root: Value = resp.json().await.map_err(|_| ApiError::Malformed)?;
            let access = root
                .get("access_token")
                .and_then(Value::as_str)
                .ok_or(ApiError::Malformed)?;
            let expires_in = root
                .get("expires_in")
                .and_then(Value::as_f64)
                .ok_or(ApiError::Malformed)?;
            return Ok(FreshTokens {
                access_token: access.to_string(),
                refresh_token: root.get("refresh_token").and_then(Value::as_str).map(String::from),
                id_token: None,
                expires_at_ms: ((now_secs() + expires_in) * 1000.0) as i64,
            });
        }
        Err(last)
    }

    async fn identity(&self, http: &reqwest::Client, t: &FreshTokens) -> Result<LoginIdentity> {
        let resp = http
            .get(CLAUDE_PROFILE_URL)
            .header("Authorization", format!("Bearer {}", t.access_token))
            .header("anthropic-beta", "oauth-2025-04-20")
            .header("Content-Type", "application/json")
            .timeout(Duration::from_secs(15))
            .send()
            .await
            .map_err(|e| ApiError::Network(e.to_string()))?;
        let status = resp.status().as_u16();
        if status == 401 || status == 403 {
            return Err(ApiError::Unauthorized.into());
        }
        if status != 200 {
            return Err(ApiError::Http(status).into());
        }
        let root: Value = resp.json().await.map_err(|_| ApiError::Malformed)?;
        let email = root
            .get("email")
            .and_then(Value::as_str)
            .or_else(|| root.get("email_address").and_then(Value::as_str))
            .or_else(|| root.get("account").and_then(|a| a.get("email_address")).and_then(Value::as_str))
            .or_else(|| root.get("account").and_then(|a| a.get("email")).and_then(Value::as_str))
            .ok_or(ApiError::Malformed)?;
        Ok(LoginIdentity { email: email.to_string(), account_id: None })
    }

    async fn persist(&self, email: &str, t: &FreshTokens) -> Result<()> {
        let old = secret_store::read(credentials::LIVE_PROVIDER, email)?
            .ok_or_else(|| anyhow::anyhow!("No saved profile for {email} — save the account once, then retry"))?;
        let patched = credentials::patch_blob(
            &old,
            &t.access_token,
            t.refresh_token.as_deref(),
            t.expires_at_ms as f64,
        )?;
        secret_store::write(credentials::LIVE_PROVIDER, email, &patched)
    }
}
```
- [ ] **Step 4: Run test, verify it passes** \n Run: `cargo test claude_authorize_url_params && cargo test claude_authorize_url_paste_mode && cargo test claude_persist_patches` \n Expected: PASS
- [ ] **Step 5: Commit** \n `git add -A && git commit -m "oauth: ClaudeLoginAdapter (authorize/exchange/identity/persist)"`

---

### Task 6: Codex exchange/identity helpers + `CodexLoginAdapter`
**Files:** Modify: `src/codex.rs:19-21` (make `CLIENT_ID` pub), `src/codex.rs` (add exchange_code + identity_from_id_token), `src/oauth.rs` / Test: `src/codex.rs` `#[cfg(test)]` + `src/oauth.rs` `#[cfg(test)]`
**Interfaces:** Consumes: `codex::{CLIENT_ID, exchange_code, identity_from_id_token, patching, normalized_blob, Refreshed, PROVIDER, TOKEN_URL}`, `secret_store::{read, write}`. Produces: `oauth::CodexLoginAdapter` (impl `LoginAdapter`); `codex::exchange_code`, `codex::identity_from_id_token`.

- [ ] **Step 1: Write the failing test** — add to `src/codex.rs` `#[cfg(test)] mod tests` (create the module if absent):
```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn make_jwt(claims: serde_json::Value) -> String {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
        let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap());
        format!("{header}.{payload}.")
    }

    #[test]
    fn identity_from_id_token_reads_email_and_account() {
        let jwt = make_jwt(json!({
            "email": "me@example.com",
            "https://api.openai.com/auth": { "chatgpt_account_id": "acc_123" }
        }));
        let (email, acc) = identity_from_id_token(&jwt).unwrap();
        assert_eq!(email, "me@example.com");
        assert_eq!(acc.as_deref(), Some("acc_123"));
    }

    #[test]
    fn identity_from_id_token_falls_back_to_profile_email() {
        let jwt = make_jwt(json!({
            "https://api.openai.com/profile": { "email": "p@example.com" }
        }));
        let (email, acc) = identity_from_id_token(&jwt).unwrap();
        assert_eq!(email, "p@example.com");
        assert!(acc.is_none());
    }
}
```
And add to `src/oauth.rs` `#[cfg(test)] mod tests`:
```rust
    #[test]
    fn codex_authorize_url_params() {
        let pkce = Pkce { verifier: "v".into(), challenge: "CH".into(), state: "ST".into() };
        let url = CodexLoginAdapter.authorize_url(&pkce, "http://localhost:1455/auth/callback", false);
        assert!(url.starts_with("https://auth.openai.com/oauth/authorize?"));
        let q = qmap(&url);
        assert_eq!(q["client_id"], "app_EMoamEEZ73f0CkXaXp7hrann");
        assert_eq!(q["response_type"], "code");
        assert_eq!(q["redirect_uri"], "http://localhost:1455/auth/callback");
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(q["id_token_add_organizations"], "true");
        assert_eq!(q["codex_cli_simplified_flow"], "true");
        assert_eq!(q["originator"], "codex_cli_rs");
        assert_eq!(q["state"], "ST");
    }

    #[tokio::test]
    async fn codex_persist_stays_compact_and_preserves_fields() {
        // Same shared fixed dir as the Claude persist test (see note there).
        let dir = std::env::temp_dir().join("pitstop-oauth-relogin-tests");
        std::env::set_var("XDG_CONFIG_HOME", &dir);
        let email = "persist-codex@example.com";
        let old = br#"{"OPENAI_API_KEY":"sk-x","last_refresh":"2020-01-01T00:00:00.000Z","tokens":{"access_token":"old","account_id":"acc_1","id_token":"oldI","refresh_token":"oldR"}}"#;
        crate::secret_store::write(crate::codex::PROVIDER, email, old).unwrap();
        let tokens = FreshTokens {
            access_token: "newA".into(),
            refresh_token: Some("newR".into()),
            id_token: Some("newI".into()),
            expires_at_ms: 0,
        };
        CodexLoginAdapter.persist(email, &tokens).await.unwrap();
        let saved = crate::secret_store::read(crate::codex::PROVIDER, email).unwrap().unwrap();
        let text = String::from_utf8(saved.clone()).unwrap();
        assert!(!text.contains(": ")); // compact, no pretty spaces
        let v: serde_json::Value = serde_json::from_slice(&saved).unwrap();
        assert_eq!(v["tokens"]["access_token"], "newA");
        assert_eq!(v["tokens"]["refresh_token"], "newR");
        assert_eq!(v["tokens"]["id_token"], "newI");
        assert_eq!(v["tokens"]["account_id"], "acc_1"); // preserved
        assert_eq!(v["OPENAI_API_KEY"], "sk-x"); // preserved
        assert_ne!(v["last_refresh"], "2020-01-01T00:00:00.000Z"); // bumped
    }
```
- [ ] **Step 2: Run test, verify it fails** \n Run: `cargo test identity_from_id_token_reads && cargo test codex_authorize_url_params` \n Expected: FAIL — `cannot find function 'identity_from_id_token'` and `use of undeclared type 'CodexLoginAdapter'`.
- [ ] **Step 3: Minimal implementation** —
  (a) In `src/codex.rs` line 20 change `const CLIENT_ID` to `pub const CLIENT_ID`, and line 18 change `const TOKEN_URL` to `pub const TOKEN_URL`.
  (b) In `src/codex.rs`, after `patching` (line 242), add:
```rust
/// Exchange an authorization code for Codex tokens. Form-urlencoded, no `state`
/// in the body — the shape the Codex CLI uses. Used only for re-login.
pub async fn exchange_code(
    client: &reqwest::Client,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<Refreshed, CodexError> {
    let params = [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("client_id", CLIENT_ID),
        ("code_verifier", verifier),
    ];
    let resp = client
        .post(TOKEN_URL)
        .form(&params)
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .map_err(|_| CodexError::Malformed)?;
    let status = resp.status().as_u16();
    if status == 400 || status == 401 || status == 403 {
        return Err(CodexError::SessionExpired);
    }
    if status != 200 {
        return Err(CodexError::Malformed);
    }
    let root: Value = resp.json().await.map_err(|_| CodexError::Malformed)?;
    let access = root
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or(CodexError::Malformed)?;
    Ok(Refreshed {
        access_token: access.to_string(),
        refresh_token: root.get("refresh_token").and_then(Value::as_str).map(String::from),
        id_token: root.get("id_token").and_then(Value::as_str).map(String::from),
    })
}

/// Decode identity (email + ChatGPT account id) from an id_token JWT.
pub fn identity_from_id_token(id_token: &str) -> Option<(String, Option<String>)> {
    let claims = decode_jwt_claims(id_token)?;
    let email = claims
        .get("email")
        .and_then(Value::as_str)
        .or_else(|| {
            claims
                .get("https://api.openai.com/profile")
                .and_then(|p| p.get("email"))
                .and_then(Value::as_str)
        })?
        .to_string();
    let account_id = claims
        .get("https://api.openai.com/auth")
        .and_then(|a| a.get("chatgpt_account_id"))
        .and_then(Value::as_str)
        .map(String::from);
    Some((email, account_id))
}
```
  (c) In `src/oauth.rs`, add `use crate::codex;` to imports and add:
```rust
const CODEX_AUTHORIZE: &str = "https://auth.openai.com/oauth/authorize";
const CODEX_SCOPES: &str = "openid profile email offline_access api.connectors.read api.connectors.invoke";

pub struct CodexLoginAdapter;

#[async_trait::async_trait]
impl LoginAdapter for CodexLoginAdapter {
    fn authorize_url(&self, pkce: &Pkce, redirect_uri: &str, _paste_mode: bool) -> String {
        let mut url = reqwest::Url::parse(CODEX_AUTHORIZE).unwrap();
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", codex::CLIENT_ID)
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("scope", CODEX_SCOPES)
            .append_pair("code_challenge", &pkce.challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("id_token_add_organizations", "true")
            .append_pair("codex_cli_simplified_flow", "true")
            .append_pair("originator", "codex_cli_rs")
            .append_pair("state", &pkce.state);
        url.to_string()
    }

    fn fixed_loopback_port(&self) -> Option<u16> {
        Some(1455)
    }
    fn redirect_path(&self) -> &'static str {
        "/auth/callback"
    }
    fn supports_paste(&self) -> bool {
        false
    }

    async fn exchange(
        &self,
        http: &reqwest::Client,
        code: &str,
        pkce: &Pkce,
        redirect_uri: &str,
    ) -> Result<FreshTokens> {
        let r = codex::exchange_code(http, code, &pkce.verifier, redirect_uri).await?;
        Ok(FreshTokens {
            access_token: r.access_token,
            refresh_token: r.refresh_token,
            id_token: r.id_token,
            expires_at_ms: 0, // Codex derives expiry from id_token on refresh; not stored here
        })
    }

    async fn identity(&self, _http: &reqwest::Client, t: &FreshTokens) -> Result<LoginIdentity> {
        let idt = t
            .id_token
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Codex sign-in returned no id_token"))?;
        let (email, account_id) = codex::identity_from_id_token(idt)
            .ok_or_else(|| anyhow::anyhow!("Could not read Codex identity from id_token"))?;
        Ok(LoginIdentity { email, account_id })
    }

    async fn persist(&self, email: &str, t: &FreshTokens) -> Result<()> {
        let old = secret_store::read(codex::PROVIDER, email)?
            .ok_or_else(|| anyhow::anyhow!("No saved Codex profile for {email}"))?;
        let refreshed = codex::Refreshed {
            access_token: t.access_token.clone(),
            refresh_token: t.refresh_token.clone(),
            id_token: t.id_token.clone(),
        };
        let patched = codex::patching(&old, &refreshed)
            .ok_or_else(|| anyhow::anyhow!("Could not patch Codex credentials"))?;
        secret_store::write(codex::PROVIDER, email, &codex::normalized_blob(&patched))
    }
}
```
> `codex::exchange_code` returns `CodexError`, which implements `std::error::Error + Send + Sync`, so `?` lifts it into `anyhow::Result` — no parallel error type. `Duration` and `json!`/`Value` are already imported in `codex.rs`.
- [ ] **Step 4: Run test, verify it passes** \n Run: `cargo test identity_from_id_token && cargo test codex_authorize_url_params && cargo test codex_persist_stays_compact` \n Expected: PASS
- [ ] **Step 5: Commit** \n `git add -A && git commit -m "oauth+codex: CodexLoginAdapter + authorization_code exchange/identity"`

---

### Task 7: `run_login` coordinator (browser, loopback, paste, identity gate)
**Files:** Modify: `src/oauth.rs` / Test: `src/oauth.rs` `#[cfg(test)]`
**Interfaces:** Consumes: `loopback::{Loopback, parse_pasted}`, `crate::notify`, `LoginAdapter`. Produces: `oauth::run_login(&reqwest::Client, &dyn LoginAdapter, &str) -> anyhow::Result<()>` (the entry point Plan 4 and `app.rs` call).

- [ ] **Step 1: Write the failing test** — add to `src/oauth.rs` `#[cfg(test)] mod tests` (exercises the private `finish` gate with an in-memory adapter — no network):
```rust
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    struct FakeAdapter {
        identity_email: String,
        persisted: Arc<AtomicBool>,
    }

    #[async_trait::async_trait]
    impl LoginAdapter for FakeAdapter {
        fn authorize_url(&self, _p: &Pkce, _r: &str, _paste: bool) -> String {
            String::new()
        }
        fn fixed_loopback_port(&self) -> Option<u16> {
            None
        }
        fn redirect_path(&self) -> &'static str {
            "/callback"
        }
        fn supports_paste(&self) -> bool {
            false
        }
        async fn exchange(&self, _h: &reqwest::Client, _c: &str, _p: &Pkce, _r: &str) -> Result<FreshTokens> {
            Ok(FreshTokens { access_token: "at".into(), refresh_token: None, id_token: None, expires_at_ms: 0 })
        }
        async fn identity(&self, _h: &reqwest::Client, _t: &FreshTokens) -> Result<LoginIdentity> {
            Ok(LoginIdentity { email: self.identity_email.clone(), account_id: None })
        }
        async fn persist(&self, _email: &str, _t: &FreshTokens) -> Result<()> {
            self.persisted.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    #[tokio::test]
    async fn finish_persists_on_email_match() {
        let flag = Arc::new(AtomicBool::new(false));
        let a = FakeAdapter { identity_email: "Me@Example.com".into(), persisted: flag.clone() };
        let pkce = Pkce::generate();
        finish(&reqwest::Client::new(), &a, "me@example.com", "code", &pkce, "http://localhost/callback")
            .await
            .unwrap();
        assert!(flag.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn finish_skips_persist_on_mismatch() {
        let flag = Arc::new(AtomicBool::new(false));
        let a = FakeAdapter { identity_email: "other@example.com".into(), persisted: flag.clone() };
        let pkce = Pkce::generate();
        let err = finish(&reqwest::Client::new(), &a, "me@example.com", "code", &pkce, "http://x/callback")
            .await
            .unwrap_err();
        assert!(!flag.load(Ordering::SeqCst));
        assert!(err.to_string().contains("other@example.com"));
    }
```
- [ ] **Step 2: Run test, verify it fails** \n Run: `cargo test finish_persists_on_email_match` \n Expected: FAIL — `error[E0425]: cannot find function 'finish'`.
- [ ] **Step 3: Minimal implementation** — add to `src/oauth.rs`. Extend imports with `use crate::loopback;` and `use std::process::Stdio;`:
```rust
const LOOPBACK_TIMEOUT_SECS: u64 = 150;

/// Run one OAuth re-login end to end. Loopback first; Claude falls back to a
/// zenity paste prompt. Writes only to the profile slot, and only when the
/// browser identity matches `target_email`.
pub async fn run_login(
    http: &reqwest::Client,
    adapter: &dyn LoginAdapter,
    target_email: &str,
) -> Result<()> {
    let pkce = Pkce::generate();

    // --- Attempt A: loopback ---
    match loopback::Loopback::bind(adapter.fixed_loopback_port()).await {
        Ok(server) => {
            let redirect_uri = format!("http://localhost:{}{}", server.port, adapter.redirect_path());
            let auth_url = adapter.authorize_url(&pkce, &redirect_uri, false);
            open_browser(&auth_url);
            match server.wait(Duration::from_secs(LOOPBACK_TIMEOUT_SECS)).await {
                Ok(cap) => {
                    if cap.state != pkce.state {
                        anyhow::bail!("Sign-in could not be verified (state mismatch)");
                    }
                    return finish(http, adapter, target_email, &cap.code, &pkce, &redirect_uri).await;
                }
                Err(e) => {
                    if !adapter.supports_paste() {
                        return Err(e);
                    }
                    // fall through to paste
                }
            }
        }
        Err(e) => {
            if !adapter.supports_paste() {
                return Err(e);
            }
        }
    }

    // --- Attempt B: paste (Claude) ---
    run_paste(http, adapter, target_email, &pkce).await
}

async fn run_paste(
    http: &reqwest::Client,
    adapter: &dyn LoginAdapter,
    target_email: &str,
    pkce: &Pkce,
) -> Result<()> {
    let redirect_uri = adapter.paste_redirect_uri().to_string();
    let auth_url = adapter.authorize_url(pkce, &redirect_uri, true);
    open_browser(&auth_url);
    let pasted = prompt_paste(&auth_url)
        .await
        .ok_or_else(|| anyhow::anyhow!("Sign-in was cancelled"))?;
    let cap = loopback::parse_pasted(&pasted)
        .ok_or_else(|| anyhow::anyhow!("Could not read the pasted code"))?;
    if cap.state != pkce.state {
        anyhow::bail!("Sign-in could not be verified (state mismatch)");
    }
    finish(http, adapter, target_email, &cap.code, pkce, &redirect_uri).await
}

/// Exchange → identity → email gate → persist. Nothing is written on mismatch.
async fn finish(
    http: &reqwest::Client,
    adapter: &dyn LoginAdapter,
    expected_email: &str,
    code: &str,
    pkce: &Pkce,
    redirect_uri: &str,
) -> Result<()> {
    let tokens = adapter.exchange(http, code, pkce, redirect_uri).await?;
    let identity = adapter.identity(http, &tokens).await?;
    if !email_matches(expected_email, &identity.email) {
        anyhow::bail!(
            "You signed in as {}, but this row is {expected_email}. \
             Switch accounts in your browser and try again.",
            identity.email
        );
    }
    adapter.persist(expected_email, &tokens).await
}

fn open_browser(url: &str) {
    let _ = std::process::Command::new("xdg-open")
        .arg(url)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

/// Ask for the pasted code via `zenity --entry`. If zenity is absent, copy the
/// authorize URL to the clipboard via `xclip`, notify, and give up (None).
async fn prompt_paste(auth_url: &str) -> Option<String> {
    let out = tokio::process::Command::new("zenity")
        .arg("--entry")
        .arg("--title=PitStop sign-in")
        .arg("--text=Paste the code from your browser (authorization code, CODE#STATE, or the full redirect URL):")
        .arg("--width=440")
        .output()
        .await;
    match out {
        Ok(o) if o.status.success() => {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if s.is_empty() {
                None
            } else {
                Some(s)
            }
        }
        Ok(_) => None, // user cancelled zenity
        Err(_) => {
            copy_to_clipboard(auth_url);
            crate::notify::post(
                "PitStop sign-in — action needed",
                "Install `zenity` to paste the sign-in code. The sign-in URL was copied to your clipboard; approve in the browser, then retry.",
            );
            None
        }
    }
}

fn copy_to_clipboard(text: &str) {
    use std::io::Write;
    if let Ok(mut child) = std::process::Command::new("xclip")
        .args(["-selection", "clipboard"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(text.as_bytes());
        }
        let _ = child.wait();
    }
}
```
- [ ] **Step 4: Run test, verify it passes** \n Run: `cargo test finish_persists_on_email_match && cargo test finish_skips_persist_on_mismatch` \n Expected: PASS
- [ ] **Step 5: Commit** \n `git add -A && git commit -m "oauth: run_login coordinator (loopback + zenity paste + identity gate)"`

---

### Task 8: `app.rs` wiring — `Action::Login`, `perform_login`, `login_in_flight`, row eligibility
**Files:** Modify: `src/app.rs:22-24,28-37,57-105,146-201,772-779`, `src/tray.rs:24-31`, `src/main.rs:37-48` / Test: `src/app.rs` `#[cfg(test)]`
**Interfaces:** Consumes: `oauth::{run_login, ClaudeLoginAdapter, CodexLoginAdapter, LoginAdapter}`. Produces: `Action::Login { key }`, `Action::LoginFinished { key, result }`, `app::login_eligible(bool, bool) -> bool`; `RowView.login: bool`.

- [ ] **Step 1: Write the failing test** — add to `src/app.rs` a `#[cfg(test)]` module at the end of the file:
```rust
#[cfg(test)]
mod tests {
    use super::login_eligible;

    #[test]
    fn login_eligible_requires_needs_action_and_inactive() {
        assert!(login_eligible(true, false)); // flagged + inactive -> Login
        assert!(!login_eligible(true, true)); // active row -> never
        assert!(!login_eligible(false, false)); // not flagged -> Switch
    }
}
```
- [ ] **Step 2: Run test, verify it fails** \n Run: `cargo test login_eligible_requires_needs_action_and_inactive` \n Expected: FAIL — `error[E0432]: unresolved import 'super::login_eligible'`.
- [ ] **Step 3: Minimal implementation** —
  (a) `src/app.rs` line 24: extend the mpsc import to `use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};`. Add `use crate::oauth::{self, LoginAdapter};` near the other `use crate::` lines (after line 19).
  (b) `src/app.rs` Action enum (lines 28-37): add two variants:
```rust
    Login { key: String },
    LoginFinished { key: String, result: Result<(), String> },
```
  (c) `src/app.rs` Engine struct (after `next_periodic: Instant,` line 79): add fields:
```rust
    action_tx: UnboundedSender<Action>,
    login_in_flight: bool,
```
  (d) `src/app.rs` `Engine::new` (line 83): change the signature to
```rust
    pub fn new(handle: Handle<PitStopTray>, action_tx: UnboundedSender<Action>) -> Self {
```
and in the struct literal (after `next_periodic: Instant::now() + REFRESH_INTERVAL,` line 103) add:
```rust
            action_tx,
            login_in_flight: false,
```
  (e) `src/app.rs` `handle_action` match (before `Action::Quit` at line 199): add:
```rust
            Action::Login { key } => {
                self.perform_login(key).await;
            }
            Action::LoginFinished { key, result } => {
                self.login_in_flight = false;
                match result {
                    Ok(()) => {
                        self.next_fetch_allowed.remove(&key);
                        self.failure_count.insert(key.clone(), 0);
                        self.needs_action.remove(&key);
                        notify::post("Signed in", "Re-authenticated — refreshing usage…");
                        self.refresh_all().await;
                        self.render().await;
                    }
                    Err(e) => {
                        notify::post("Sign-in failed", &e);
                        self.render().await;
                    }
                }
            }
```
  (f) `src/app.rs` add the method + free function (place `perform_login` next to `perform_switch`, ~line 566; place `login_eligible` next to `pick_auto_switch` at the bottom):
```rust
    async fn perform_login(&mut self, key: String) {
        if self.login_in_flight {
            notify::post(
                "Sign-in already in progress",
                "Finish or cancel the current sign-in before starting another.",
            );
            return;
        }
        self.login_in_flight = true;
        let (email, adapter): (String, Box<dyn LoginAdapter>) =
            if let Some(e) = key.strip_prefix("codex:") {
                (e.to_string(), Box::new(oauth::CodexLoginAdapter))
            } else {
                (key.clone(), Box::new(oauth::ClaudeLoginAdapter))
            };
        let http = self.client.clone();
        let tx = self.action_tx.clone();
        tokio::spawn(async move {
            let result = oauth::run_login(&http, adapter.as_ref(), &email)
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(Action::LoginFinished { key, result });
        });
    }
```
```rust
/// A row shows the coral Login action instead of Switch when its key is flagged
/// `needs_action` (token rejected) AND the row is inactive (switchable).
pub fn login_eligible(in_needs_action: bool, is_active: bool) -> bool {
    in_needs_action && !is_active
}
```
  (g) `src/app.rs` `build_row` — in the `RowView { … }` literal (lines 772-779) add a field:
```rust
            login: login_eligible(self.needs_action.contains(&key), account.is_active),
```
  (h) `src/tray.rs` `RowView` struct (lines 24-31): add `pub login: bool,` after `switch_key: String,`.
  (i) `src/tray.rs` menu render (lines 122-131): replace the `if row.switchable { … }` switch item with a Login-aware label:
```rust
                if row.switchable {
                    let (suffix, action) = if row.login {
                        ("⟳ Log in again", Action::Login { key: row.switch_key.clone() })
                    } else {
                        ("⮂ switch", Action::Switch { key: row.switch_key.clone() })
                    };
                    items.push(send_item(format!("{header}    {suffix}"), true, action));
                } else {
                    items.push(disabled(header));
                }
```
  (j) `src/main.rs` (lines 37-48): clone `tx` into the tray and pass the original to the engine:
```rust
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let tray = tray::PitStopTray {
        view: tray::TrayView::loading(),
        tx: tx.clone(),
    };
```
and change the run line to:
```rust
    app::Engine::new(handle, tx).run(rx).await;
```
  (k) `src/main.rs`: remove the two `#[allow(dead_code)]` attributes on `mod oauth;` / `mod loopback;` (added in Task 1) — both modules are now reachable from the engine.
- [ ] **Step 4: Run test, verify it passes** \n Run: `cargo test login_eligible_requires_needs_action_and_inactive && cargo build && cargo clippy --all-targets` \n Expected: PASS; build + clippy clean with no dead-code warnings (oauth/loopback are now used via `perform_login`).
- [ ] **Step 5: Commit** \n `git add -A && git commit -m "app: Action::Login/LoginFinished + perform_login guard + row eligibility"`

---

### Task 9: `tray.rs` render helper for the Login label
**Files:** Modify: `src/tray.rs:122-131` / Test: `src/tray.rs` `#[cfg(test)]`
**Interfaces:** Produces: `tray::row_trailing(login: bool) -> &'static str`. Refactors the inline label from Task 8 into a tested pure helper.

- [ ] **Step 1: Write the failing test** — add a `#[cfg(test)]` module to `src/tray.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::row_trailing;

    #[test]
    fn row_trailing_switches_on_login_flag() {
        assert_eq!(row_trailing(false), "⮂ switch");
        assert_eq!(row_trailing(true), "⟳ Log in again");
    }
}
```
- [ ] **Step 2: Run test, verify it fails** \n Run: `cargo test row_trailing_switches_on_login_flag` \n Expected: FAIL — `error[E0432]: unresolved import 'super::row_trailing'`.
- [ ] **Step 3: Minimal implementation** — add the helper near the other builders in `src/tray.rs`:
```rust
/// The trailing action label for a switchable row: Login when the token was
/// rejected, otherwise the plain account switch.
fn row_trailing(login: bool) -> &'static str {
    if login {
        "⟳ Log in again"
    } else {
        "⮂ switch"
    }
}
```
and replace the inline `let (suffix, action) = …` tuple from Task 8 (i) with:
```rust
                if row.switchable {
                    let suffix = row_trailing(row.login);
                    let action = if row.login {
                        Action::Login { key: row.switch_key.clone() }
                    } else {
                        Action::Switch { key: row.switch_key.clone() }
                    };
                    items.push(send_item(format!("{header}    {suffix}"), true, action));
                } else {
                    items.push(disabled(header));
                }
```
- [ ] **Step 4: Run test, verify it passes** \n Run: `cargo test row_trailing_switches_on_login_flag && cargo clippy --all-targets` \n Expected: PASS; clippy clean.
- [ ] **Step 5: Commit** \n `git add -A && git commit -m "tray: row_trailing helper for the Login/switch label"`

---

## Manual E2E verification (documented; not an automated task)
After Task 9, verify the `[verify]` network paths from the spec on the real machine:
1. Let a saved, **inactive** Claude Code account's token expire (or delete its saved profile's `accessToken`/`refreshToken` so the next fetch 401s) → the row enters `needs_action` and renders **⟳ Log in again**.
2. Click it → browser opens `claude.ai/oauth/authorize`; approve. Confirm the loopback callback heals the row (pill → normal chip) on the next refresh, and that `~/.config/pitstop/accounts/claude-<email>.json` changed while `~/.claude/.credentials.json` and `~/.claude.json` did **not** (check mtimes). This validates the `platform.claude.com` → `console.anthropic.com` token-host fallback and the `/api/oauth/profile` identity endpoint.
3. Repeat for a Codex row (ports 1455/1457, form-urlencoded exchange, id_token identity), confirming only `~/.config/pitstop/accounts/codex-<email>.json` is written.
4. Sign in as the **wrong** Google/ChatGPT account → confirm the identity-mismatch notification and that nothing is written.
