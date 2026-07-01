# Changelog

All notable changes to PitStop (Linux) will be documented here.

## [Unreleased]

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
