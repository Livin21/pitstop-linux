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

/// Create-or-replace a go-keyring item (matched by attributes service+username),
/// storing `value` verbatim (Antigravity's `go-keyring-base64:` string). Label
/// `"<service>/<account>"` matches go-keyring's schema.
#[allow(dead_code)] // wired up in Task 8 (switch write-back)
pub async fn set(service: &str, account: &str, value: &str) -> Result<()> {
    let ss = SecretService::connect(EncryptionType::Dh).await?;
    let collection = ss.get_default_collection().await?;
    collection.ensure_unlocked().await?;
    collection
        .create_item(
            &format!("{service}/{account}"),
            attrs(service, account),
            value.as_bytes(),
            true, // replace an existing item with the same attributes
            "text/plain",
        )
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Run manually against a live GNOME keyring: `cargo test --lib secret_service -- --ignored`.
    #[tokio::test]
    #[ignore = "needs a live GNOME keyring/Secret Service daemon"]
    async fn set_get_round_trip_preserves_prefix() {
        let value = "go-keyring-base64:eyJhIjoxfQ=="; // opaque go-keyring string
        set("pitstop-selftest", "rt", value).await.unwrap();
        let got = get("pitstop-selftest", "rt").await.unwrap();
        assert_eq!(got.as_deref(), Some(value));

        // Clean up: delete the throwaway test item from the keyring.
        let ss = SecretService::connect(EncryptionType::Dh).await.unwrap();
        let found = ss
            .search_items(attrs("pitstop-selftest", "rt"))
            .await
            .unwrap();
        for item in found
            .unlocked
            .into_iter()
            .chain(found.locked.into_iter())
        {
            item.delete().await.unwrap();
        }
    }
}
