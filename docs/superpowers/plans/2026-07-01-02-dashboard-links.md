# Usage-Dashboard Links Implementation Plan
> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (- [ ]) syntax for tracking.

**Goal:** Add a clickable "↗ Open <Provider> usage dashboard" item beneath each provider's section header in the tray menu, opening the URL with `xdg-open`.
**Architecture:** Add `Provider::dashboard_url() -> Option<&'static str>` to `model.rs`; add `Action::OpenUrl(String)` + a fire-and-forget `open_url()` helper to `app.rs`; extend `GroupView` with `dashboard_url: Option<String>` and populate it in `Engine::grouped_view()`; render the clickable item in `PitStopTray::menu()`. No new modules, no new threads.
**Tech Stack:** std::process::Command (xdg-open), existing ksni MenuItem / send_item pattern, existing Provider enum in model.rs.
**Depends on:** none

## Global Constraints
- Rust 2021; single tokio task (Engine::run tokio::select! loop over an mpsc Action channel + 120s timer); ksni tray; no new threads/locks in the render path.
- Secrets only in 0600 files or the GNOME keyring; never logged.
- reqwest (async) for HTTP; serde/serde_json for JSON; chrono for time; anyhow for errors.
- Each task ends green: cargo build clean, cargo test passes, cargo clippy clean, one commit.

---

### Task 1: Add `Provider::dashboard_url()` to `model.rs`
**Files:** Modify: `src/model.rs:8-22`  
**Interfaces:** Consumes: nothing | Produces: `Provider::dashboard_url() -> Option<&'static str>` (used by Task 3)

#### Context

Current `Provider` impl block in `src/model.rs` (lines 14–22):

```rust
impl Provider {
    pub fn title(&self) -> &'static str {
        match self {
            Provider::Claude => "Claude",
            Provider::Codex => "Codex",
        }
    }
    pub const ALL: [Provider; 2] = [Provider::Claude, Provider::Codex];
}
```

- [ ] **Step 1: Write the failing test**

  Add at the bottom of `src/model.rs`:

  ```rust
  #[cfg(test)]
  mod tests {
      use super::*;

      #[test]
      fn dashboard_url_claude() {
          assert_eq!(
              Provider::Claude.dashboard_url(),
              Some("https://claude.ai/new#settings/usage"),
          );
      }

      #[test]
      fn dashboard_url_codex() {
          assert_eq!(
              Provider::Codex.dashboard_url(),
              Some("https://chatgpt.com/codex/cloud/settings/analytics#usage"),
          );
      }

      #[test]
      fn all_current_providers_have_dashboard_url() {
          for p in Provider::ALL {
              assert!(
                  p.dashboard_url().is_some(),
                  "{} is missing dashboard_url",
                  p.title()
              );
          }
      }
  }
  ```

- [ ] **Step 2: Run test, verify it fails**

  Run: `cargo test dashboard_url`

  Expected: FAIL — `error[E0599]: no method named 'dashboard_url' found for enum 'Provider'`

- [ ] **Step 3: Minimal implementation**

  Insert the new method into the existing `impl Provider` block in `src/model.rs`, immediately after `title()` (after line 20 of the current file, before `pub const ALL`):

  ```rust
      /// Web usage dashboard for this provider.
      /// NOTE: The `Provider::Gemini` arm (`Some("https://gemini.google.com/usage")`)
      /// is intentionally absent here — it will be added by Plan 4 when the Gemini
      /// variant is introduced. Add it as:
      ///   Provider::Gemini => Some("https://gemini.google.com/usage"),
      pub fn dashboard_url(&self) -> Option<&'static str> {
          match self {
              Provider::Claude => Some("https://claude.ai/new#settings/usage"),
              Provider::Codex => Some("https://chatgpt.com/codex/cloud/settings/analytics#usage"),
          }
      }
  ```

  The updated `impl Provider` block becomes:

  ```rust
  impl Provider {
      pub fn title(&self) -> &'static str {
          match self {
              Provider::Claude => "Claude",
              Provider::Codex => "Codex",
          }
      }

      /// Web usage dashboard for this provider.
      /// NOTE: The `Provider::Gemini` arm (`Some("https://gemini.google.com/usage")`)
      /// is intentionally absent here — it will be added by Plan 4 when the Gemini
      /// variant is introduced. Add it as:
      ///   Provider::Gemini => Some("https://gemini.google.com/usage"),
      pub fn dashboard_url(&self) -> Option<&'static str> {
          match self {
              Provider::Claude => Some("https://claude.ai/new#settings/usage"),
              Provider::Codex => Some("https://chatgpt.com/codex/cloud/settings/analytics#usage"),
          }
      }

      pub const ALL: [Provider; 2] = [Provider::Claude, Provider::Codex];
  }
  ```

- [ ] **Step 4: Run test, verify it passes**

  Run: `cargo test dashboard_url`

  Expected: PASS — three tests (`dashboard_url_claude`, `dashboard_url_codex`, `all_current_providers_have_dashboard_url`) all pass; `cargo clippy` clean.

- [ ] **Step 5: Commit**

  ```
  git add src/model.rs && git commit -m "$(cat <<'EOF'
  feat(model): add Provider::dashboard_url() for Claude and Codex

  Returns the exact URL for each provider's web usage dashboard. The
  Gemini arm is left as a comment placeholder for Plan 4 to fill in
  once Provider::Gemini is introduced.
  EOF
  )"
  ```

---

### Task 2: Add `Action::OpenUrl(String)` and `open_url()` helper to `app.rs`
**Files:** Modify: `src/app.rs:29-37` (Action enum), `src/app.rs:146-201` (handle_action)  
**Interfaces:** Consumes: nothing | Produces: `Action::OpenUrl(String)`, `pub(crate) fn open_url(url: &str)` (used by Task 3)

#### Context

Current `Action` enum in `src/app.rs` (lines 29–37):

```rust
#[derive(Clone)]
pub enum Action {
    RefreshNow,
    Summary,
    Switch { key: String },
    Save,
    Remove { key: String },
    SetSetting(SettingChange),
    Quit,
}
```

`handle_action` match at `src/app.rs:147` ends with `Action::Quit => std::process::exit(0)`.

`notify.rs` uses `std::process::Command` / `Stdio::null()` / fire-and-forget `.spawn()` — the same pattern is used here.

- [ ] **Step 1: Write the failing test**

  Add inside a `#[cfg(test)]` block at the bottom of `src/app.rs`:

  ```rust
  #[cfg(test)]
  mod tests {
      use super::*;

      #[test]
      fn action_open_url_variant_round_trips() {
          let action = Action::OpenUrl("https://claude.ai/new#settings/usage".to_string());
          match &action {
              Action::OpenUrl(url) => {
                  assert_eq!(url, "https://claude.ai/new#settings/usage");
              }
              _ => panic!("Expected Action::OpenUrl"),
          }
      }
  }
  ```

- [ ] **Step 2: Run test, verify it fails**

  Run: `cargo test action_open_url_variant_round_trips`

  Expected: FAIL — `error[E0599]: no variant named 'OpenUrl' in enum 'Action'` (or compile error)

- [ ] **Step 3: Minimal implementation**

  **3a.** Add `OpenUrl(String)` to the `Action` enum in `src/app.rs` (after `Remove` and before `SetSetting`):

  ```rust
  #[derive(Clone)]
  pub enum Action {
      RefreshNow,
      Summary,
      Switch { key: String },
      Save,
      Remove { key: String },
      OpenUrl(String),
      SetSetting(SettingChange),
      Quit,
  }
  ```

  **3b.** Add a standalone `open_url` helper function at module level in `src/app.rs` (place it near the bottom of the file, after the `pick_auto_switch` function, which currently ends at line 1000):

  ```rust
  /// Shells out to `xdg-open` to open `url` in the default browser.
  /// Fire-and-forget: if `xdg-open` is absent the error goes to stderr only.
  pub(crate) fn open_url(url: &str) {
      use std::process::{Command, Stdio};
      let result = Command::new("xdg-open")
          .arg(url)
          .stdout(Stdio::null())
          .stderr(Stdio::null())
          .spawn();
      if result.is_err() {
          eprintln!("PitStop: xdg-open unavailable — visit {url}");
      }
  }
  ```

  **3c.** Add the `OpenUrl` arm to `handle_action` in `src/app.rs`. The current last arm is `Action::Quit => std::process::exit(0)` (line 199). Insert before it:

  ```rust
              Action::OpenUrl(url) => {
                  open_url(&url);
              }
  ```

  So `handle_action` looks like:

  ```rust
      async fn handle_action(&mut self, action: Action) {
          match action {
              Action::RefreshNow => { /* ... existing ... */ }
              Action::Summary => { /* ... existing ... */ }
              Action::Switch { key } => { /* ... existing ... */ }
              Action::Save => { /* ... existing ... */ }
              Action::Remove { key } => { /* ... existing ... */ }
              Action::OpenUrl(url) => {
                  open_url(&url);
              }
              Action::SetSetting(change) => {
                  self.apply_setting(change);
                  self.render().await;
              }
              Action::Quit => std::process::exit(0),
          }
      }
  ```

- [ ] **Step 4: Run test, verify it passes**

  Run: `cargo test action_open_url_variant_round_trips`

  Expected: PASS; `cargo build` clean; `cargo clippy` clean.

- [ ] **Step 5: Commit**

  ```
  git add src/app.rs && git commit -m "$(cat <<'EOF'
  feat(app): add Action::OpenUrl(String) and open_url() helper

  New action shells out to xdg-open (fire-and-forget, matching the
  notify-send pattern). Falls back to stderr when xdg-open is absent.
  EOF
  )"
  ```

---

### Task 3: Thread `dashboard_url` through `GroupView` and render it in the tray menu
**Files:** Modify: `src/tray.rs:19-22` (GroupView struct), `src/tray.rs:112-136` (menu loop) / Modify: `src/app.rs:697-706` (grouped_view push)  
**Interfaces:** Consumes: `Provider::dashboard_url() -> Option<&'static str>` (Task 1), `Action::OpenUrl(String)` (Task 2) | Produces: visible "↗ Open <Provider> usage dashboard" menu item per provider section

#### Context

`GroupView` in `src/tray.rs` (lines 19–22):

```rust
pub struct GroupView {
    pub title: String,
    pub rows: Vec<RowView>,
}
```

`PitStopTray::menu()` loop in `src/tray.rs` (lines 112–136):

```rust
for g in &v.groups {
    items.push(disabled(format!("——  {}  ——", g.title)));
    for row in &g.rows {
        // ... row rendering
    }
}
```

`Engine::grouped_view()` group push in `src/app.rs` (lines 697–706):

```rust
            groups.push(GroupView {
                title: provider.title().into(),
                rows,
            });
```

- [ ] **Step 1: Write the failing test**

  Add to the existing `#[cfg(test)] mod tests` block in `src/tray.rs` (create the block if absent):

  ```rust
  #[cfg(test)]
  mod tests {
      use super::*;

      #[test]
      fn group_view_carries_dashboard_url() {
          let g = GroupView {
              title: "Claude".to_string(),
              dashboard_url: Some("https://claude.ai/new#settings/usage".to_string()),
              rows: vec![],
          };
          assert_eq!(
              g.dashboard_url.as_deref(),
              Some("https://claude.ai/new#settings/usage"),
          );
      }

      #[test]
      fn group_view_dashboard_url_can_be_none() {
          let g = GroupView {
              title: "Unknown".to_string(),
              dashboard_url: None,
              rows: vec![],
          };
          assert!(g.dashboard_url.is_none());
      }
  }
  ```

- [ ] **Step 2: Run test, verify it fails**

  Run: `cargo test group_view_carries_dashboard_url`

  Expected: FAIL — `error[E0560]: struct 'GroupView' has no field named 'dashboard_url'`

- [ ] **Step 3: Minimal implementation**

  **3a.** Add the `dashboard_url` field to `GroupView` in `src/tray.rs` (lines 19–22). Replace:

  ```rust
  pub struct GroupView {
      pub title: String,
      pub rows: Vec<RowView>,
  }
  ```

  with:

  ```rust
  pub struct GroupView {
      pub title: String,
      pub dashboard_url: Option<String>,
      pub rows: Vec<RowView>,
  }
  ```

  **3b.** Update `Engine::grouped_view()` in `src/app.rs` to populate the field. The existing push (lines 697–706) becomes:

  ```rust
              groups.push(GroupView {
                  title: provider.title().into(),
                  dashboard_url: provider.dashboard_url().map(str::to_string),
                  rows,
              });
  ```

  **3c.** Update `PitStopTray::menu()` in `src/tray.rs` to render the dashboard link. Replace the existing group loop (lines 112–136):

  ```rust
          for g in &v.groups {
              items.push(disabled(format!("——  {}  ——", g.title)));
              for row in &g.rows {
  ```

  with:

  ```rust
          for g in &v.groups {
              items.push(disabled(format!("——  {}  ——", g.title)));
              if let Some(url) = &g.dashboard_url {
                  items.push(send_item(
                      format!("↗ Open {} usage dashboard", g.title),
                      true,
                      Action::OpenUrl(url.clone()),
                  ));
              }
              for row in &g.rows {
  ```

  The rest of the row-rendering loop is unchanged.

- [ ] **Step 4: Run test, verify it passes**

  Run: `cargo test group_view_carries_dashboard_url group_view_dashboard_url_can_be_none`

  Then run: `cargo build && cargo clippy -- -D warnings`

  Expected: both tests PASS; build and clippy both clean.

  Manual verification: launch the tray (`cargo run` or the installed binary) and confirm each provider section (Claude, Codex) now shows a "↗ Open Claude usage dashboard" / "↗ Open Codex usage dashboard" item directly under the `——  Claude  ——` / `——  Codex  ——` separator. Clicking the item should open the browser at the correct URL.

- [ ] **Step 5: Commit**

  ```
  git add src/tray.rs src/app.rs && git commit -m "$(cat <<'EOF'
  feat(tray): render '↗ Open <Provider> usage dashboard' item per section

  GroupView gains a dashboard_url field populated from Provider::dashboard_url().
  The tray menu renders a clickable send_item under each provider header;
  clicking dispatches Action::OpenUrl which shells out to xdg-open.
  EOF
  )"
  ```

---

## Plan-4 hook (Gemini variant)

When Plan 4 introduces `Provider::Gemini`, the implementor must make exactly three edits to bring dashboard links to Gemini with zero additional wiring:

1. **`src/model.rs` — `Provider::dashboard_url()`**: add the Gemini arm (replace the comment):
   ```rust
   Provider::Gemini => Some("https://gemini.google.com/usage"),
   ```
2. **`src/model.rs` — `Provider::ALL`**: extend the array to include `Provider::Gemini`.
3. **`src/model.rs` — `Provider::title()`**: add `Provider::Gemini => "Gemini"`.

No changes to `tray.rs` or `app.rs` are needed — the grouping loop and render path are already provider-agnostic.
