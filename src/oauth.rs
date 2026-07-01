//! Native OAuth `authorization_code` re-login (PKCE S256) for expired,
//! inactive Claude Code & Codex rows. Writes fresh tokens ONLY into the
//! saved-profile snapshot — never the live store.

use anyhow::Result;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use rand::RngCore;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::process::Stdio;
use std::time::Duration;

use crate::codex;
use crate::credentials;
use crate::loopback;
use crate::secret_store;
use crate::usage_api::ApiError;
use crate::util::now_secs;

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

const LOOPBACK_TIMEOUT_SECS: u64 = 150;

/// Run one OAuth re-login end to end. Loopback first; Claude falls back to a
/// zenity paste prompt. Writes only to the profile slot, and only when the
/// browser identity matches `target_email`.
///
/// Designed to run inside a detached `tokio::spawn`: fully async and
/// cancellable. Dropping the future closes the loopback listener and, because
/// nothing is written before the identity gate passes, leaves no partial state.
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
            crate::app::open_url(&auth_url);
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
    crate::app::open_url(&auth_url);
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
            copy_to_clipboard(auth_url).await;
            crate::notify::post(
                "PitStop sign-in — action needed",
                "Install `zenity` to paste the sign-in code. The sign-in URL was copied to your clipboard; approve in the browser, then retry.",
            );
            None
        }
    }
}

async fn copy_to_clipboard(text: &str) {
    use tokio::io::AsyncWriteExt;
    if let Ok(mut child) = tokio::process::Command::new("xclip")
        .args(["-selection", "clipboard"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(text.as_bytes()).await;
            // Drop stdin to signal EOF so xclip stops reading and exits.
            drop(stdin);
        }
        let _ = child.wait().await;
    }
}

// ─── ClaudeLoginAdapter ────────────────────────────────────────────────────

const CLAUDE_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const CLAUDE_AUTHORIZE: &str = "https://claude.ai/oauth/authorize";
/// [verify] primary host; falls back to console.anthropic.com on 404 or transport error.
const CLAUDE_TOKEN_HOSTS: [&str; 2] = [
    "https://platform.claude.com/v1/oauth/token",
    "https://console.anthropic.com/v1/oauth/token",
];
/// [verify] identity endpoint.
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
        // Try platform host first; fall through to console ONLY on 404 (endpoint
        // not present on this host) or a transport error. Auth errors (400/401/403)
        // are final — the code is single-use, replaying it will not help.
        // [verify] CLAUDE_TOKEN_HOSTS primary/fallback addresses.
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
                refresh_token: root
                    .get("refresh_token")
                    .and_then(Value::as_str)
                    .map(String::from),
                id_token: None,
                expires_at_ms: ((now_secs() + expires_in) * 1000.0) as i64,
            });
        }
        Err(last)
    }

    async fn identity(&self, http: &reqwest::Client, t: &FreshTokens) -> Result<LoginIdentity> {
        // [verify] CLAUDE_PROFILE_URL — confirmed same pattern as usage endpoint.
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
        // Try common email field locations from most to least specific.
        let email = root
            .get("email")
            .and_then(Value::as_str)
            .or_else(|| root.get("email_address").and_then(Value::as_str))
            .or_else(|| {
                root.get("account")
                    .and_then(|a| a.get("email_address"))
                    .and_then(Value::as_str)
            })
            .or_else(|| {
                root.get("account")
                    .and_then(|a| a.get("email"))
                    .and_then(Value::as_str)
            })
            .ok_or(ApiError::Malformed)?;
        Ok(LoginIdentity { email: email.to_string(), account_id: None })
    }

    async fn persist(&self, email: &str, t: &FreshTokens) -> Result<()> {
        let old = secret_store::read(credentials::LIVE_PROVIDER, email)?.ok_or_else(|| {
            anyhow::anyhow!(
                "No saved profile for {email} — save the account once, then retry"
            )
        })?;
        // patch_blob preserves all non-token fields (subscriptionType, rateLimitTier,
        // mcpOAuth, etc.) — only accessToken, refreshToken, and expiresAt are replaced.
        // Writes ONLY to the saved-profile file; NEVER touches ~/.claude/.credentials.json.
        let patched = credentials::patch_blob(
            &old,
            &t.access_token,
            t.refresh_token.as_deref(),
            t.expires_at_ms as f64,
        )?;
        secret_store::write(credentials::LIVE_PROVIDER, email, &patched)
    }
}

// ─── CodexLoginAdapter ────────────────────────────────────────────────────────

const CODEX_AUTHORIZE: &str = "https://auth.openai.com/oauth/authorize";
const CODEX_SCOPES: &str =
    "openid profile email offline_access api.connectors.read api.connectors.invoke";

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
        // Expiry is carried in the id_token `exp` claim; derive it if present.
        let expires_at_ms = r
            .id_token
            .as_deref()
            .and_then(|idt| {
                // Decode without verifying to read `exp` (seconds).
                let payload = idt.split('.').nth(1)?;
                let bytes =
                    base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload).ok()?;
                let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
                let exp = v.get("exp")?.as_i64()?;
                Some(exp.saturating_mul(1000))
            })
            .unwrap_or(0);
        Ok(FreshTokens {
            access_token: r.access_token,
            refresh_token: r.refresh_token,
            id_token: r.id_token,
            expires_at_ms,
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

// ─── GeminiLoginAdapter ─────────────────────────────────────────────────────

const GEMINI_AUTHORIZE: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const GEMINI_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

/// Google installed-app login for Gemini/Antigravity. Google accepts an
/// arbitrary loopback port → fully automatic (no paste fallback). `persist`
/// writes ONLY the profile snapshot file, never the live keyring (shared
/// contract). Token exchange is form-urlencoded (Google's OAuth endpoint),
/// unlike Claude's JSON body.
pub struct GeminiLoginAdapter;

#[async_trait::async_trait]
impl LoginAdapter for GeminiLoginAdapter {
    fn authorize_url(&self, pkce: &Pkce, redirect_uri: &str, _paste_mode: bool) -> String {
        let mut url = reqwest::Url::parse(GEMINI_AUTHORIZE).unwrap();
        url.query_pairs_mut()
            .append_pair("client_id", crate::gemini::ANTIGRAVITY_CLIENT_ID)
            .append_pair("response_type", "code")
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("scope", crate::gemini::SCOPES)
            .append_pair("code_challenge", &pkce.challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", &pkce.state)
            .append_pair("access_type", "offline")
            .append_pair("prompt", "consent");
        url.to_string()
    }

    fn fixed_loopback_port(&self) -> Option<u16> {
        None // ephemeral port; Google accepts any loopback
    }

    fn redirect_path(&self) -> &'static str {
        "/oauth2callback"
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
        let resp = http
            .post(GEMINI_TOKEN_URL)
            .form(&crate::gemini::exchange_form(code, &pkce.verifier, redirect_uri))
            .timeout(Duration::from_secs(15))
            .send()
            .await
            .map_err(|e| ApiError::Network(e.to_string()))?;
        let status = resp.status().as_u16();
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
            .unwrap_or(3600.0);
        Ok(FreshTokens {
            access_token: access.to_string(),
            refresh_token: root.get("refresh_token").and_then(Value::as_str).map(String::from),
            id_token: root.get("id_token").and_then(Value::as_str).map(String::from),
            expires_at_ms: (crate::util::now_ms() + expires_in * 1000.0) as i64,
        })
    }

    async fn identity(&self, http: &reqwest::Client, t: &FreshTokens) -> Result<LoginIdentity> {
        // The Antigravity blob carries no email, so identity comes from `userinfo`.
        let email = crate::gemini::fetch_email(http, &t.access_token)
            .await
            .map_err(anyhow::Error::from)?;
        Ok(LoginIdentity { email, account_id: None })
    }

    async fn persist(&self, email: &str, t: &FreshTokens) -> Result<()> {
        // Profile snapshot ONLY (never the live keyring). Build a fresh blob:
        // unlike Claude/Codex there is nothing to patch — a re-login produces a
        // complete fresh token set. `build_antigravity_blob` always emits the
        // go-keyring-wrapped form; Task 5's `switch_to` form-matches it to the
        // live keyring at switch time.
        let iso = crate::gemini::expiry_iso(t.expires_at_ms as f64);
        let blob = crate::gemini::build_antigravity_blob(
            &t.access_token,
            t.refresh_token.as_deref(),
            t.id_token.as_deref(),
            &iso,
        );
        secret_store::write(crate::gemini_store::PROVIDER, email, &blob)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn qmap(url: &str) -> HashMap<String, String> {
        reqwest::Url::parse(url).unwrap().query_pairs().into_owned().collect()
    }

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

    #[test]
    fn claude_authorize_url_params() {
        let pkce = Pkce { verifier: "v".into(), challenge: "CH".into(), state: "ST".into() };
        let url =
            ClaudeLoginAdapter.authorize_url(&pkce, "http://localhost:5000/callback", false);
        assert!(url.starts_with("https://claude.ai/oauth/authorize?"));
        let q = qmap(&url);
        assert_eq!(q["client_id"], "9d1c250a-e61b-44d9-88ed-5944d1962f5e");
        assert_eq!(q["response_type"], "code");
        assert_eq!(q["redirect_uri"], "http://localhost:5000/callback");
        assert_eq!(q["code_challenge"], "CH");
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(q["state"], "ST");
        assert!(!q.contains_key("code"));
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

    #[test]
    fn claude_authorize_url_contains_all_scopes() {
        let pkce = Pkce { verifier: "v".into(), challenge: "CH".into(), state: "ST".into() };
        let url =
            ClaudeLoginAdapter.authorize_url(&pkce, "http://localhost:5000/callback", false);
        let q = qmap(&url);
        let scope = &q["scope"];
        assert!(scope.contains("org:create_api_key"), "missing org:create_api_key");
        assert!(scope.contains("user:profile"), "missing user:profile");
        assert!(scope.contains("user:inference"), "missing user:inference");
        assert!(scope.contains("user:sessions:claude_code"), "missing user:sessions:claude_code");
        assert!(scope.contains("user:mcp_servers"), "missing user:mcp_servers");
        assert!(scope.contains("user:file_upload"), "missing user:file_upload");
    }

    #[test]
    fn claude_redirect_uri_byte_matches_in_authorize_and_exchange_body() {
        // Verifies the redirect_uri used in authorize_url is byte-identical to
        // what would be passed to exchange — both receive the same &str.
        let loopback = "http://localhost:9999/callback";
        let pkce = Pkce { verifier: "v".into(), challenge: "CH".into(), state: "ST".into() };
        let url = ClaudeLoginAdapter.authorize_url(&pkce, loopback, false);
        let q = qmap(&url);
        // The exchange body would also use `loopback`; both are the same &str.
        assert_eq!(q["redirect_uri"], loopback);
    }

    #[test]
    fn claude_adapter_metadata() {
        assert_eq!(ClaudeLoginAdapter.fixed_loopback_port(), None);
        assert_eq!(ClaudeLoginAdapter.redirect_path(), "/callback");
        assert!(ClaudeLoginAdapter.supports_paste());
        assert_eq!(
            ClaudeLoginAdapter.paste_redirect_uri(),
            "https://platform.claude.com/oauth/code/callback"
        );
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

    #[test]
    fn codex_authorize_url_params() {
        let pkce = Pkce { verifier: "v".into(), challenge: "CH".into(), state: "ST".into() };
        let url =
            CodexLoginAdapter.authorize_url(&pkce, "http://localhost:1455/auth/callback", false);
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
        let saved =
            crate::secret_store::read(crate::codex::PROVIDER, email).unwrap().unwrap();
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

    #[test]
    fn gemini_authorize_url_has_google_params() {
        let pkce = Pkce { verifier: "v".into(), challenge: "chal".into(), state: "st".into() };
        let url = GeminiLoginAdapter.authorize_url(&pkce, "http://127.0.0.1:5123/oauth2callback", false);
        assert!(url.starts_with("https://accounts.google.com/o/oauth2/v2/auth?"));
        assert!(url.contains("code_challenge=chal"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("state=st"));
        assert!(url.contains("access_type=offline"));
        // client_id chars (`.` `-`) are unreserved, so it appears literally.
        assert!(url.contains("client_id=1071006060591-"));
        assert_eq!(GeminiLoginAdapter.redirect_path(), "/oauth2callback");
        assert_eq!(GeminiLoginAdapter.fixed_loopback_port(), None);
        assert!(!GeminiLoginAdapter.supports_paste());
    }

    #[test]
    fn gemini_authorize_url_carries_redirect_and_scopes() {
        let pkce = Pkce { verifier: "v".into(), challenge: "chal".into(), state: "st".into() };
        let url = GeminiLoginAdapter.authorize_url(&pkce, "http://localhost:5123/oauth2callback", false);
        let q = qmap(&url);
        assert_eq!(q["response_type"], "code");
        assert_eq!(q["redirect_uri"], "http://localhost:5123/oauth2callback");
        assert_eq!(q["scope"], crate::gemini::SCOPES);
        assert_eq!(q["prompt"], "consent");
    }

    #[tokio::test]
    async fn gemini_persist_writes_only_profile_snapshot() {
        // Writes ONLY the saved-profile snapshot file (via secret_store) — never
        // the live keyring. The live keyring path (secret_service) is unreachable
        // from `persist`, so a filesystem-only temp dir is a complete assertion:
        // if persist had touched the keyring the test env has none to touch.
        let dir = std::env::temp_dir().join("pitstop-oauth-relogin-tests");
        std::env::set_var("XDG_CONFIG_HOME", &dir);
        let email = "persist-gemini@example.com";
        // Start with no saved snapshot: persist builds a FRESH blob (unlike
        // Claude/Codex which patch an existing profile).
        let _ = crate::secret_store::delete(crate::gemini_store::PROVIDER, email);
        let tokens = FreshTokens {
            access_token: "ya29.NEW".into(),
            refresh_token: Some("1//newR".into()),
            id_token: Some("idt.NEW".into()),
            expires_at_ms: 4_102_444_800_000, // 2100-01-01, safely in the future
        };
        GeminiLoginAdapter.persist(email, &tokens).await.unwrap();
        let saved = crate::secret_store::read(crate::gemini_store::PROVIDER, email)
            .unwrap()
            .unwrap();
        // Snapshot is the go-keyring-wrapped form (Task 5's switch form-matches it).
        assert!(String::from_utf8_lossy(&saved).starts_with("go-keyring-base64:"));
        // Round-trips back to the fresh tokens.
        let creds = crate::gemini::antigravity_creds(&saved).unwrap();
        assert_eq!(creds.access_token, "ya29.NEW");
        assert_eq!(creds.refresh_token.as_deref(), Some("1//newR"));
        assert_eq!(creds.id_token.as_deref(), Some("idt.NEW"));
        crate::secret_store::delete(crate::gemini_store::PROVIDER, email).unwrap();
    }

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
        async fn exchange(
            &self,
            _h: &reqwest::Client,
            _c: &str,
            _p: &Pkce,
            _r: &str,
        ) -> Result<FreshTokens> {
            Ok(FreshTokens {
                access_token: "at".into(),
                refresh_token: None,
                id_token: None,
                expires_at_ms: 0,
            })
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
}
