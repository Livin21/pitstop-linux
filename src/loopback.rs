//! One-shot loopback HTTP callback server on 127.0.0.1 for the OAuth flow —
//! the Linux stand-in for macOS's raw-socket LoopbackServer. Captures the first
//! `GET /<path>?code=…&state=…`, replies 200, yields (code, state). Also parses
//! a value the user pastes from a hosted callback page.

use anyhow::{bail, Result};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[derive(Debug)]
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
