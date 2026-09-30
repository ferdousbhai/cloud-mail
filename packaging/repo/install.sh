#!/bin/bash
# Install Cloudmail (the cloudmail CLI, its cmail alias, and the cloudmail-gtk
# app) from its signed package repository, and keep it updating with the system:
#
#   curl -fsSL https://ferdousbhai.com/cloudmail/install.sh | sudo bash
#
# Every step is idempotent, so re-running is safe. It trusts the
# package-signing key (checked against the fingerprint pinned below), adds the
# [cloudmail] repository, installs an Omarchy hook that restores it after
# `omarchy refresh pacman` rewrites /etc/pacman.conf, and installs the package
# plus npm, which `cloudmail setup` uses to deploy your mail worker.
set -euo pipefail

REPO=cloudmail
RELEASES=https://github.com/ferdousbhai/cloud-mail/releases/latest/download
SIGNING_KEY_FINGERPRINT=35C47A06567940B6796B4D0F9B3C7BDF85268B31
PACKAGES=(cloudmail npm)

# --- add_signed_repo (shared) ---
# Trust a project's package-signing key (checked against the pinned
# fingerprint), add its signed pacman repository, and keep the repository
# across `omarchy refresh pacman`, which rewrites /etc/pacman.conf from
# Omarchy's defaults and then runs the user's pre-refresh-pacman hooks.
# Works as root (`sudo bash`) or as a desktop user (sudo inside). This text
# is identical in every installer that uses it, and each repository's test
# pins its hash: change it here and in its twins together.
add_signed_repo() {
  local name="$1" release="$2" fingerprint="$3"
  local conf="/etc/pacman.d/$name.conf" include="Include = /etc/pacman.d/$name.conf"
  local sudo='' key user home hook_dir
  (( EUID == 0 )) || sudo=sudo
  key="$(mktemp)"
  if ! curl -fsSL "$release/$name-signing-key.asc" -o "$key"; then
    rm -f "$key"
    echo "Could not download the package-signing key from $release." >&2
    return 1
  fi
  if ! gpg --batch --with-colons --show-keys "$key" 2>/dev/null | grep -q "^fpr:*:$fingerprint:"; then
    rm -f "$key"
    echo "The downloaded key does not match the pinned fingerprint $fingerprint; nothing was changed." >&2
    return 1
  fi
  $sudo pacman-key --add "$key"
  $sudo pacman-key --lsign-key "$fingerprint"
  rm -f "$key"
  printf '[%s]\nSigLevel = Required DatabaseRequired\nServer = %s\n' "$name" "$release" | $sudo tee "$conf" >/dev/null
  grep -qxF "$include" /etc/pacman.conf || printf '\n%s\n' "$include" | $sudo tee -a /etc/pacman.conf >/dev/null
  user="${SUDO_USER:-${USER:-$(id -un)}}"
  home="$(getent passwd "$user" | cut -d: -f6)"
  if [[ -n $home && -d $home/.config/omarchy ]]; then
    hook_dir="$home/.config/omarchy/hooks/pre-refresh-pacman.d"
    install -d -o "$user" -g "$(id -gn "$user")" "$hook_dir"
    printf '%s\n' '#!/bin/bash' \
      "# Restore the [$name] repository after Omarchy rewrote /etc/pacman.conf." \
      "grep -qxF '$include' /etc/pacman.conf || printf '\\n%s\\n' '$include' | sudo tee -a /etc/pacman.conf >/dev/null" \
      > "$hook_dir/$name"
    chown "$user" "$hook_dir/$name"
    chmod 755 "$hook_dir/$name"
  fi
  $sudo pacman -Sy
}
# --- end add_signed_repo ---

if ! command -v pacman >/dev/null; then
  echo "pacman not found: this installer is for Omarchy and other Arch Linux systems." >&2
  exit 1
fi
if [[ $(uname -m) != x86_64 ]]; then
  echo "The signed repository has x86_64 packages only; on $(uname -m), build from source:" >&2
  echo "  https://github.com/ferdousbhai/cloud-mail#from-source" >&2
  exit 1
fi

echo "Adding the [$REPO] repository"
add_signed_repo "$REPO" "$RELEASES" "$SIGNING_KEY_FINGERPRINT"

echo "Installing ${PACKAGES[*]}"
if command -v omarchy-pkg-add >/dev/null; then
  # Omarchy refuses direct `pacman -Syu` (system upgrades go through `omarchy update`), so install
  # the way Omarchy installs its own apps; the next `omarchy update` brings everything current.
  omarchy-pkg-add "${PACKAGES[@]}"
else
  # Upgrade and install in one transaction: add_signed_repo has just synced every database, and
  # installing from those without upgrading is a partial upgrade.
  sudo=''
  (( EUID == 0 )) || sudo=sudo
  $sudo pacman -Syu --needed --noconfirm "${PACKAGES[@]}"
fi

cat <<EOT

Done. Launch "Cloudmail" from the app launcher (Super + Space), or run: cloudmail
To deploy your own mail worker to your Cloudflare account:
  cloudmail setup you@yourdomain.com
Updates arrive with the rest of the system: omarchy update (or pacman -Syu elsewhere)
EOT
