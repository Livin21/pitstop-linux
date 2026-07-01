//! go-keyring-compatible GNOME keyring (Secret Service) client. Used only by
//! the Gemini/Antigravity surface, whose live token lives in the keyring rather
//! than a file. Matches zalando/go-keyring's schema so PitStop can read the
//! item Antigravity wrote (attributes service+username, value carrying the
//! `go-keyring-base64:` prefix) and, on switch, write it back in place.

use anyhow::Result;
use secret_service::{EncryptionType, SecretService};
use std::collections::HashMap;

fn attrs<'a>(service: &'a str, account: &'a str) -> HashMap<&'a str, &'a str> {
    HashMap::from([("service", service), ("username", account)])
}

/// The raw secret string for a go-keyring item, or `None` if no item matches.
/// The returned string is opaque (for Antigravity it is `"go-keyring-base64:" + base64(JSON)`).
pub async fn get(service: &str, account: &str) -> Result<Option<String>> {
    let ss = SecretService::connect(EncryptionType::Dh).await?;
    let found = ss.search_items(attrs(service, account)).await?;
    let item = found
        .unlocked
        .into_iter()
        .next()
        .or_else(|| found.locked.into_iter().next());
    let Some(item) = item else {
        return Ok(None);
    };
    item.unlock().await?;
    let secret = item.get_secret().await?;
    Ok(Some(String::from_utf8(secret)?))
}
