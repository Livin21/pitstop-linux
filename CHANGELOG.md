# Changelog

All notable changes to PitStop (Linux) will be documented here.

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
- **Self-update:** at startup and once per day, PitStop checks GitHub for a
  newer release. When one is found the menu shows the new version and an
  "Update & Relaunch" item; if the app was installed via `install.sh` it runs
  `git pull` + `cargo build` and exec-relaunches; otherwise it opens the
  releases page.

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

## [0.4.0] — 2026-07-02

### Added

- **Gemini provider (Antigravity surface).** Live Code Assist usage, account
  switching, auto-switch participation, and an in-app Login safety net. The
  Antigravity OAuth token is read from and written back to the GNOME keyring
  (`service=gemini, account=antigravity`). Note: Antigravity's terms discourage
  rotating this token — switching is surfaced with that caveat and auto-switch
  stays opt-in.
- `pitstop --gemini-spike` diagnostic: reads the keyring token and probes the
  Code Assist endpoint, printing results to stdout (no GUI required).
- **Secret Service / D-Bus keyring layer** (`secret_service.rs`): go-keyring
  compatible read/write over the D-Bus Secret Service protocol, used by the
  Gemini provider.
- **In-app Login flow** for expired accounts (Claude Code, Codex, Gemini):
  a Login button appears on expired rows and triggers the provider's PKCE
  re-authentication flow without leaving PitStop.

## [0.3.0] — initial Linux port

### Added

- Claude Code provider: usage from `api.anthropic.com/api/oauth/usage`, token
  refresh, account snapshots in `~/.config/pitstop/accounts/`, one-click
  account switching via `~/.claude/.credentials.json`.
- Codex provider: usage from `chatgpt.com/backend-api/codex/usage`, identity
  and switching via `~/.codex/auth.json`.
- StatusNotifierItem tray icon (ksni), color-coded by usage percentage.
- Auto-switch, threshold notifications, time-to-limit projection.
- XDG autostart launcher, `install.sh`, `pitstop --check` headless mode.
