#!/usr/bin/env bash
set -Eeuo pipefail

# omui's dev/qa.sh for Cloudmail: runs the app, built with feature `qa`, through QA scripts (the protocol is in omui's
# README) against dev/fake-worker's made-up mail, on a broadway display of its own with a private HOME and XDG
# directories and no session bus. No window opens anywhere visible, nothing of the desktop this is run from is read,
# and no real account is reached. Prints each command with its answer; fails when a command fails or GLib logs a
# warning or a critical. Each script gets a fresh app and worker, so one script's moves are not the next one's mail.
#
#   dev/qa.sh <app-binary> <script.jsonl>...
#   OMUI_QA_TIMEOUT=120                    seconds before one script's run is stopped
(( $# >= 2 )) || { echo "usage: dev/qa.sh <app-binary> <script.jsonl>..." >&2; exit 2; }
app=$(realpath -- "$1")
shift
here=$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
command -v gtk4-broadwayd >/dev/null || { echo "qa: gtk4-broadwayd is required" >&2; exit 1; }

root=""
broadway_pid=""
worker_pid=""
app_pid=""
stop() {
  # Only what this script started.
  local pid
  for pid in "$app_pid" "$worker_pid" "$broadway_pid"; do
    [[ -z $pid ]] || kill -TERM "$pid" 2>/dev/null || true
    [[ -z $pid ]] || wait "$pid" 2>/dev/null || true
  done
  app_pid="" worker_pid="" broadway_pid=""
  [[ -z $root ]] || rm -rf -- "$root"
  root=""
}
trap 'status=$?; stop; exit "$status"' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

run() {
  local script=$1
  root=$(mktemp -d /tmp/cloudmail-qa.XXXXXX)
  mkdir -p "$root"/{home,config,state,data,cache,runtime}
  chmod 700 "$root/runtime"

  # A display number and input port from this script's PID, so that two runs at once do not share one. broadwayd
  # listens on that port on 127.0.0.1 alone, where omui sends key presses as broadway's web page would.
  local display=$(( 100 + $$ % 800 ))
  local port=$(( 8080 + display ))
  XDG_RUNTIME_DIR=$root/runtime gtk4-broadwayd --address 127.0.0.1 --port "$port" ":$display" \
    >"$root/broadwayd.log" 2>&1 &
  broadway_pid=$!
  for _ in {1..50}; do
    [[ -S $root/runtime/broadway$((display + 1)).socket ]] && break
    kill -0 "$broadway_pid" 2>/dev/null || { cat "$root/broadwayd.log" >&2; return 1; }
    sleep 0.1
  done

  python3 "$here/fake-worker" "$root/port" &
  worker_pid=$!
  for _ in {1..50}; do
    [[ -s $root/port ]] && break
    sleep 0.1
  done
  [[ -s $root/port ]] || { echo "qa: the stand-in worker did not start" >&2; return 1; }

  # The worker and its token come from the environment, so neither a config file nor a keyring is looked for, and the
  # stand-in commands keep a linked account from reaching the real services. In the background, waited for: bash runs
  # a trap only once the foreground command ends, and `timeout` puts the app in a process group Ctrl-C does not reach.
  # Broadway has no OpenGL, and WebKit's DMA-BUF renderer warns that it fell back without it, which would fail the run;
  # WebKit's other renderer draws the same message without asking for OpenGL.
  local status=0
  env -u WAYLAND_DISPLAY -u DISPLAY -u DBUS_SESSION_BUS_ADDRESS -u GTK_THEME -u CLOUDMAIL_SCRIPT \
    HOME="$root/home" XDG_CONFIG_HOME="$root/config" XDG_STATE_HOME="$root/state" XDG_DATA_HOME="$root/data" \
    XDG_CACHE_HOME="$root/cache" XDG_RUNTIME_DIR="$root/runtime" \
    CLOUDMAIL_API_URL="http://127.0.0.1:$(< "$root/port")" CLOUDMAIL_API_TOKEN=qa \
    CLOUDMAIL_HEY_COMMAND=/usr/bin/false CLOUDMAIL_GWS_COMMAND=/usr/bin/false CLOUDMAIL_BROWSER=/usr/bin/true \
    WEBKIT_DISABLE_DMABUF_RENDERER=1 GDK_BACKEND=broadway BROADWAY_DISPLAY=":$display" \
    OMUI_QA=1 OMUI_QA_INPUT="127.0.0.1:$port" OMUI_QA_FD=3 \
    timeout "${OMUI_QA_TIMEOUT:-120}" "$app" <"$script" 3>"$root/answers" &
  app_pid=$!
  wait "$app_pid" || status=$?
  app_pid=""

  # Each command beside its answer; `quit`'s answer, or the failure that ended the run, comes last.
  echo "qa: $script"
  grep -Ev '^[[:space:]]*(#|$)' "$script" | awk -v answers="$root/answers" '
    { if ((getline answer < answers) > 0) { print "> " $0; print "< " answer } else { print "> " $0 " (not run)" } }
    END { while ((getline answer < answers) > 0) print "< " answer }'
  return "$status"
}

failed=0
for script in "$@"; do
  status=0
  run "$(realpath -- "$script")" || status=$?
  stop
  if (( status == 0 )); then echo "qa: passed"; else echo "qa: failed with status $status"; failed=1; fi
done
exit "$failed"
