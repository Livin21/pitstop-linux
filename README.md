# PitStop (Linux)

A Linux **system-tray** app that tracks **usage limits** across your AI coding
accounts — **Claude Code**, **OpenAI Codex**, and **Google Gemini (Antigravity)**
— and lets you **switch accounts** with one click, so when one hits its rate
limit you flip to another and your work keeps going.

This is a Rust port of the macOS [PitStop](https://github.com/Livin21/pitstop)
menu-bar app. The tray icon shows the active Claude Code account's usage
percentage, color-coded; clicking it opens a menu grouped by provider with a
color-coded usage bar per rate-limit window, and a one-click switch on each
inactive account.

## What's different from the macOS version

Linux stores these credentials differently, which makes the port **simpler**,
not harder:

| macOS | Linux |
| --- | --- |
| Claude Code credentials in the **Keychain** (`security` CLI, "Always Allow" prompts) | a plain **`~/.claude/.credentials.json`** file (mode 0600) — so switching is a file swap, no prompts |
| Saved account snapshots in the Keychain | 0600 files under `~/.config/pitstop/accounts/` |
| **Claude Desktop** read via Chromium-cookie + Keychain decryption | **dropped** — Claude Desktop has no Linux build |
| `NSStatusItem` menu bar + SwiftUI settings | **StatusNotifierItem** tray (`ksni`) + tray-submenu settings |
| `UNUserNotifications` | `notify-send` (libnotify) |
| launch at login via `SMAppService` | XDG autostart (`~/.config/autostart/pitstop.desktop`) |

The usage/refresh logic (Anthropic OAuth usage endpoint, ChatGPT Codex usage,
token refresh, backoff, auto-switch, projection, threshold notifications) is a
faithful port.

## Requirements

- **Rust** (to build) — https://rustup.rs
- A **StatusNotifierItem host** for the tray icon to appear:
  - **Cinnamon / Linux Mint** — works out of the box (`xapp-sn-watcher`).
  - **KDE Plasma** — native.
  - **GNOME** — install the *AppIndicator and KStatusNotifierItem* extension.
  - **XFCE / sway / others** — any SNI host (e.g. the XFCE *Status Notifier*
    plugin, `waybar`'s tray, or `snixembed`).
- **Claude Code** installed and logged in at least once. Optionally the
  **Codex** CLI/app signed in with a ChatGPT account.
- **Gemini (Antigravity) provider only:** a running **Secret Service** daemon
  (e.g. `gnome-keyring-daemon`). The Antigravity OAuth token is read from
  and written to the GNOME keyring; if no keyring daemon is running the
  Gemini provider will not start.

## Install

```sh
git clone https://github.com/Livin21/pitstop-linux && cd pitstop-linux
./install.sh            # builds release, installs to ~/.local/bin, adds a launcher
pitstop &               # or launch "PitStop" from your application menu
```

Or just build and run from the tree:

```sh
cargo build --release
./target/release/pitstop
```

## Usage

- The **tray icon** shows the active Claude Code account's binding usage
  (`max(5-hour, weekly)`), color-coded (🟢 < 75 %, 🟠 ≥ 75 %, 🔴 ≥ 90 %), dimmed
  when showing stale data. Hover for a tooltip with the exact numbers.
- **Click** the icon for the menu: one section per provider, the live account
  first (●), then the rest by headroom. Each account shows a bar per window
  (5h / 7d for Claude; the plan's windows for Codex). Inactive accounts have a
  **⮂ switch** action — click to make that account live.
- **Save Current Account**, **Remove Account**, **Refresh Now**, and a
  **Settings** submenu (what the icon shows, which account/limit it tracks,
  auto-switch + threshold, time-to-limit projection, launch at login).
- `pitstop --check` prints accounts and live usage to stdout — no GUI.

## Adding a second account

PitStop can only switch between accounts it has snapshotted, and it snapshots
whatever is live on each refresh:

1. PitStop auto-saves whatever account is currently live.
2. Sign in with the **other** account — Claude Code: `claude` → `/login`;
   Codex: run `codex` and sign in.
3. PitStop notices it on the next refresh (or use **Save Current Account**).
4. Both appear in the menu — click an inactive one to switch.

## How it works

- **Claude usage** comes from `api.anthropic.com/api/oauth/usage`, called with
  the same OAuth token Claude Code uses; refreshes every 2 min with exponential
  backoff honoring `Retry-After`. Stale tokens of *saved* accounts are refreshed
  via the OAuth refresh grant; the *active* account is left to Claude Code.
- **Switching Claude** writes the saved blob back into
  `~/.claude/.credentials.json` (carrying that account's MCP OAuth tokens) and
  restores its `oauthAccount` identity in `~/.claude.json`.
- **Codex** uses `~/.codex/auth.json` (shared by the CLI and app) for identity
  and switching; usage from `chatgpt.com/backend-api/codex/usage`. Switching is
  the file analog of the Claude flow.
- **Gemini (Antigravity)** reads the Antigravity OAuth token from the GNOME
  keyring (`service=gemini, account=antigravity`, the same go-keyring blob the
  Antigravity CLI writes). Identity is resolved via Google's `userinfo` endpoint.
  Per-model Google Code Assist usage is shown per rate-limit window. Switching
  an Antigravity account writes the saved token blob back into that same keyring
  entry. **Important caveat: Antigravity's terms discourage rotating this token
  — switch sparingly and keep auto-switch off unless you accept that risk.**
  The **Login** action (shown on expired rows) triggers an in-app Google PKCE
  re-login flow and is a safety net for the rare case that the token expires.
  Diagnostic: `pitstop --gemini-spike` verifies the keyring read and Code Assist
  probe, printing only the resolved account, plan, and per-model usage to stdout
  (no GUI, no secrets — tokens are never printed).
- **Secrets never leave 0600 files or the keyring.** Non-secret metadata lives
  in `~/.config/pitstop/{profiles.json,codex-profiles.json,gemini-profiles.json,settings.json}`.

## Caveats

- The tray icon needs an SNI host (see Requirements). If the icon doesn't
  appear, that's the cause — `pitstop --check` still works regardless.
- Codex keeps inactive snapshots fresh via the OAuth refresh grant, but the
  *live* account is Codex's to maintain — if its on-disk token is expired the
  row says so until you next run `codex`.
- The usage/refresh endpoints are the same unofficial OAuth surface Claude Code
  and Codex use; if they change, update `usage_api.rs` / `codex.rs`.
- **Gemini / Antigravity:** switching rewrites the Antigravity keyring token.
  Google's Antigravity terms discourage rotating this token — switch between
  Gemini accounts sparingly and leave auto-switch off unless you understand and
  accept that caveat. The Gemini provider requires a running Secret Service
  daemon (e.g. `gnome-keyring-daemon`); without one it silently skips Gemini.

## Development

Module map (each mirrors a Swift source file):

| Rust | macOS Swift |
| --- | --- |
| `credentials.rs`, `claude_store.rs` | `Credentials.swift`, `ProfileStore.swift` |
| `codex.rs`, `codex_store.rs` | `Codex.swift`, `CodexStore.swift` |
| `gemini.rs`, `gemini_store.rs` | *(Linux-only)* Antigravity keyring + Code Assist usage |
| `usage_api.rs` | `UsageAPI.swift` |
| `app.rs` | `AppDelegate.swift` (logic half) |
| `tray.rs`, `icon.rs` | status item + `AccountRowView.swift` |
| `settings.rs`, `notify.rs` | `Settings*.swift`, `Notifier.swift` |
| `secret_store.rs` | `Keychain.swift` (now file-based) |
| `secret_service.rs` | *(Linux-only)* D-Bus Secret Service / GNOME keyring |
