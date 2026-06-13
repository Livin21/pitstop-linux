#!/usr/bin/env bash
# Build PitStop (release) and install it for the current user: binary into
# ~/.local/bin, an app icon into the hicolor theme, and a .desktop launcher.
# Autostart-at-login is toggled from the tray menu (Settings → Launch at login).
set -euo pipefail
cd "$(dirname "$0")"

if ! command -v cargo >/dev/null; then
  echo "Rust toolchain not found. Install it: https://rustup.rs" >&2
  exit 1
fi

echo "Building release…"
cargo build --release

BIN_DIR="$HOME/.local/bin"
BIN="$BIN_DIR/pitstop"
mkdir -p "$BIN_DIR"
install -m755 target/release/pitstop "$BIN"

ICON_DIR="$HOME/.local/share/icons/hicolor/128x128/apps"
mkdir -p "$ICON_DIR"
"$BIN" --export-icon "$ICON_DIR/pitstop.png" >/dev/null || true

APPS="$HOME/.local/share/applications"
mkdir -p "$APPS"
cat > "$APPS/pitstop.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=PitStop
Comment=Track AI coding usage limits and switch accounts
Exec=$BIN
Icon=pitstop
Terminal=false
Categories=Utility;
EOF

update-desktop-database "$APPS" 2>/dev/null || true
gtk-update-icon-cache "$HOME/.local/share/icons/hicolor" 2>/dev/null || true

echo
echo "Installed: $BIN"
case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *) echo "Note: $BIN_DIR is not on your PATH — add it to use 'pitstop' directly." ;;
esac
echo "Start it now:   pitstop &        (or launch 'PitStop' from your menu)"
echo "Headless check: pitstop --check"
echo "Autostart:      tray menu → Settings → Launch at login"
