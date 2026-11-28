#!/usr/bin/env bash
set -Eeuo pipefail

# omui's dev/qa.sh for Cloudmail: runs the app, built with feature `qa`, through QA scripts (the protocol is in omui's
# README) against dev/fake-worker's made-up mail, on a broadway display of its own with a private HOME and XDG
# directories and no session bus. No window opens anywhere visible, nothing of the desktop this is run from is read,
# and no real account is reached. Prints each command with its answer; fails when a command fails or GLib logs a
# warning or a critical. Each script gets a fresh app and worker, so one script's moves are not the next one's mail.
# A directory `<script>.state` beside `<script>.jsonl` is what the app finds in its state directory as it starts (a
# draft kept from an earlier run), and a file `<script>.state-after` lists the files that must be there once it has
# quit, one path a line, sorted, with N for each run of digits: a draft's name is the time it was made.
# Google is the tests' stand-in `gws` (crates/cloudmail/tests/fake-gws), which signs in at once as me@gmail.example
# and opens no browser. A file `<script>.google` lists the Google sign-ins the other Cloud apps have as the app starts,
# one a line: `<app's config directory>/<account> <address>`, as in `cloud-calendar/google sam@gmail.example`.
#
#   dev/qa.sh <app-binary> <script.jsonl>...
# There is no desktop portal on the hidden display: a script answers the file chooser itself (omui's `picker`), with
# a failure or with `attachment.txt`, a copy of dev/qa/attachment.txt put in the directory the app runs in.
#
#   CLOUDMAIL_QA_SHOTS=<dir>   where a script's `shot` paths are written (default: a scratch directory, removed)
#   CLOUDMAIL_QA_JOBS=4        how many scripts run at once (default: half the processors)
#   OMUI_QA_TIMEOUT=120        seconds before one script's run is stopped
(( $# >= 2 )) || { echo "usage: dev/qa.sh <app-binary> <script.jsonl>..." >&2; exit 2; }
app=$(realpath -- "$1")
shift
here=$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
command -v gtk4-broadwayd >/dev/null || { echo "qa: gtk4-broadwayd is required" >&2; exit 1; }

root=""
shots=""
shots_root=""
out=""
broadway_pid=""
worker_pid=""
app_pid=""
runs=()
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
# The scripts run side by side, each in a shell of its own that stops what it started; this one stops those shells.
finish() {
  local pid
  for pid in "${runs[@]}"; do kill -TERM "$pid" 2>/dev/null || true; done
  wait
  [[ -z $shots ]] || rm -f -- "$shots/attachment.txt"
  [[ -z $out ]] || rm -rf -- "$out"
  [[ -z $shots_root ]] || rm -rf -- "$shots_root"
}
trap 'status=$?; finish; exit "$status"' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
if [[ -n ${CLOUDMAIL_QA_SHOTS:-} ]]; then
  mkdir -p "$CLOUDMAIL_QA_SHOTS"
  shots=$(realpath -- "$CLOUDMAIL_QA_SHOTS")
else
  shots_root=$(mktemp -d /tmp/cloudmail-qa-shots.XXXXXX)
  shots=$shots_root
fi
# What a script's `picker` names by its bare name is found where the app runs; removed again as this ends.
cp -- "$here/qa/attachment.txt" "$shots/attachment.txt"
out=$(mktemp -d /tmp/cloudmail-qa-out.XXXXXX)

run() {
  local script=$1 index=$2
  root=$(mktemp -d /tmp/cloudmail-qa.XXXXXX)
  mkdir -p "$root"/{home,config,state,data,cache,runtime}
  chmod 700 "$root/runtime"
  local seed=${script%.jsonl}.state after=${script%.jsonl}.state-after
  [[ ! -d $seed ]] || cp -r -- "$seed/." "$root/state/"
  # Each sign-in as the stand-in gws keeps one: what its `auth status` says of it, and a key beside it.
  local google=${script%.jsonl}.google where address
  if [[ -f $google ]]; then
    while read -r where address; do
      [[ -n $where && $where != '#'* ]] || continue
      mkdir -p "$root/config/${where%/*}/gws/${where##*/}"
      printf 'key' > "$root/config/${where%/*}/gws/${where##*/}/.encryption_key"
      printf '{"token_valid": true, "user": "%s", "scopes": ["https://www.googleapis.com/auth/gmail.modify", "https://www.googleapis.com/auth/gmail.settings.basic"]}\n' \
        "$address" > "$root/config/${where%/*}/gws/${where##*/}/credentials.enc"
    done < "$google"
  fi

  # A display number and input port from this script's PID and the script's place, so that neither two runs at once
  # nor two scripts one after the other share one. broadwayd listens on that port on 127.0.0.1 alone, where omui sends
  # key presses as broadway's web page would.
  local display=$(( 100 + ($$ * 8 + index) % 800 ))
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
  (
    cd "$shots"
    exec env -u WAYLAND_DISPLAY -u DISPLAY -u DBUS_SESSION_BUS_ADDRESS -u GTK_THEME -u CLOUDMAIL_SCRIPT \
      HOME="$root/home" XDG_CONFIG_HOME="$root/config" XDG_STATE_HOME="$root/state" XDG_DATA_HOME="$root/data" \
      XDG_CACHE_HOME="$root/cache" XDG_RUNTIME_DIR="$root/runtime" \
      CLOUDMAIL_API_URL="http://127.0.0.1:$(< "$root/port")" CLOUDMAIL_API_TOKEN=qa \
      CLOUDMAIL_HEY_COMMAND=/usr/bin/false CLOUDMAIL_GWS_COMMAND="$here/../../cloudmail/tests/fake-gws" \
      CLOUDMAIL_BROWSER=/usr/bin/true \
      WEBKIT_DISABLE_DMABUF_RENDERER=1 GDK_BACKEND=broadway BROADWAY_DISPLAY=":$display" \
      OMUI_QA=1 OMUI_QA_INPUT="127.0.0.1:$port" OMUI_QA_FD=3 \
      timeout "${OMUI_QA_TIMEOUT:-120}" "$app" <"$script" 3>"$root/answers"
  ) &
  app_pid=$!
  wait "$app_pid" || status=$?
  app_pid=""
  # The display going away ends the app with no answer; what it said is why.
  kill -0 "$broadway_pid" 2>/dev/null || { echo "qa: broadwayd exited:" >&2; cat "$root/broadwayd.log" >&2; }

  # Each command beside its answer; `quit`'s answer, or the failure that ended the run, comes last.
  echo "qa: $script"
  grep -Ev '^[[:space:]]*(#|$)' "$script" | awk -v answers="$root/answers" '
    { if ((getline answer < answers) > 0) { print "> " $0; print "< " answer } else { print "> " $0 " (not run)" } }
    END { while ((getline answer < answers) > 0) print "< " answer }'
  if (( status == 0 )) && [[ -f $after ]]; then
    diff <(cd "$root/state" && find . -type f | sed -E 's|^\./||; s/[0-9]+/N/g' | LC_ALL=C sort) "$after" \
      || { echo "qa: the state directory's files (<) are not those $after lists (>)" >&2; status=1; }
  fi
  return "$status"
}

# Side by side: each script has its own app, worker, display and directories. An app at work keeps most of a processor
# busy and a toast is gone after five seconds whatever the load, so no more run at once than half the processors. Each
# one's output is kept and printed whole, in the order given, so a run reads as it did one after another.
at_once=${CLOUDMAIL_QA_JOBS:-$(( ($(nproc) + 1) / 2 ))}
index=0
for script in "$@"; do
  while (( $(jobs -rp | wc -l) >= at_once )); do wait -n || true; done
  (
    trap 'status=$?; stop; exit "$status"' EXIT
    trap 'exit 130' INT
    trap 'exit 143' TERM
    status=0
    run "$(realpath -- "$script")" "$index" || status=$?
    stop
    if (( status == 0 )); then echo "qa: passed"; else echo "qa: failed with status $status"; fi
    echo "$status" > "$out/$index.status"
  ) >"$out/$index" 2>&1 &
  runs+=($!)
  index=$(( index + 1 ))
done
# A script's shell that was stopped, or died, left no status: that is a failure too.
failed=0
for index in "${!runs[@]}"; do
  wait "${runs[index]}" 2>/dev/null || true
  cat -- "$out/$index"
  [[ $(cat -- "$out/$index.status" 2>/dev/null) == 0 ]] || failed=1
done
runs=()
exit "$failed"
