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
