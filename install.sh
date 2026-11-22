#!/usr/bin/env bash
# Builds cloudmail (CLI) and its desktop app and installs them for the current user: `cloudmail` in ~/.local/bin,
# the app under ~/.local/lib/cloudmail (`cloudmail open` starts it), and `cloudmail.desktop` for the app launcher.
#
# To make Cloudmail open mailto: links afterwards:
#   xdg-mime default cloudmail.desktop x-scheme-handler/mailto
#
# Set CLOUDMAIL_NO_GTK=1 to install only the CLI (no GTK4/WebKitGTK needed),
# CLOUDMAIL_NO_ALIAS=1 to skip the short `cmail` alias, CLOUDMAIL_NO_INIT=1 to skip `cloudmail init`.
set -euo pipefail
cd "$(dirname "$0")"

BIN="${CLOUDMAIL_BIN_DIR:-$HOME/.local/bin}"
SHARE="${XDG_DATA_HOME:-$HOME/.local/share}"

if [[ "${CLOUDMAIL_NO_GTK:-}" == 1 ]]; then
  cargo build --release -p cloudmail
else
  # The desktop app builds against GTK 4 and WebKitGTK 6.0; on Arch, install whichever is missing.
  if ! pkg-config --exists gtk4 webkitgtk-6.0 2>/dev/null && command -v pacman >/dev/null; then
    echo "Installing GTK 4 and WebKitGTK 6.0 for the desktop app"
    sudo pacman -S --needed gtk4 webkitgtk-6.0
  fi
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
  # Where `cloudmail open` looks: lib/cloudmail beside the directory cloudmail is in.
  LIB="$(dirname "$BIN")/lib/cloudmail"
  install -Dm755 target/release/cloudmail-gtk "$LIB/cloudmail-gtk"
  install -Dm644 crates/cloudmail-gtk/data/cloudmail.desktop "$SHARE/applications/cloudmail.desktop"
  install -Dm644 crates/cloudmail-gtk/data/cloudmail.svg "$SHARE/icons/hicolor/scalable/apps/cloudmail.svg"
  # The app's command before 0.8, kept for key bindings and notification actions that name it: it runs
  # `cloudmail open`, and replaces the app an earlier install put there.
  install -Dm755 crates/cloudmail-gtk/data/cloudmail-gtk "$BIN/cloudmail-gtk"
  # What an install from before 0.8 left under the old names; `cloudmail` itself points a mailto: default that named
  # the old .desktop file at the new one when it next runs.
  rm -f "$SHARE/applications/com.ferdousbhai.Cloudmail.desktop" \
    "$SHARE/icons/hicolor/scalable/apps/com.ferdousbhai.Cloudmail.svg"
  command -v update-desktop-database >/dev/null && update-desktop-database "$SHARE/applications" 2>/dev/null || true
  command -v gtk-update-icon-cache >/dev/null && gtk-update-icon-cache -q -t "$SHARE/icons/hicolor" 2>/dev/null || true
fi

echo "Installed to $BIN: cloudmail$([[ "${CLOUDMAIL_NO_GTK:-}" != 1 ]] && echo " (the app: cloudmail open, or Cloudmail in the launcher)")"
case ":$PATH:" in *":$BIN:"*) ;; *) echo "Note: $BIN is not on your PATH." ;; esac
# At a terminal, get started right away (invite link, your own domains, Gmail, iCloud Mail, HEY);
# CLOUDMAIL_NO_INIT=1 skips it, and an agent runs `cloudmail init --json` itself.
if [[ -z "${CLOUDMAIL_NO_INIT:-}" && -t 0 && -t 1 ]]; then
  "$BIN/cloudmail" init || true
else
  echo "Next: \`cloudmail init\` (asks what to set up; agents: \`cloudmail init --json\`)."
fi
