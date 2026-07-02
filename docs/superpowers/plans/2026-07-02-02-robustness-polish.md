# Robustness Fixes + Status Polish Batch Implementation Plan
> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (- [ ]) syntax for tracking.

**Goal:** Port the macOS v0.4.0/v0.4.1 robustness + status-polish batch (spec §2a–2l plus version/docs) to PitStop-Linux: single-instance lock, credential-preserving switches/re-logins, symlink-safe atomic writes, deny-as-cancel loopback, one-cycle needs-action heal, orphan-state pruning, countdown/row-status/color polish, and the 0.4.1 version bump.
**Architecture:** Small, surgical fixes across the existing single-tokio-task engine and its stores. Each fix is factored so its decision logic lives in a pure, unit-testable free function (mirroring the existing `record_window_sample` / `clear_after_login` / `pick_auto_switch` pattern); the impure wiring stays a thin caller. No new threads or locks in the render path; the only OS lock is a launch-time `flock` held for the process lifetime.
**Tech Stack:** libc (new — `flock`/`open`), serde_json, chrono, anyhow, reqwest (`Url` query parsing), tokio.
**Depends on:** **Plan 1 (Fable scoped limits)** — Task 10's "Extra" guard is written against the POST-Plan-1 `build_row` shape (Plan 1 removes the Opus/Sonnet extras arms; only the `Extra` arm remains). Execute Plan 1 first. Everything else in this plan is independent of Plan 1.

## Global Constraints
- Rust 2021; single tokio task (Engine::run select loop); ksni tray; no new threads/locks in the render path.
- Secrets only in 0600 files or the GNOME keyring; never logged; secret-bearing structs must not derive Debug.
- reqwest async; serde/serde_json; chrono; anyhow. Reuse existing ApiError.
- Each task ends green: cargo build clean, cargo test passes, cargo clippy --all-targets -- -D warnings clean, one commit.
- Re-login writes only saved-profile snapshots; live stores are mutated only by an explicit switch (documented exception: same-account active-token refresh).
---

### Task 1: Single-instance flock at launch (§2a)
**Files:** Modify: `Cargo.toml` (`[dependencies]`, after `async-trait = "0.1"`), `src/main.rs` (add `use std::path::Path;`, two fns, a call in `run_tray`, a `#[cfg(test)] mod tests`).
**Interfaces:** Produces `acquire_lock_at(&Path) -> bool` (pure/testable), `single_instance_ok() -> bool`. Consumes `util::config_dir()`.

Context: a direct-binary launch alongside an installed copy gives two trays with independent auto-switch clocks fighting over the live credential files (reproduced live). Mac `bbdd09d`: `open(dir/pitstop.lock, O_CREAT|O_RDWR, 0o600)` then `flock(fd, LOCK_EX|LOCK_NB)`; exit(0) on contention; leak the fd. One-shot flags (`--check`, `--gemini-spike`, `--export-icon`) already `return` in `main()` before `run_tray()`, so putting the lock in `run_tray()` skips them for free. `libc` is present transitively (`Cargo.lock` `libc 0.2.186`) but is not a direct dep — add it.

- [ ] **Step 1: Write the failing test** — add to the bottom of `src/main.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_instance_lock_is_exclusive() {
        let path = std::env::temp_dir()
            .join(format!("pitstop-lock-test-{}.lock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        assert!(acquire_lock_at(&path), "first acquire should hold the lock");
        assert!(
            !acquire_lock_at(&path),
            "a second acquire while the first fd is held must fail"
        );
        let _ = std::fs::remove_file(&path);
    }
}
```
- [ ] **Step 2: Run test, verify it fails** \n Run: `cargo test --bin pitstop single_instance_lock_is_exclusive` \n Expected: FAIL — `error[E0425]: cannot find function 'acquire_lock_at'` (and an `unresolved import`/`use of undeclared crate 'libc'` once referenced).
- [ ] **Step 3: Minimal implementation** — add to `Cargo.toml` under `[dependencies]` (after the `async-trait` line): `libc = "0.2"`. Add `use std::path::Path;` near the top of `src/main.rs` (below `use anyhow::Result;`), and add these two functions above `run_tray` (or anywhere at module scope):
```rust
/// Take an exclusive advisory `flock` on `path` (created 0600). Returns `true`
/// when we now hold it — or when the lock file can't even be created (fail open:
/// never block startup on a lock we couldn't take). Returns `false` only when
/// another live process already holds it. The fd is deliberately leaked so the
/// lock is held for the whole process; the kernel releases it on exit, so a
/// crash can't wedge future launches.
fn acquire_lock_at(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let mut c: Vec<u8> = path.as_os_str().as_bytes().to_vec();
    c.push(0);
    unsafe {
        let fd = libc::open(
            c.as_ptr() as *const libc::c_char,
            libc::O_CREAT | libc::O_RDWR,
            0o600,
        );
        if fd < 0 {
            return true; // couldn't create the lock file — don't block startup
        }
        if libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) != 0 {
            libc::close(fd);
            return false;
        }
        // Leak `fd`: closing it would release the lock. Held until the process exits.
    }
    true
}

/// Refuse to run a second tray beside a live one — two instances fight over the
/// live credential files with independent auto-switch clocks. One-shot flags
/// return before this in `main()`, so they never take the lock.
fn single_instance_ok() -> bool {
    let dir = util::config_dir();
    let _ = std::fs::create_dir_all(&dir);
    acquire_lock_at(&dir.join("pitstop.lock"))
}
```
Then add to the top of `run_tray`, immediately after the opening brace (before `use ksni::TrayMethods;`):
```rust
    if !single_instance_ok() {
        println!("another PitStop instance is running");
        return Ok(());
    }
```
- [ ] **Step 4: Run test, verify it passes** \n Run: `cargo test --bin pitstop single_instance_lock_is_exclusive` then `cargo build && cargo clippy --all-targets -- -D warnings`. \n Manual: `cargo run` in one terminal (tray appears), then `cargo run` in a second — the second prints `another PitStop instance is running` and exits 0. \n Expected: test PASS; second instance exits.
- [ ] **Step 5: Commit** \n `git add -A && git commit -m "Exit at launch when another PitStop instance holds the lock"`

---

### Task 2: Gemini re-login PATCHES the saved blob (§2b, risk 5)
**Files:** Modify: `src/gemini.rs` (`patch_antigravity_blob` signature + body + tests), `src/oauth.rs` (`GeminiLoginAdapter::persist` + a test), `src/app.rs` (`fetch_gemini_usage` caller ~line 573).
**Interfaces:** Produces `gemini::patch_antigravity_blob(old, access, refresh: Option<&str>, id_token, expiry_iso)`. Consumes `secret_store::read/write`, `gemini::build_antigravity_blob`.

Context (Mac `0970bb4`): `persist` builds the blob from scratch, so a Google re-consent response omitting `refresh_token` destroys the stored one → the profile is permanently unrefreshable. Fix: patch the existing saved snapshot when present (preserving `refresh_token` when the fresh tokens lack one and preserving the wrapper form), build from scratch only when no snapshot exists. Extend the existing form-preserving `patch_antigravity_blob` (do NOT fork a second patcher) so a `Some(refresh)` updates the field while `None` preserves it. The `app.rs` refresh caller passes `None` (Google's refresh grant never rotates the refresh token — see `gemini::refresh_form`).

- [ ] **Step 1: Write the failing tests** — in `src/gemini.rs` `#[cfg(test)] mod tests`, add:
```rust
    #[test]
    fn patch_antigravity_blob_updates_or_preserves_refresh_token() {
        let built = build_antigravity_blob("acc", Some("OLD-RT"), None, "2026-07-01T20:00:00.000Z");
        // Some(refresh) updates it.
        let updated =
            patch_antigravity_blob(&built, "acc2", Some("NEW-RT"), None, "2026-08-01T00:00:00.000Z").unwrap();
        assert_eq!(antigravity_creds(&updated).unwrap().refresh_token.as_deref(), Some("NEW-RT"));
        // None preserves the stored one (Google omits refresh_token on re-consent).
        let preserved =
            patch_antigravity_blob(&built, "acc3", None, None, "2026-08-01T00:00:00.000Z").unwrap();
        assert_eq!(antigravity_creds(&preserved).unwrap().refresh_token.as_deref(), Some("OLD-RT"));
    }
```
And in `src/oauth.rs` `#[cfg(test)] mod tests`, add:
```rust
    #[tokio::test]
    async fn gemini_persist_patches_existing_snapshot_preserving_refresh() {
        // Same shared fixed dir as the other oauth persist tests (all set the
        // SAME XDG value, so parallel interleaving is harmless; unique email).
        let dir = std::env::temp_dir().join("pitstop-oauth-relogin-tests");
        std::env::set_var("XDG_CONFIG_HOME", &dir);
        let email = "persist-gemini-patch@example.com";
        // Seed a saved snapshot carrying a refresh token.
        let old = crate::gemini::build_antigravity_blob(
            "ya29.OLD", Some("1//OLD-RT"), None, "2026-07-01T20:00:00.000Z",
        );
        crate::secret_store::write(crate::gemini_store::PROVIDER, email, &old).unwrap();
        // Re-login returns NO refresh_token (Google omitted it on re-consent).
        let tokens = FreshTokens {
            access_token: "ya29.NEW".into(),
            refresh_token: None,
            id_token: None,
            expires_at_ms: 4_102_444_800_000,
        };
        GeminiLoginAdapter.persist(email, &tokens).await.unwrap();
        let saved = crate::secret_store::read(crate::gemini_store::PROVIDER, email)
            .unwrap()
            .unwrap();
        let creds = crate::gemini::antigravity_creds(&saved).unwrap();
        assert_eq!(creds.access_token, "ya29.NEW"); // updated
        assert_eq!(creds.refresh_token.as_deref(), Some("1//OLD-RT")); // preserved
        crate::secret_store::delete(crate::gemini_store::PROVIDER, email).unwrap();
    }
```
- [ ] **Step 2: Run tests, verify they fail** \n Run: `cargo test patch_antigravity_blob_updates_or_preserves_refresh_token gemini_persist_patches_existing_snapshot_preserving_refresh` \n Expected: FAIL — compile error: `patch_antigravity_blob` takes 4 arguments but the new test passes 5 (`E0061`), and `persist` still rebuilds from scratch so the preserve assertion is unreachable until it compiles.
- [ ] **Step 3: Minimal implementation** — in `src/gemini.rs`, change the signature and body of `patch_antigravity_blob` to accept a `refresh: Option<&str>` between `access` and `id_token`:
```rust
pub fn patch_antigravity_blob(
    old: &[u8],
    access: &str,
    refresh: Option<&str>,
    id_token: Option<&str>,
    expiry_iso: &str,
) -> Option<Vec<u8>> {
    let raw = std::str::from_utf8(old).ok()?;
    let is_wrapped = raw.trim().starts_with(GO_KEYRING_PREFIX);
    let inner = if is_wrapped {
        decode_go_keyring(raw)?
    } else {
        raw.trim().as_bytes().to_vec()
    };
    let mut root: Value = serde_json::from_slice(&inner).ok()?;
    {
        let tok = root.get_mut("token")?.as_object_mut()?;
        tok.insert("access_token".into(), json!(access));
        tok.insert("expiry".into(), json!(expiry_iso));
        if let Some(r) = refresh {
            tok.insert("refresh_token".into(), json!(r));
        }
        if let Some(i) = id_token {
            tok.insert("id_token".into(), json!(i));
        }
    }
    let serialized = serde_json::to_vec(&root).ok()?;
    if is_wrapped {
        Some(encode_go_keyring(&serialized).into_bytes())
    } else {
        Some(serialized)
    }
}
```
Update the two existing gemini tests that call the old 4-arg form to insert `None` for `refresh`: in `build_and_patch_antigravity_blob_preserve_prefix_and_fields`, change `patch_antigravity_blob(&built, "newacc", Some("idt"), "2026-08-01T00:00:00.000Z")` → `patch_antigravity_blob(&built, "newacc", None, Some("idt"), "2026-08-01T00:00:00.000Z")`; in `patch_antigravity_blob_raw_json_preserves_form`, change `patch_antigravity_blob(raw, "new_acc", Some("idt"), "2026-08-01T00:00:00.000Z")` → `patch_antigravity_blob(raw, "new_acc", None, Some("idt"), "2026-08-01T00:00:00.000Z")`.
In `src/app.rs` `fetch_gemini_usage`, update the caller (~line 573) to pass `None` for refresh:
```rust
                if let Some(patched) = gemini::patch_antigravity_blob(
                    &blob,
                    &refreshed.access_token,
                    None,
                    refreshed.id_token.as_deref(),
                    &gemini::expiry_iso(refreshed.expires_at_ms),
                ) {
```
In `src/oauth.rs`, replace `GeminiLoginAdapter::persist`'s body with a patch-when-present / build-when-absent version:
```rust
    async fn persist(&self, email: &str, t: &FreshTokens) -> Result<()> {
        // Patch the saved snapshot when one exists — Google omits refresh_token
        // on re-consent, and rebuilding from scratch would destroy the stored
        // one (and the wrapper form). Build fresh only when there's no snapshot.
        // Profile snapshot ONLY — never the live keyring.
        let iso = crate::gemini::expiry_iso(t.expires_at_ms as f64);
        let build = || {
            crate::gemini::build_antigravity_blob(
                &t.access_token,
                t.refresh_token.as_deref(),
                t.id_token.as_deref(),
                &iso,
            )
        };
        let blob = match secret_store::read(crate::gemini_store::PROVIDER, email)? {
            Some(old) => crate::gemini::patch_antigravity_blob(
                &old,
                &t.access_token,
                t.refresh_token.as_deref(),
                t.id_token.as_deref(),
                &iso,
            )
            .unwrap_or_else(build),
            None => build(),
        };
        secret_store::write(crate::gemini_store::PROVIDER, email, &blob)
    }
```
- [ ] **Step 4: Run tests, verify they pass** \n Run: `cargo test` (whole suite: the signature change touches gemini/oauth/app tests) then `cargo build && cargo clippy --all-targets -- -D warnings`. \n Expected: PASS.
- [ ] **Step 5: Commit** \n `git add -A && git commit -m "Patch saved Gemini blob on re-login instead of rebuilding"`

---

### Task 3: Claude switch rollback on half-failure (§2c)
**Files:** Modify: `src/claude_store.rs` (add `write_then_set_identity` helper, rewrite `switch_to`, add `#[cfg(test)] mod tests`).
**Interfaces:** Produces `write_then_set_identity(previous, blob, write_live, set_identity)` (generic/testable). Consumes `read_live`, `write_live`, `credentials::set_oauth_account`.

Context (Mac `409311e`): `switch_to` writes `~/.claude/.credentials.json` then `credentials::set_oauth_account` (`~/.claude.json`). If the second write fails, the two files disagree and the next `capture_current` files the new tokens under the old profile. Read the previous live blob first; on identity-write failure restore it, then propagate the original error. Factor the sequence behind a generic helper so the rollback is unit-testable without touching real files.

- [ ] **Step 1: Write the failing test** — add a tests module at the bottom of `src/claude_store.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn switch_rollback_restores_previous_live_when_identity_fails() {
        let writes: RefCell<Vec<Vec<u8>>> = RefCell::new(Vec::new());
        let err = write_then_set_identity(
            Some(b"PREVIOUS".to_vec()),
            b"NEW",
            |d| {
                writes.borrow_mut().push(d.to_vec());
                Ok(())
            },
            || Err(anyhow!("identity write failed")),
        )
        .unwrap_err();
        assert!(err.to_string().contains("identity write failed"));
        // New blob written first, then the previous blob restored.
        assert_eq!(writes.borrow().len(), 2);
        assert_eq!(writes.borrow()[0], b"NEW");
        assert_eq!(writes.borrow()[1], b"PREVIOUS");
    }

    #[test]
    fn switch_commits_without_rollback_on_success() {
        let writes: RefCell<Vec<Vec<u8>>> = RefCell::new(Vec::new());
        write_then_set_identity(
            Some(b"PREVIOUS".to_vec()),
            b"NEW",
            |d| {
                writes.borrow_mut().push(d.to_vec());
                Ok(())
            },
            || Ok(()),
        )
        .unwrap();
        assert_eq!(writes.borrow().len(), 1);
        assert_eq!(writes.borrow()[0], b"NEW");
    }
}
```
- [ ] **Step 2: Run test, verify it fails** \n Run: `cargo test --lib switch_rollback_restores_previous_live_when_identity_fails` \n Expected: FAIL — `error[E0425]: cannot find function 'write_then_set_identity'`.
- [ ] **Step 3: Minimal implementation** — add the helper to `src/claude_store.rs` (module scope, e.g. above `impl ProfileStore`):
```rust
/// Write the target's live blob, then apply its identity. If applying the
/// identity fails, restore `previous` so `~/.claude/.credentials.json` and
/// `~/.claude.json` can't disagree (a mismatched pair makes the next
/// `capture_current` file the new tokens under the old profile), then surface
/// the original error. Generic over the write/apply closures so the rollback is
/// testable without the real files.
fn write_then_set_identity<W, S>(
    previous: Option<Vec<u8>>,
    blob: &[u8],
    write_live: W,
    set_identity: S,
) -> Result<()>
where
    W: Fn(&[u8]) -> Result<()>,
    S: FnOnce() -> Result<()>,
{
    write_live(blob)?;
    if let Err(e) = set_identity() {
        if let Some(prev) = previous {
            let _ = write_live(&prev);
        }
        return Err(e);
    }
    Ok(())
}
```
Rewrite the tail of `ProfileStore::switch_to` (the `write_live(&blob)?; credentials::set_oauth_account(&account)` lines) to route through it:
```rust
        write_then_set_identity(
            read_live(),
            &blob,
            write_live,
            || credentials::set_oauth_account(&account),
        )
```
(The module-level `fn write_live(data: &[u8]) -> Result<()>` and `fn read_live() -> Option<Vec<u8>>` already exist; a function item satisfies the `Fn(&[u8]) -> Result<()>` bound.)
- [ ] **Step 4: Run test, verify it passes** \n Run: `cargo test --lib -p pitstop claude_store` then `cargo build && cargo clippy --all-targets -- -D warnings`. \n Expected: PASS.
- [ ] **Step 5: Commit** \n `git add -A && git commit -m "Roll back live credential file when a Claude switch half-fails"`

---

### Task 4: Preserve API-key-only Codex auth across switches (§2d, risk 2)
**Files:** Modify: `src/codex.rs` (add `preserving_api_key`, add tests), `src/codex_store.rs` (`switch_to`).
**Interfaces:** Produces `codex::preserving_api_key(live: Option<&[u8]>, blob: &[u8]) -> Vec<u8>`. Consumes `codex::live_blob`, `secret_store::read`.

Context (Mac `056a519`): `codex::credentials()` returns `None` for an API-key-only `auth.json` (no `tokens.access_token`), so `capture_current` can't snapshot it and a switch would clobber it. Replicate `preservingAPIKey` EXACTLY: only merge when the LIVE file has a non-empty `OPENAI_API_KEY` AND the saved blob's `OPENAI_API_KEY` is absent/empty (a non-empty saved key WINS); output compact key-sorted JSON (serde_json sorts object keys by default, matching `normalized_blob`).

- [ ] **Step 1: Write the failing tests** — in `src/codex.rs` `#[cfg(test)] mod tests`, add:
```rust
    #[test]
    fn preserving_api_key_merges_when_saved_lacks_key() {
        let live = br#"{"OPENAI_API_KEY":"sk-live"}"#;
        let saved = br#"{"tokens":{"access_token":"AT","account_id":"acc","id_token":"x.y.z"}}"#;
        let root: Value = serde_json::from_slice(&preserving_api_key(Some(live), saved)).unwrap();
        assert_eq!(root["OPENAI_API_KEY"], "sk-live");
        assert!(root.get("tokens").is_some());
    }

    #[test]
    fn preserving_api_key_saved_key_wins() {
        let live = br#"{"OPENAI_API_KEY":"sk-live"}"#;
        let saved = br#"{"OPENAI_API_KEY":"sk-saved"}"#;
        let root: Value = serde_json::from_slice(&preserving_api_key(Some(live), saved)).unwrap();
        assert_eq!(root["OPENAI_API_KEY"], "sk-saved");
    }

    #[test]
    fn preserving_api_key_noop_without_live_key() {
        let live = br#"{"tokens":{"access_token":"A"}}"#;
        let saved = br#"{"tokens":{"access_token":"B"}}"#;
        assert_eq!(preserving_api_key(Some(live), saved), saved.to_vec());
        assert_eq!(preserving_api_key(None, saved), saved.to_vec());
    }
```
- [ ] **Step 2: Run tests, verify they fail** \n Run: `cargo test --lib preserving_api_key` \n Expected: FAIL — `error[E0425]: cannot find function 'preserving_api_key'`.
- [ ] **Step 3: Minimal implementation** — add to `src/codex.rs` (module scope, e.g. after `normalized_blob`):
```rust
/// If the live `auth.json` carries a non-empty `OPENAI_API_KEY` that the saved
/// snapshot lacks, carry it into the blob being written — a switch must not
/// destroy API-key auth that only ever lived in the file (`credentials()`
/// returns `None` for it, so `capture_current` can't snapshot it). A non-empty
/// key already in the saved blob wins. Output is compact, key-sorted JSON.
pub fn preserving_api_key(live: Option<&[u8]>, blob: &[u8]) -> Vec<u8> {
    (|| -> Option<Vec<u8>> {
        let live = live?;
        let live_root: Value = serde_json::from_slice(live).ok()?;
        let api_key = live_root.get("OPENAI_API_KEY").and_then(Value::as_str)?;
        if api_key.is_empty() {
            return None;
        }
        let mut root: Value = serde_json::from_slice(blob).ok()?;
        let obj = root.as_object_mut()?;
        let saved_has_key = obj
            .get("OPENAI_API_KEY")
            .and_then(Value::as_str)
            .map(|s| !s.is_empty())
            .unwrap_or(false);
        if saved_has_key {
            return None;
        }
        obj.insert("OPENAI_API_KEY".into(), json!(api_key));
        serde_json::to_vec(&root).ok()
    })()
    .unwrap_or_else(|| blob.to_vec())
}
```
In `src/codex_store.rs`, change `CodexStore::switch_to`'s final line from `write_live(&blob)` to merge the live key first:
```rust
        write_live(&codex::preserving_api_key(codex::live_blob().as_deref(), &blob))
```
- [ ] **Step 4: Run tests, verify they pass** \n Run: `cargo test --lib preserving_api_key` then `cargo build && cargo clippy --all-targets -- -D warnings`. \n Expected: PASS.
- [ ] **Step 5: Commit** \n `git add -A && git commit -m "Preserve API-key auth in ~/.codex/auth.json across switches"`

---

### Task 5: `write_atomic` preserves symlinks (§2e, risk 3)
**Files:** Modify: `src/util.rs` (`write_atomic` prologue, add `#[cfg(test)] mod tests`).
**Interfaces:** Consumes/Produces: unchanged `write_atomic` signature; behavior now resolves symlinks. **Used by ALL stores** (claude/codex/gemini live files, profiles/settings, secret_store) — run the full suite.

Context (Mac `4449c04`): the tmp+`rename(2)` replaces a symlink node itself with a regular file, silently forking state for dotfiles-managed `~/.claude.json`, `~/.codex/auth.json`, etc. Fix: canonicalize the destination first (resolving to the symlink target) before the tmp+rename. Risk 3: `std::fs::canonicalize` errors on a path that doesn't exist yet — only use it when it succeeds; otherwise write the path as-is.

- [ ] **Step 1: Write the failing test** — add a tests module at the bottom of `src/util.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn write_atomic_preserves_symlink() {
        let dir = std::env::temp_dir().join(format!("pitstop-atomic-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let real = dir.join("real.json");
        let link = dir.join("link.json");
        std::fs::write(&real, b"old").unwrap();
        symlink(&real, &link).unwrap();

        write_atomic(&link, b"new", Some(0o600)).unwrap();

        // The link is STILL a symlink, and its target received the new bytes.
        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read(&real).unwrap(), b"new");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_atomic_writes_plain_file() {
        let dir = std::env::temp_dir().join(format!("pitstop-atomic-plain-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("x.json");
        write_atomic(&path, b"hello", None).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"hello");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
```
- [ ] **Step 2: Run test, verify it fails** \n Run: `cargo test --lib write_atomic_preserves_symlink` \n Expected: FAIL — assertion `is_symlink()` is false (the rename replaced the link with a regular file), and `real.json` still reads `old`.
- [ ] **Step 3: Minimal implementation** — in `src/util.rs`, insert at the very top of `write_atomic` (before `let dir = path.parent()...`):
```rust
    // Resolve symlinks so the write lands on the real target: dotfile managers
    // symlink credential files, and the tmp+rename below would otherwise replace
    // the symlink node itself with a regular file, forking state. `canonicalize`
    // errors on a path that doesn't exist yet — fall back to the path as-is then.
    let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let path = resolved.as_path();
```
- [ ] **Step 4: Run test, verify it passes** \n Run: `cargo test` (WHOLE suite — this changes behavior for every store) then `cargo build && cargo clippy --all-targets -- -D warnings`. \n Expected: PASS (all 100 existing tests + the 2 new ones).
- [ ] **Step 5: Commit** \n `git add -A && git commit -m "Preserve dotfile symlinks when writing live credential files"`

---

### Task 6: Loopback deny = cancel (§2f)
**Files:** Modify: `src/loopback.rs` (add `Denied`, `Outcome`, `classify`, `query_has_error`, use in `wait`, add tests), `src/oauth.rs` (`run_login` loopback-error arm).
**Interfaces:** Produces `loopback::Denied` (error), `loopback::Outcome`, `loopback::classify(&str) -> Outcome`. Consumes `loopback::parse_query`.

Context (Mac `6f0e9f7`, the `error=access_denied` part only): a callback carrying `error=access_denied` (Deny clicked, no code) is currently skipped by the accept loop, so the user waits out the timeout (Claude even falls back to a paste prompt). Parse `error` in the callback query and yield a distinct outcome so `run_login` fails fast with "Sign-in was cancelled" — for both providers, before any paste fallback.

- [ ] **Step 1: Write the failing tests** — in `src/loopback.rs` `#[cfg(test)] mod tests`, add:
```rust
    #[test]
    fn classify_captures_code_and_state() {
        match classify("GET /callback?code=abc&state=xyz HTTP/1.1") {
            Outcome::Captured(c) => {
                assert_eq!(c.code, "abc");
                assert_eq!(c.state, "xyz");
            }
            _ => panic!("expected Captured"),
        }
    }

    #[test]
    fn classify_denies_on_error_param() {
        assert!(matches!(
            classify("GET /callback?error=access_denied&state=xyz HTTP/1.1"),
            Outcome::Denied
        ));
    }

    #[test]
    fn classify_ignores_non_callback() {
        assert!(matches!(classify("GET /favicon.ico HTTP/1.1"), Outcome::NotCallback));
        assert!(matches!(classify("GET / HTTP/1.1"), Outcome::NotCallback));
    }

    #[test]
    fn denied_downcasts_and_reads_as_cancelled() {
        assert_eq!(Denied.to_string(), "Sign-in was cancelled");
        let e = anyhow::Error::new(Denied);
        assert!(e.downcast_ref::<Denied>().is_some());
    }
```
- [ ] **Step 2: Run tests, verify they fail** \n Run: `cargo test --lib classify_denies_on_error_param denied_downcasts_and_reads_as_cancelled` \n Expected: FAIL — `cannot find type 'Outcome'` / `cannot find function 'classify'` / `cannot find value 'Denied'`.
- [ ] **Step 3: Minimal implementation** — in `src/loopback.rs`, add after the `Callback`/`parse_query` items:
```rust
/// A user-denied sign-in: the callback carried `?error=…` (the consent page's
/// Deny button) instead of a code. Distinguished from a timeout so the login
/// coordinator can fail fast rather than waiting out the deadline or falling
/// back to paste.
#[derive(Debug)]
pub struct Denied;
impl std::fmt::Display for Denied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Sign-in was cancelled")
    }
}
impl std::error::Error for Denied {}

/// What one HTTP request to the loopback means to the wait loop.
pub enum Outcome {
    Captured(Callback),
    Denied,
    NotCallback,
}

/// Classify an HTTP request line (`GET /path?query HTTP/1.1`): a parseable
/// `code`+`state` is the callback; a query carrying `error` is a denial;
/// anything else (favicon, browser preconnect probes) keeps the loop waiting.
pub fn classify(request_line: &str) -> Outcome {
    let Some(query) = request_line
        .split_whitespace()
        .nth(1)
        .and_then(|p| p.split_once('?'))
        .map(|(_, q)| q)
    else {
        return Outcome::NotCallback;
    };
    if let Some(c) = parse_query(query) {
        return Outcome::Captured(c);
    }
    if query_has_error(query) {
        return Outcome::Denied;
    }
    Outcome::NotCallback
}

fn query_has_error(query: &str) -> bool {
    reqwest::Url::parse(&format!("http://127.0.0.1/?{query}"))
        .ok()
        .map(|u| u.query_pairs().any(|(k, _)| k == "error"))
        .unwrap_or(false)
}
```
In `Loopback::wait`, replace the `let cap = first.split_whitespace()…parse_query(q);` line and the trailing `if let Some(c) = cap { return Ok::<Callback, anyhow::Error>(c); }` block with a classify call AFTER the response is written (so the browser tab still gets its 200). The updated accept body reads:
```rust
                let (mut stream, _) = self.listener.accept().await?;
                let mut buf = vec![0u8; 8192];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let text = String::from_utf8_lossy(&buf[..n]);
                let first = text.lines().next().unwrap_or("");
                let body = "You can close this tab and return to PitStop.";
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(resp.as_bytes()).await;
                let _ = stream.shutdown().await;
                match classify(first) {
                    Outcome::Captured(c) => return Ok::<Callback, anyhow::Error>(c),
                    Outcome::Denied => return Err(anyhow::Error::new(Denied)),
                    Outcome::NotCallback => {}
                }
```
In `src/oauth.rs` `run_login`, change the loopback `wait` error arm so a denial fails fast regardless of paste support:
```rust
                Err(e) => {
                    // A user-denied sign-in fails fast; it must not fall back to paste.
                    if e.downcast_ref::<loopback::Denied>().is_some() || !adapter.supports_paste() {
                        return Err(e);
                    }
                    // fall through to paste
                }
```
- [ ] **Step 4: Run tests, verify they pass** \n Run: `cargo test --lib loopback` (includes the existing `wait_captures_first_callback` / `wait_times_out_with_no_client`) then `cargo build && cargo clippy --all-targets -- -D warnings`. \n Expected: PASS.
- [ ] **Step 5: Commit** \n `git add -A && git commit -m "Treat an OAuth deny redirect as a cancel, not a timeout"`

---

### Task 7: External re-login heals `needs_action` in one refresh (§2g)
**Files:** Modify: `src/claude_store.rs` (`capture_current` return + `capture_changed` helper + test), `src/codex_store.rs` (same), `src/app.rs` (`fetch_pass`, `refresh_codex`, `Save` action, add `credentials_renewed`).
**Interfaces:** Produces `ProfileStore::capture_current() -> Result<(Option<String>, bool)>`, `CodexStore::capture_current() -> Result<(Option<String>, bool)>`, `capture_changed(...)` (pure/testable), `Engine::credentials_renewed(&str)`.

Context (Mac `7576d6b`): the 1-hour unauthorized gate claims "a re-login noticed next pass clears this", but nothing did — after an external `claude`/`codex login` the row keeps showing needs-action with stale data for up to an hour. Stores now report whether `capture_current` actually stored new credentials; when it did, the engine clears that key's `needs_action`/backoff/failure_count so the fetch happens THIS cycle. (Gemini: N/A on Linux — its snapshot path differs and Google tokens rarely die; not ported.)

- [ ] **Step 1: Write the failing tests** — add a tests module to `src/claude_store.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_changed_truth_table() {
        assert!(capture_changed(false, false, false)); // no profile yet → changed
        assert!(capture_changed(true, false, true));   // blob differs → changed
        assert!(capture_changed(true, true, false));   // identity differs → changed
        assert!(!capture_changed(true, true, true));   // all match → unchanged
    }
}
```
And to `src/codex_store.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_changed_truth_table() {
        assert!(capture_changed(false, false)); // no profile yet → changed
        assert!(capture_changed(true, false));  // blob differs → changed
        assert!(!capture_changed(true, true));  // identical → unchanged
    }
}
```
- [ ] **Step 2: Run tests, verify they fail** \n Run: `cargo test --lib capture_changed_truth_table` \n Expected: FAIL — `error[E0425]: cannot find function 'capture_changed'` in both stores.
- [ ] **Step 3: Minimal implementation** —
In `src/claude_store.rs`, add the helper (module scope):
```rust
/// Whether re-capturing the live account would store new bytes: true unless a
/// saved profile already exists with a byte-identical blob AND matching
/// identity. Pure so the change-detection is unit-testable.
fn capture_changed(has_profile: bool, stored_eq: bool, account_eq: bool) -> bool {
    !(has_profile && stored_eq && account_eq)
}
```
Change `ProfileStore::capture_current` to return `Result<(Option<String>, bool)>`: the three early `return Ok(None)`s become `return Ok((None, false))`; replace the existing "unchanged" short-circuit block with change-detection that avoids a borrow conflict, and return `(Some(email), true)` at the end:
```rust
    pub fn capture_current(&mut self) -> Result<(Option<String>, bool)> {
        let Some(blob) = read_live() else {
            return Ok((None, false));
        };
        let Some(account) = credentials::oauth_account() else {
            return Ok((None, false));
        };
        let Some(email) = account
            .get("emailAddress")
            .and_then(Value::as_str)
            .map(String::from)
        else {
            return Ok((None, false));
        };

        let mut account_eq = false;
        let has_profile = if let Some(existing) = self.profiles.iter().find(|p| p.email == email) {
            account_eq = existing.oauth_account == account;
            true
        } else {
            false
        };
        let mut stored_eq = false;
        if has_profile {
            if let Ok(Some(stored)) = secret_store::read(PROVIDER, &email) {
                stored_eq = stored == blob;
            }
        }
        if !capture_changed(has_profile, stored_eq, account_eq) {
            return Ok((Some(email), false));
        }

        let creds = credentials::parse_blob(&blob)?;
        secret_store::write(PROVIDER, &email, &blob)?;
        self.profiles.retain(|p| p.email != email);
        self.profiles.push(Profile {
            email: email.clone(),
            saved_at: now_secs(),
            subscription_type: creds.subscription_type,
            rate_limit_tier: creds.rate_limit_tier,
            oauth_account: account,
        });
        self.profiles.sort_by(|a, b| a.email.cmp(&b.email));
        self.save()?;
        Ok((Some(email), true))
    }
```
In `src/codex_store.rs`, add the helper and rework `capture_current` the same way:
```rust
/// Whether re-capturing the live account would store new bytes: true unless a
/// saved profile already exists with a byte-identical blob. Pure for testing.
fn capture_changed(has_profile: bool, stored_eq: bool) -> bool {
    !(has_profile && stored_eq)
}
```
```rust
    pub fn capture_current(&mut self) -> Result<(Option<String>, bool)> {
        let Some(live) = codex::live_blob() else {
            return Ok((None, false));
        };
        let Some(creds) = codex::credentials(&live) else {
            return Ok((None, false));
        };
        let email = creds.email.clone();
        let blob = codex::normalized_blob(&live);

        let has_profile = self.profiles.iter().any(|p| p.email == email);
        let mut stored_eq = false;
        if has_profile {
            if let Ok(Some(stored)) = secret_store::read(codex::PROVIDER, &email) {
                stored_eq = stored == blob;
            }
        }
        if !capture_changed(has_profile, stored_eq) {
            return Ok((Some(email), false));
        }

        secret_store::write(codex::PROVIDER, &email, &blob)?;
        self.profiles.retain(|p| p.email != email);
        self.profiles.push(CodexProfile {
            email: email.clone(),
            saved_at: now_secs(),
            plan_label: creds.plan_label,
        });
        self.profiles.sort_by(|a, b| a.email.cmp(&b.email));
        self.save()?;
        Ok((Some(email), true))
    }
```
In `src/app.rs`, add the engine method (near `clear_fetch_error`):
```rust
    /// The stored credentials for `key` were externally replaced (an external
    /// `claude`/`codex` re-login, or the provider's own refresh). If the row was
    /// gated needs-action, the new credentials are the fix — clear the gate so
    /// this cycle fetches instead of waiting out the hour. Rate-limit backoffs
    /// (not in `needs_action`) are left alone.
    fn credentials_renewed(&mut self, key: &str) {
        if self.needs_action.contains(key) {
            self.clear_fetch_error(key);
        }
    }
```
In `fetch_pass`, replace `if let Err(e) = self.store.capture_current() { self.last_top_level_error = Some(e.to_string()); }` with:
```rust
        match self.store.capture_current() {
            Ok((profile, changed)) => {
                if changed {
                    if let Some(email) = profile {
                        self.credentials_renewed(&email);
                    }
                }
            }
            Err(e) => self.last_top_level_error = Some(e.to_string()),
        }
```
In `refresh_codex`, replace `if let Err(e) = self.codex_store.capture_current() { self.last_top_level_error = Some(e.to_string()); }` with:
```rust
        match self.codex_store.capture_current() {
            Ok((profile, changed)) => {
                if changed {
                    if let Some(email) = profile {
                        self.credentials_renewed(&format!("codex:{email}"));
                    }
                }
            }
            Err(e) => self.last_top_level_error = Some(e.to_string()),
        }
```
In the `Action::Save` arm, update the match to destructure the tuple:
```rust
                match self.store.capture_current() {
                    Ok((Some(email), _)) => notify::post(
                        &format!("Saved {email}"),
                        "This account can now be switched to from PitStop.",
                    ),
                    Ok((None, _)) => notify::post(
                        "Nothing to save",
                        "No Claude Code login found. Run `claude` and log in first.",
                    ),
                    Err(e) => notify::post("Couldn't save account", &e.to_string()),
                }
```
(The `main.rs` `check()` call sites use `if let Err(e) = …capture_current()` and remain valid; `switch_to`'s `let _ = self.capture_current()?;` also remains valid.)
- [ ] **Step 4: Run tests, verify they pass** \n Run: `cargo test` (the return-type change touches `app.rs` + both stores) then `cargo build && cargo clippy --all-targets -- -D warnings`. \n Expected: PASS.
- [ ] **Step 5: Commit** \n `git add -A && git commit -m "Clear needs-action backoff when a re-login lands new credentials"`

---

### Task 8: Prune orphaned per-account state on removal (§2h)
**Files:** Modify: `src/app.rs` (add `prune_account_state` free fn, use it in `Action::Remove`, add test).
**Interfaces:** Produces `prune_account_state(...)` (pure/testable). Consumes the engine's per-account maps.

Context (Mac `e808d24`): `Action::Remove` clears `fetch_error`, `next_fetch_allowed`, `failure_count` but leaks `needs_action`, `notified_bucket`, and `usage_history` (`"{key}#…"`) — so a ghost account keeps driving the most-urgent menu-bar reading and projections. Clear all three too. Factor the clearing behind a free function so it's unit-testable.

- [ ] **Step 1: Write the failing test** — in `src/app.rs` `#[cfg(test)] mod tests`, add:
```rust
    #[test]
    fn prune_account_state_clears_all_keyed_entries() {
        let mut fe: HashMap<String, String> = HashMap::new();
        let mut nfa: HashMap<String, Instant> = HashMap::new();
        let mut fc: HashMap<String, u32> = HashMap::new();
        let mut na: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut nb: HashMap<String, u8> = HashMap::new();
        let mut uh: History = HashMap::new();
        let now = Instant::now();
        let key = "codex:me@x.com";
        fe.insert(key.into(), "err".into());
        nfa.insert(key.into(), now);
        fc.insert(key.into(), 3);
        na.insert(key.into());
        nb.insert(key.into(), 2);
        uh.insert(format!("{key}#5h"), vec![(now, 10.0)]);
        // Unrelated account state must survive.
        uh.insert("other@x#7d".into(), vec![(now, 5.0)]);
        fe.insert("other@x".into(), "keep".into());

        prune_account_state(&mut fe, &mut nfa, &mut fc, &mut na, &mut nb, &mut uh, key);

        assert!(!fe.contains_key(key));
        assert!(!nfa.contains_key(key));
        assert!(!fc.contains_key(key));
        assert!(!na.contains(key));
        assert!(!nb.contains_key(key));
        assert!(!uh.contains_key(&format!("{key}#5h")));
        assert!(uh.contains_key("other@x#7d"));
        assert!(fe.contains_key("other@x"));
    }
```
- [ ] **Step 2: Run test, verify it fails** \n Run: `cargo test --lib prune_account_state_clears_all_keyed_entries` \n Expected: FAIL — `error[E0425]: cannot find function 'prune_account_state'`.
- [ ] **Step 3: Minimal implementation** — add the free function to `src/app.rs` (module scope, near `record_window_sample`):
```rust
/// Drop every per-account state entry keyed on `key` when an account is removed
/// (its usage is already dropped by the caller from the provider-specific map).
/// Without this, `needs_action`, `notified_bucket`, and `usage_history`
/// (`"{key}#…"`) entries linger and keep driving the most-urgent reading and
/// projections for a ghost account. Free function so it's unit-testable.
#[allow(clippy::too_many_arguments)]
fn prune_account_state(
    fetch_error: &mut HashMap<String, String>,
    next_fetch_allowed: &mut HashMap<String, Instant>,
    failure_count: &mut HashMap<String, u32>,
    needs_action: &mut HashSet<String>,
    notified_bucket: &mut HashMap<String, u8>,
    usage_history: &mut HashMap<String, Vec<(Instant, f64)>>,
    key: &str,
) {
    fetch_error.remove(key);
    next_fetch_allowed.remove(key);
    failure_count.remove(key);
    needs_action.remove(key);
    notified_bucket.remove(key);
    let prefix = format!("{key}#");
    usage_history.retain(|k, _| !k.starts_with(&prefix));
}
```
In the `Action::Remove { key }` arm, replace the three trailing clears
(`self.fetch_error.remove(&key); self.next_fetch_allowed.remove(&key); self.failure_count.remove(&key);`) with a single call after the provider-specific `usage` removal:
```rust
                prune_account_state(
                    &mut self.fetch_error,
                    &mut self.next_fetch_allowed,
                    &mut self.failure_count,
                    &mut self.needs_action,
                    &mut self.notified_bucket,
                    &mut self.usage_history,
                    &key,
                );
```
- [ ] **Step 4: Run test, verify it passes** \n Run: `cargo test --lib prune_account_state_clears_all_keyed_entries` then `cargo build && cargo clippy --all-targets -- -D warnings`. \n Expected: PASS.
- [ ] **Step 5: Commit** \n `git add -A && git commit -m "Prune per-account state when an account is removed"`

---

### Task 9: Countdown fixes (§2i)
**Files:** Modify: `src/format.rs` (`relative`, `relative_short`, add tests).
**Interfaces:** Consumes/Produces: unchanged signatures; boundary behavior fixed.

Context (Mac `02a0b63`): `relative_short` renders `0m` for windows resetting within a minute (or just past) — should be `<1m`; `relative` renders `in 0s` for elapsed dates — should be `now`.

- [ ] **Step 1: Write the failing tests** — in `src/format.rs` `#[cfg(test)] mod tests`, add:
```rust
    #[test]
    fn relative_now_and_seconds() {
        assert_eq!(relative(-5.0), "now");
        assert_eq!(relative(0.0), "now");
        assert_eq!(relative(45.0), "in 45s");
        assert_eq!(relative(300.0), "in 5m");
    }

    #[test]
    fn relative_short_sub_minute_and_elapsed() {
        assert_eq!(relative_short(45.0), "<1m");
        assert_eq!(relative_short(-90.0), "<1m");
        assert_eq!(relative_short(60.0), "1m");
        assert_eq!(relative_short(3.0 * 3600.0 + 34.0 * 60.0), "3h 34m");
        assert_eq!(relative_short(5.0 * 86400.0 + 16.0 * 3600.0), "5d 16h");
    }
```
- [ ] **Step 2: Run tests, verify they fail** \n Run: `cargo test --lib relative_now_and_seconds relative_short_sub_minute_and_elapsed` \n Expected: FAIL — `relative(-5.0)` returns `"in 0s"` (not `"now"`) and `relative_short(45.0)` returns `"0m"` (not `"<1m"`).
- [ ] **Step 3: Minimal implementation** — in `src/format.rs`, change the tail of `relative` from `format!("in {total}s")` to a now-guard:
```rust
    } else if m > 0 {
        format!("in {m}m")
    } else if total > 0 {
        format!("in {total}s")
    } else {
        "now".to_string()
    }
```
and the tail of `relative_short` from `format!("{m}m")` to:
```rust
    } else if m > 0 {
        format!("{m}m")
    } else {
        "<1m".to_string()
    }
```
- [ ] **Step 4: Run tests, verify they pass** \n Run: `cargo test --lib format` then `cargo build && cargo clippy --all-targets -- -D warnings`. \n Expected: PASS.
- [ ] **Step 5: Commit** \n `git add -A && git commit -m "Fix sub-minute and elapsed reset countdowns"`

---

### Task 10: Row-status polish + Extra guard + rounded-percent color (§2j, §2k)
**Files:** Modify: `src/app.rs` (`build_row` Extra arm, `row_status`, `dot`, add `error_is_stale` helper + tests).
**Interfaces:** Produces `error_is_stale(needs_action, data_age_secs)` (pure/testable). **Depends on Plan 1** — the Extra guard targets the post-Plan-1 `build_row` where only the `Extra` extras arm remains (Opus/Sonnet arms removed by Plan 1).

Context (Mac `72711a2` + `a3626bc`): (a) drop the dangling "Extra –" when `extra_usage_utilization` is `None`; (b) age-gate transient errors — needs-action / no-data / >600 s-old data stay `⚠︎ … · showing … data` (stamp now via `short_clock`, no seconds); otherwise a fresh transient hiccup renders as a muted info line (error text only); (c) the gated live Codex row shows `Last seen {short_clock}` / `No usage data yet` instead of the token-mechanics sentence (bars already render on Linux); (d) `dot()` thresholds on the ROUNDED percent (89.6 shows "90%" so it must be red).

- [ ] **Step 1: Write the failing tests** — in `src/app.rs` `#[cfg(test)] mod tests`, add:
```rust
    #[test]
    fn error_is_stale_gates_on_age_and_needs_action() {
        assert!(error_is_stale(true, Some(10)));    // needs-action always warns
        assert!(error_is_stale(false, None));       // no cached data → warn
        assert!(error_is_stale(false, Some(700)));  // >600s old → warn
        assert!(!error_is_stale(false, Some(300))); // fresh transient → muted
        assert!(!error_is_stale(false, Some(600))); // exactly 600 (not >600) → muted
    }

    #[test]
    fn dot_thresholds_on_rounded_percent() {
        assert_eq!(dot(Some(89.6)), "🔴"); // rounds to 90
        assert_eq!(dot(Some(89.4)), "🟠");
        assert_eq!(dot(Some(69.6)), "🟠"); // rounds to 70
        assert_eq!(dot(Some(69.4)), "🟢");
        assert_eq!(dot(None), "▫");
    }
```
- [ ] **Step 2: Run tests, verify they fail** \n Run: `cargo test --lib error_is_stale_gates_on_age_and_needs_action dot_thresholds_on_rounded_percent` \n Expected: FAIL — `cannot find function 'error_is_stale'`, and `dot(Some(89.6))` returns `"🟠"` (thresholds on raw 89.6, not rounded 90).
- [ ] **Step 3: Minimal implementation** —
Add the helper to `src/app.rs` (module scope):
```rust
/// Whether a fetch error for a row should render as a stale ⚠︎ warning rather
/// than a muted info line: needs-action rows, rows with no cached data, and data
/// older than 600 s stay warnings; a transient hiccup with fresh data is muted.
/// Pure so it's unit-testable.
fn error_is_stale(needs_action: bool, data_age_secs: Option<i64>) -> bool {
    needs_action || data_age_secs.is_none_or(|a| a > 600)
}
```
Rewrite `dot`:
```rust
fn dot(pct: Option<f64>) -> &'static str {
    // Threshold on the rounded value — the same number the text shows — so a
    // displayed "90%" is never coloured as if it were 89.
    match pct.map(f64::round) {
        Some(p) if p >= 90.0 => "🔴",
        Some(p) if p >= 70.0 => "🟠",
        Some(_) => "🟢",
        None => "▫",
    }
}
```
In `build_row`'s Claude branch (POST-Plan-1: only the `Extra` extras arm remains), wrap the push in a `Some` guard — replace:
```rust
            if report.extra_usage_enabled {
                extras.push(format!("Extra {}", format::percent(report.extra_usage_utilization)));
            }
```
with:
```rust
            if report.extra_usage_enabled {
                if let Some(v) = report.extra_usage_utilization {
                    extras.push(format!("Extra {}", format::percent(Some(v))));
                }
            }
```
Rewrite `row_status` in full:
```rust
    fn row_status(&self, account: &MenuAccount, key: &str, data_date: Option<DateTime<Local>>) -> Option<String> {
        if account.is_codex() && account.is_active && self.needs_action.contains(key) {
            // The live token on disk is stale and PitStop won't rotate it out
            // from under Codex — show the last-known bars with a "Last seen"
            // stamp until Codex saves a fresh token, not the token mechanics.
            return Some(match data_date {
                Some(d) => format!("Last seen {}", format::short_clock(d)),
                None => "No usage data yet".into(),
            });
        }
        if let Some(err) = self.fetch_error.get(key) {
            let mut text = err.clone();
            if let Some(until) = self.next_fetch_allowed.get(key) {
                if !self.needs_action.contains(key) {
                    let remaining = until.saturating_duration_since(Instant::now()).as_secs_f64();
                    text += &if remaining > 1.0 {
                        format!(" — retrying {}", format::relative(remaining))
                    } else {
                        " — retrying on next refresh".into()
                    };
                }
            }
            // Data under 10 minutes old needs no staleness caveat, and a
            // transient hiccup with fresh data isn't worth the orange ⚠︎.
            let age = data_date.map(|d| (Local::now() - d).num_seconds());
            if error_is_stale(self.needs_action.contains(key), age) {
                return Some(match data_date {
                    Some(d) => format!("⚠︎ {text} · showing {} data", format::short_clock(d)),
                    None => format!("⚠︎ {text}"),
                });
            }
            return Some(text);
        }
        if data_date.is_none() {
            return Some("Loading…".into());
        }
        None
    }
```
- [ ] **Step 4: Run tests, verify they pass** \n Run: `cargo test --lib` then `cargo build && cargo clippy --all-targets -- -D warnings`. \n Expected: PASS.
- [ ] **Step 5: Commit** \n `git add -A && git commit -m "Polish row status lines and round bar-dot color"`

---

### Task 11: Auto-switch label + version 0.4.1 + CHANGELOG/README (§2l + version/docs)
**Files:** Modify: `src/tray.rs` (settings label + `version_line_label_correct` test), `Cargo.toml` (version), `Cargo.lock` (regenerated by build), `CHANGELOG.md`, `README.md`.
**Interfaces:** none (copy + version).

Context: Mac `5f50f18` discloses Gemini in the auto-switch copy; the catchup moves the Linux app to **0.4.1**. The version bump makes `tray.rs`'s `version_line_label_correct` test (which asserts `"PitStop v0.3.1"`) the natural failing test.

- [ ] **Step 1: Write the failing test** — in `src/tray.rs` `#[cfg(test)] mod tests`, change `version_line_label_correct` to expect the new version:
```rust
        assert_eq!(label, "PitStop v0.4.1");
```
- [ ] **Step 2: Run test, verify it fails** \n Run: `cargo test --lib version_line_label_correct` \n Expected: FAIL — `assertion failed: left: "PitStop v0.3.1", right: "PitStop v0.4.1"` (`CARGO_PKG_VERSION` is still 0.3.1).
- [ ] **Step 3: Minimal implementation** —
In `Cargo.toml`, bump `version = "0.3.1"` → `version = "0.4.1"`.
In `src/tray.rs` `settings_submenu`, change the auto-switch label `"Auto-switch when an account runs low"` → `"Auto-switch when low (Claude, Codex, Gemini)"`.
In `CHANGELOG.md`, insert a new released section (keep `## [Unreleased]` at the top as an empty header) by changing:
```
## [Unreleased]

### Added
```
to:
```
## [Unreleased]

## [0.4.1] — 2026-07-02

### Added

- **Per-model scoped weekly limits** (e.g. **Fable**), parsed from the Claude
  usage API's new `limits` array and shown as their own labelled bar on Claude
  rows. They count toward the binding number, so the menu-bar %, most-urgent
  tracking, auto-switch, threshold notifications, and projections all react.
- **Single-instance lock:** a second PitStop (e.g. a dev binary beside an
  installed copy) exits at launch instead of fighting over the live credential
  files.
```
and, immediately before the `## [0.3.0] — initial Linux port` header (i.e. after the last existing Added bullet, "…re-authentication flow without leaving PitStop."), insert:
```

### Fixed

- Codex switches preserve an API-key-only `~/.codex/auth.json` instead of
  destroying it; a half-failed Claude switch now rolls the live credential
  file back so it can't disagree with `~/.claude.json`.
- An external re-login (`claude` / `codex`) heals a "re-login needed" row
  within one refresh cycle instead of waiting out the 1-hour backoff.
- Clicking **Deny** on the consent page now reads as a cancel instead of a
  spurious timeout.
- Ghost usage from removed accounts no longer drives the most-urgent menu-bar
  reading or projections (`needs_action`, `notified_bucket`, and usage history
  are pruned on removal).
- Live credential-file writes preserve dotfile symlinks; sub-minute reset
  countdowns show "<1m" instead of "0m"; bar-dot colours match the displayed
  (rounded) percentage; Gemini re-login patches the saved blob so a Google
  response without a refresh token can't strand the profile.

### Changed

- Transient fetch errors with data under 10 minutes old render as a muted info
  line without the "showing … data" caveat; stale-data stamps drop the seconds.
- The live Codex row waiting on a fresh token shows its last-known bars with a
  "Last seen …" stamp instead of a sentence about PitStop's token mechanics.
- The auto-switch setting copy now discloses it covers Gemini as well as
  Claude Code and Codex.
```
In `README.md`, after the intro paragraph ending "…and a one-click switch on each\ninactive account.", add:
```

Claude rows also show any **per-model scoped weekly limits** (e.g. Fable) as
their own labelled bar. Only one PitStop runs at a time: a second launch
(for example a dev binary beside an installed copy) detects the running
instance via a lock file (`~/.config/pitstop/pitstop.lock`) and exits instead
of fighting over the live credential files.
```
- [ ] **Step 4: Run test, verify it passes** \n Run: `cargo build` (regenerates `Cargo.lock` with the new package version), then `cargo test --lib version_line_label_correct`, then `cargo test && cargo clippy --all-targets -- -D warnings`. \n Expected: PASS; `Cargo.lock` shows `name = "pitstop"` / `version = "0.4.1"`.
- [ ] **Step 5: Commit** \n `git add -A && git commit -m "Bump to 0.4.1: disclose Gemini in auto-switch copy, update CHANGELOG/README"`

---

## Self-review notes
- **Coverage vs spec:** §2a→T1, §2b→T2, §2c→T3, §2d→T4, §2e→T5, §2f→T6, §2g→T7, §2h→T8, §2i→T9, §2j+§2k→T10, §2l+version/docs→T11. Gemini for §2g is deliberately N/A (documented) — its snapshot path differs and Google tokens rarely die, matching the spec's "otherwise note as N/A".
- **No placeholders:** every step contains real, compilable code and real commands; no "similar to Task N".
- **Type consistency:** `capture_current` return type change (T7) is threaded through all call sites (`app.rs` fetch_pass/refresh_codex/Save, `main.rs` check, both `switch_to`); `patch_antigravity_blob` signature change (T2) is threaded through gemini tests + `app.rs` + `oauth.rs`; the T2 signature also lands before T10 (no ordering hazard — different files).
- **Plan-1 dependency:** only T10's Extra-guard edit targets a post-Plan-1 shape; the 3 target lines survive Plan 1 unchanged, so the Edit is safe once Plan 1 has removed the Opus/Sonnet arms.
- **Preservation semantics verified against Mac diffs:** `preservingAPIKey` precedence (saved non-empty key wins; live key filled only when saved absent/empty) matches `056a519`; the Gemini patch preserve-on-`None` matches `0970bb4`; the error age-gate (>600, `short_clock`, needs-action) matches `72711a2`; dot rounding matches `a3626bc`.
