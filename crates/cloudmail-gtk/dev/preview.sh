#!/usr/bin/env bash
set -Eeuo pipefail

# The app in a nested Hyprland with a private HOME, XDG directories and D-Bus, reading made-up mail from
# dev/fake-worker: nothing of the desktop it is run from is read or shown on, and no real account is reached.
#
#   dev/preview.sh <screenshot.png> [theme-dir [next-theme-dir]]
#
# `theme-dir` holds a `colors.toml` (an Omarchy theme, such as /usr/share/omarchy/themes/catppuccin-latte) and is
# copied in as the scratch desktop's current theme; without one the app shows its fallback. With `next-theme-dir`
# the scratch theme directory is then replaced by `mv` while the app runs, as Omarchy does on a switch, and a second
# picture is written beside the first as `<screenshot>-switched.png`.
#
#   CLOUDMAIL_SCRIPT           the debug build's script (src/ui/devscript.rs); the picture is taken once it has run
#   CLOUDMAIL_PREVIEW_KEYS     key names (`wtype -k`: e, Return, Escape) pressed for real once the script has run, 1.5 s
#                              apart, before the picture: what a dialog does with keys is seen only this way, since
#                              the script's keys go straight to the window's handler
#   CLOUDMAIL_PREVIEW_BINARY   the app to run (default target/debug/cloudmail-gtk)
#   CLOUDMAIL_PREVIEW_SIZE     the output's size (default 1400x900); under 1000 wide the sidebar is icons only
#   CLOUDMAIL_PREVIEW_LOG      a file for what the app printed
if (( $# < 1 || $# > 3 )); then
  echo "usage: $0 <screenshot.png> [theme-dir [next-theme-dir]]" >&2
  exit 2
fi
screenshot=$(realpath -m -- "$1")
theme=${2:-}
next_theme=${3:-}

here=$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
binary=${CLOUDMAIL_PREVIEW_BINARY:-$here/../../../target/debug/cloudmail-gtk}
[[ -x $binary ]] || { echo "preview: build the app first (cargo build -p cloudmail-gtk)" >&2; exit 1; }
for command_name in Hyprland hyprctl grim jq dbus-daemon setsid python3; do
  command -v "$command_name" >/dev/null || { echo "preview: $command_name is required" >&2; exit 1; }
done
for dir in "$theme" "$next_theme"; do
  [[ -z $dir || -f $dir/colors.toml ]] || { echo "preview: $dir has no colors.toml" >&2; exit 1; }
done
parent_runtime=${XDG_RUNTIME_DIR:?preview: XDG_RUNTIME_DIR is required}
parent_display=${WAYLAND_DISPLAY:?preview: run this from a Wayland desktop}
parent_socket=$parent_runtime/$parent_display
script=${CLOUDMAIL_SCRIPT:-j}
# The script's steps run 1.5 s apart, the first after 1.5 s; then time for the last one to draw.
steps=${script//[^;]/}
settle=$(( (${#steps} + 1) * 3 / 2 + 4 ))

root=""
runtime=""
hypr_pid=""
app_pid=""
worker_pid=""
dbus_pid=""
cleanup() {
  local status=$?
  trap - EXIT INT TERM
  local pid
  # Only the PIDs this script started.
  for pid in "$app_pid" "$worker_pid" "$hypr_pid" "$dbus_pid"; do
    [[ -z $pid ]] || kill -TERM "$pid" 2>/dev/null || true
  done
  for _ in {1..30}; do
    local alive=0
    for pid in "$app_pid" "$worker_pid" "$hypr_pid" "$dbus_pid"; do
      [[ -z $pid ]] || ! kill -0 "$pid" 2>/dev/null || alive=1
    done
    (( alive == 0 )) && break
    sleep 0.1
  done
  for pid in "$app_pid" "$worker_pid" "$hypr_pid" "$dbus_pid"; do
    [[ -z $pid ]] || kill -KILL "$pid" 2>/dev/null || true
  done
  [[ -z $root || $root != /tmp/cloudmail-preview.* ]] || rm -rf -- "$root"
  [[ -z $runtime || $runtime != /tmp/cmp.* ]] || rm -rf -- "$runtime"
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# Wayland and Hyprland IPC use AF_UNIX paths capped at 108 bytes.
root=$(mktemp -d /tmp/cloudmail-preview.XXXXXX)
runtime=$(mktemp -d /tmp/cmp.XXXXXX)
chmod 700 "$root" "$runtime"
mkdir -p "$root"/{home/.local/state,config,data,cache}

export HOME=$root/home
# The state directory is where Omarchy's scripts write the current theme, whatever XDG_STATE_HOME says.
export XDG_CONFIG_HOME=$root/config XDG_STATE_HOME=$HOME/.local/state XDG_DATA_HOME=$root/data XDG_CACHE_HOME=$root/cache
export XDG_RUNTIME_DIR=$runtime
unset GTK_THEME

current=$XDG_STATE_HOME/omarchy/current
# What `omarchy-theme-set` leaves behind: the theme's files in a directory of their own, and its name beside it.
stage_theme() {
  mkdir -p "$current/$2"
  cp -- "$1/colors.toml" "$current/$2/colors.toml"
}
if [[ -n $theme ]]; then
  stage_theme "$theme" theme
  basename -- "$(realpath -- "$theme")" > "$current/theme.name"
fi

# A session bus with no service directories: nothing can be dbus-activated on it, and it has no keyring.
cat > "$root/dbus.conf" <<EOF_DBUS
<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:abstract=cloudmail-preview-$BASHPID</listen>
  <keep_umask/>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
EOF_DBUS
mapfile -t dbus_details < <(/usr/bin/dbus-daemon --config-file="$root/dbus.conf" --fork --print-address=1 --print-pid=1)
(( ${#dbus_details[@]} == 2 )) || { echo "preview: could not start the isolated session bus" >&2; exit 1; }
export DBUS_SESSION_BUS_ADDRESS=${dbus_details[0]}
dbus_pid=${dbus_details[1]}

python3 "$here/fake-worker" "$root/port" &
worker_pid=$!
for _ in {1..50}; do
  [[ -s $root/port ]] && break
  sleep 0.1
done
[[ -s $root/port ]] || { echo "preview: the stand-in worker did not start" >&2; exit 1; }
# The worker and its token come from the environment, so neither a config file nor a keyring is looked for; and the
# stand-in commands keep a linked account, were one configured, from reaching the real services.
export CLOUDMAIL_API_URL="http://127.0.0.1:$(< "$root/port")" CLOUDMAIL_API_TOKEN=preview
export CLOUDMAIL_HEY_COMMAND=/usr/bin/false CLOUDMAIL_GWS_COMMAND=/usr/bin/false CLOUDMAIL_BROWSER=/usr/bin/true

printf '%s\n' \
  "hl.monitor({ output = \"\", mode = \"${CLOUDMAIL_PREVIEW_SIZE:-1400x900}@60\", position = \"auto\", scale = 1 })" \
  'hl.config({' \
  '  general = { gaps_in = 0, gaps_out = 0, border_size = 0 },' \
  '  decoration = { rounding = 0, shadow = { enabled = false }, blur = { enabled = false } },' \
  '  animations = { enabled = false },' \
  '  misc = { disable_hyprland_logo = true, disable_splash_rendering = true },' \
  '})' \
  'hl.window_rule({ match = { class = ".*" }, monitor = "cloudmail" })' > "$root/hyprland.lua"
unset HYPRLAND_INSTANCE_SIGNATURE
/usr/bin/setsid /usr/bin/env WAYLAND_DISPLAY="$parent_socket" /usr/bin/Hyprland --config "$root/hyprland.lua" > "$root/hyprland.log" 2>&1 &
hypr_pid=$!
signature=""
display=""
for _ in {1..120}; do
  kill -0 "$hypr_pid" 2>/dev/null || { echo "preview: nested Hyprland exited during startup" >&2; sed -n '1,80p' "$root/hyprland.log" >&2; exit 1; }
  for instance in "$XDG_RUNTIME_DIR"/hypr/*; do
    [[ -S $instance/.socket.sock && -f $instance/hyprland.lock ]] || continue
    candidate=$(sed -n '2p' "$instance/hyprland.lock")
    [[ -n $candidate && -S $XDG_RUNTIME_DIR/$candidate ]] || continue
    signature=$(basename "$instance")
    display=$candidate
    break 2
  done
  sleep 0.1
done
[[ -n $signature ]] || { echo "preview: nested Hyprland did not publish its sockets" >&2; exit 1; }
export HYPRLAND_INSTANCE_SIGNATURE=$signature WAYLAND_DISPLAY=$display

# The nested compositor's own output is a window on the desktop this runs from, as large as that desktop tiles it.
# The app goes on a headless output instead, whose size is the monitor rule's; the window rule sends it there.
hyprctl output create headless cloudmail >/dev/null

CLOUDMAIL_SCRIPT=$script GDK_BACKEND=wayland "$binary" > "$root/app.log" 2>&1 &
app_pid=$!
on_output() {
  local output
  output=$(hyprctl monitors -j | jq 'first(.[] | select(.name == "cloudmail") | .id)') &&
    hyprctl clients -j | jq -e --argjson output "${output:-null}" 'any(.[]; .mapped and .monitor == $output)' >/dev/null
}
for _ in {1..100}; do
  on_output && break
  kill -0 "$app_pid" 2>/dev/null || break
  sleep 0.1
done
shoot() {
  sleep "$2"
  kill -0 "$app_pid" 2>/dev/null || { echo "preview: the app exited" >&2; sed -n '1,40p' "$root/app.log" >&2; exit 1; }
  # A picture of an output the window is not on would be blank, and look like a result.
  on_output || { echo "preview: the app's window is not on the output being captured" >&2; hyprctl clients >&2; exit 1; }
  mkdir -p "$(dirname "$1")"
  # Hyprland's own notices (it says it was started without its wrapper) would cover the window.
  hyprctl dismissnotify >/dev/null
  /usr/bin/grim -o cloudmail "$1"
  echo "preview: $1"
}
if [[ -n ${CLOUDMAIL_PREVIEW_KEYS:-} ]]; then
  command -v wtype >/dev/null || { echo "preview: wtype is required for CLOUDMAIL_PREVIEW_KEYS" >&2; exit 1; }
  sleep "$settle"
  for key in $CLOUDMAIL_PREVIEW_KEYS; do
    wtype -k "$key"
    sleep 1.5
  done
  settle=1
fi
shoot "$screenshot" "$settle"
if [[ -n $next_theme ]]; then
  stage_theme "$next_theme" next
  [[ ! -d $current/theme ]] || mv -- "$current/theme" "$current/previous"
  mv -- "$current/next" "$current/theme"
  rm -rf -- "$current/previous"
  basename -- "$(realpath -- "$next_theme")" > "$current/theme.name"
  shoot "${screenshot%.png}-switched.png" 4
fi
[[ -z ${CLOUDMAIL_PREVIEW_LOG:-} ]] || cp -- "$root/app.log" "$CLOUDMAIL_PREVIEW_LOG"
[[ ! -s $root/app.log ]] || sed -n '1,20p' "$root/app.log" >&2
