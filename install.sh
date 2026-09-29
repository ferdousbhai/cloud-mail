#!/usr/bin/env bash
# Builds cloudmail (CLI) and cloudmail-gtk (desktop app) and installs them for the current user.
#
# To make Cloudmail open mailto: links afterwards:
#   xdg-mime default com.ferdousbhai.Cloudmail.desktop x-scheme-handler/mailto
#
# Set CLOUDMAIL_NO_GTK=1 to install only the CLI (no GTK4/WebKitGTK needed),
# CLOUDMAIL_NO_ALIAS=1 to skip the short `cmail` alias.
set -euo pipefail
cd "$(dirname "$0")"

BIN="${CLOUDMAIL_BIN_DIR:-$HOME/.local/bin}"
SHARE="${XDG_DATA_HOME:-$HOME/.local/share}"

if [[ "${CLOUDMAIL_NO_GTK:-}" == 1 ]]; then
  cargo build --release -p cloudmail
else
  cargo build --release -p cloudmail -p cloudmail-gtk
fi

install -Dm755 target/release/cloudmail "$BIN/cloudmail"

# Short alias. Never replaces a different program that already owns the name.
if [[ "${CLOUDMAIL_NO_ALIAS:-}" != 1 ]]; then
  if [[ ! -e "$BIN/cmail" || -L "$BIN/cmail" ]]; then
    ln -sfn cloudmail "$BIN/cmail"
  else
    echo "Skipped the cmail alias: $BIN/cmail already exists and isn't ours" >&2
  fi
fi

if [[ "${CLOUDMAIL_NO_GTK:-}" != 1 ]]; then
  install -Dm755 target/release/cloudmail-gtk "$BIN/cloudmail-gtk"
  install -Dm644 crates/cloudmail-gtk/data/com.ferdousbhai.Cloudmail.desktop "$SHARE/applications/com.ferdousbhai.Cloudmail.desktop"
  install -Dm644 crates/cloudmail-gtk/data/com.ferdousbhai.Cloudmail.svg "$SHARE/icons/hicolor/scalable/apps/com.ferdousbhai.Cloudmail.svg"
  command -v update-desktop-database >/dev/null && update-desktop-database "$SHARE/applications" 2>/dev/null || true
  command -v gtk-update-icon-cache >/dev/null && gtk-update-icon-cache -q -t "$SHARE/icons/hicolor" 2>/dev/null || true
fi

echo "Installed to $BIN: cloudmail$([[ "${CLOUDMAIL_NO_GTK:-}" != 1 ]] && echo ', cloudmail-gtk')"
case ":$PATH:" in *":$BIN:"*) ;; *) echo "Note: $BIN is not on your PATH." ;; esac
"$BIN/cloudmail" --styled >/dev/null 2>&1 && "$BIN/cloudmail" status --styled 2>/dev/null || echo "Next: run \`cloudmail setup\` (see README) or \`cloudmail config set api-url …\`."
