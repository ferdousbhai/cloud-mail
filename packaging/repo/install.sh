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
RELEASES=${CLOUDMAIL_RELEASES_URL:-https://github.com/ferdousbhai/cloud-mail/releases/latest/download}
SIGNING_KEY_FINGERPRINT=52130299581DAD685226900CA89CA1A6A1E74251
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
  # ARM updates replace [omarchy] and everything below its header up to
  # the next section. Keep our Include before repository sections so it
  # cannot be mistaken for part of [omarchy], even on an existing install.
  restore_repo_include() {
    local include=$1 temporary sudo=''
    (( EUID == 0 )) || sudo=sudo
    temporary=$(mktemp)
    awk -v repo_include="$include" '
      $0 == repo_include { next }
      !added && /^[[:space:]]*\[/ && !/^[[:space:]]*\[options\]/ {
        print repo_include
        added = 1
      }
      { print }
      END { if (!added) print repo_include }
    ' /etc/pacman.conf > "$temporary"
    if ! cmp -s "$temporary" /etc/pacman.conf; then
      $sudo tee /etc/pacman.conf < "$temporary" >/dev/null
    fi
    rm -f "$temporary"
  }
  restore_repo_include "$include"
  user="${SUDO_USER:-${USER:-$(id -un)}}"
  home="$(getent passwd "$user" | cut -d: -f6)"
  if [[ -n $home && -d $home/.config/omarchy ]]; then
    hook_dir="$home/.config/omarchy/hooks/pre-refresh-pacman.d"
    install -d -o "$user" -g "$(id -gn "$user")" "$hook_dir"
    {
      printf '%s\n' '#!/bin/bash' 'set -euo pipefail' \
        "# Restore the [$name] repository before Omarchy's ARM source rewrite."
      declare -f restore_repo_include
      printf 'restore_repo_include %q\n' "$include"
    } > "$hook_dir/$name"
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
case "$(uname -m)" in
  x86_64) ;;
  aarch64) REPO=cloudmail-aarch64 ;;
  *) echo "Unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac

# Older/manual installs may have configured the x86_64 repository on ARM.
# Remove only this installer's exact include, config and refresh hook.
if [[ $REPO == cloudmail-aarch64 ]]; then
  sudo=''
  (( EUID == 0 )) || sudo=sudo
  $sudo sed -i '\|^Include = /etc/pacman.d/cloudmail.conf$|d' /etc/pacman.conf
  $sudo rm -f /etc/pacman.d/cloudmail.conf
  user="${SUDO_USER:-${USER:-$(id -un)}}"
  home="$(getent passwd "$user" | cut -d: -f6)"
  [[ -z $home ]] || $sudo rm -f "$home/.config/omarchy/hooks/pre-refresh-pacman.d/cloudmail"
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

# Get started right away when someone is at a terminal: `cloudmail init` asks for an invite link,
# your own domains, then Gmail, iCloud Mail and HEY. It runs as you, not root, in your desktop
# session, so sign-ins open in your browser and tokens land in your keyring. Run by an agent (no
# terminal) it's skipped: the agent runs `cloudmail init --json` itself. CLOUDMAIL_NO_INIT=1 skips it.
run_init() {
  [[ -z ${CLOUDMAIL_NO_INIT:-} && -t 1 ]] || return 1
  # `curl … | sudo bash` reads this script from the pipe, so answers come from the terminal itself.
  { : </dev/tty; } 2>/dev/null || return 1
  local user="${SUDO_USER:-${USER:-$(id -un)}}" uid runtime wayland
  if (( EUID != 0 )); then
    cloudmail init </dev/tty
    return 0
  fi
  [[ $user != root ]] || return 1
  uid="$(id -u "$user")"
  runtime="/run/user/$uid"
  wayland="${WAYLAND_DISPLAY:-}"
  [[ -n $wayland ]] || wayland="$(cd "$runtime" 2>/dev/null && ls -d wayland-[0-9] 2>/dev/null | head -n1)"
  # A login shell, for the PATH where your hey, npm and browser live.
  sudo -u "$user" -i env XDG_RUNTIME_DIR="$runtime" DBUS_SESSION_BUS_ADDRESS="unix:path=$runtime/bus" \
    ${wayland:+WAYLAND_DISPLAY="$wayland"} ${DISPLAY:+DISPLAY="$DISPLAY"} cloudmail init </dev/tty
  return 0
}

echo
if run_init; then
  echo
  echo "Run \`cloudmail init\` again any time to add more. Updates arrive with the rest of the system: omarchy update (or pacman -Syu elsewhere)"
else
  cat <<EOT
Done. Get started (asks for an invite link, your own domains, then Gmail, iCloud Mail and HEY):
  cloudmail init
Or straight to one:
  cloudmail connect <invite link>             # someone invited you
  cloudmail setup you@yourdomain.com          # your own domains, on your Cloudflare account
  cloudmail account add gmail                 # or: icloud, hey
AI agents: \`cloudmail init --json\` says what's set up and what to run next.
Updates arrive with the rest of the system: omarchy update (or pacman -Syu elsewhere)
EOT
fi
