#!/usr/bin/env bash
# Update acceptance (decision 0011, PR 4) — POSIX scenario runner.
#
# Runs the five scenario legs a–e against real installed builds:
#   a. v1 -> v2        offered/downloaded/installed, pending->confirmed,
#                      on_update_confirmed once, update-ok, health ready
#   b. v1 -> v2-slow   ~45s of component progress outlives the 10s
#                      heartbeat -> still CONFIRMED (progress.json
#                      advancing the whole window)
#   c. v2 -> v3        on_startup raises -> readiness_failed, v2
#                      restored+relaunched, v3 skipped, re-check offers
#                      nothing
#   d. v2 -> v3-crash  exits before ready x maxBoots -> crash_loop
#   e. v2 -> v3 with   cache corrupted after arming -> watchdog refuses
#      tampered cache  restore (verify_failed), state dir says so and
#                      the running app reports it via health().update
#
# Env:
#   OS       linux | darwin        (default: uname -s mapped)
#   WORK     scratch dir — installs, evidence, home (required)
#   HTDOCS   served tree — releases/<tag>/{artifact,sig,latest.json} and
#            installable/<tag>/ (required)
#   IDENT    tauri identifier (default com.pylair.acceptupd)
#   PORT     loopback port (default 8791)
#
# Linux is meant to run INSIDE the clean ubuntu:22.04 container (xvfb,
# APPIMAGE_EXTRACT_AND_RUN=1, no system webkit — the AppImage's own libs
# are the whole point of the watchdog-spawn claim).

set -uo pipefail

OS="${OS:-$(uname -s)}"
WORK="${WORK:?WORK is required}"
HTDOCS="${HTDOCS:?HTDOCS is required}"
IDENT="${IDENT:-com.pylair.acceptupd}"
PORT="${PORT:-8791}"

case "$OS" in
  Linux|linux)   DATA_DIR="$HOME/.local/share/$IDENT";;
  Darwin|darwin) OS=darwin; DATA_DIR="$HOME/Library/Application Support/$IDENT";;
  *) echo "unknown OS $OS"; exit 2;;
esac
STATE_DIR="$DATA_DIR/updater-state"
LOG="$DATA_DIR/accept/log.jsonl"
EVIDENCE="$WORK/evidence"
TIMINGS="$WORK/timings.log"
FAILED=0

mkdir -p "$WORK/app" "$EVIDENCE" "$DATA_DIR/accept" "$HTDOCS/latest"

say() { echo "== $(date +%H:%M:%S) $*"; }
fail() { echo "FAIL: $*" | tee -a "$WORK/FAILURES"; FAILED=1; }
note_t() { echo "$(date +%s) $*" >> "$TIMINGS"; }

# ---- log assertions ------------------------------------------------------

log_has() { # log_has <tag> <substring>
  [ -f "$LOG" ] || return 1
  grep -F "\"tag\": \"$1\"" "$LOG" 2>/dev/null | grep -F -q "$2"
}
log_count() { # log_count <tag> <substring>
  [ -f "$LOG" ] || { echo 0; return; }
  grep -F "\"tag\": \"$1\"" "$LOG" 2>/dev/null | grep -F -c "$2" || true
}

# wait_for <seconds> <desc> — then a shell fragment evaluated each poll
wait_for() {
  local deadline=$((SECONDS + $1)) desc=$2 frag=$3
  until eval "$frag"; do
    if [ $SECONDS -ge $deadline ]; then
      fail "timeout ($1s): $desc"
      return 1
    fi
    sleep 0.5
  done
}
wait_file()  { wait_for "$1" "$2" "[ -f '$3' ]"; }
wait_log()   { wait_for "$1" "$2" "log_has '$3' '$4'"; }
assert_true() { # assert_true <desc> <frag>
  if ! eval "$2"; then fail "assert: $1"; fi
}

state_seq() { # update.state transitions, consecutive-dup collapsed
  [ -f "$LOG" ] || return 0
  grep -F '"tag": "update.state"' "$LOG" | grep -o '"data": "[a-z_]*"' | cut -d'"' -f4 |
    awk '{ if ($0 != prev) { printf "%s ", $0; prev = $0 } }'
}

# ---- app control ----------------------------------------------------------

set_offer() { cp "$HTDOCS/releases/$1/latest.json" "$HTDOCS/latest/latest.json"; say "offer <- $1"; }

install_tag() { # install_tag <tag> — replace the install with this version
  local s=$SECONDS
  case "$OS" in
    linux)
      cp "$HTDOCS/installable/$1/"*.AppImage "$WORK/app/app.AppImage"
      chmod +x "$WORK/app/app.AppImage";;
    darwin)
      rm -rf "$WORK/app"/*.app
      cp -R "$HTDOCS/installable/$1/"*.app "$WORK/app/";;
  esac
  note_t "install $1 ${SECONDS}s-elapsed=$((SECONDS - s))"
}

APP_PID=""
launch() { # launch — run whatever is currently installed
  case "$OS" in
    linux)
      # DISPLAY comes from the environment (linux-in-container.sh runs
      # one shared Xvfb) — relaunches and the watchdog's restore-spawn
      # land outside any per-launch wrapper and must inherit it.
      APPIMAGE_EXTRACT_AND_RUN=1 "$WORK/app/app.AppImage" >/dev/null 2>&1 &
      APP_PID=$!;;
    darwin)
      local app exe
      app=$(find "$WORK/app" -maxdepth 1 -name '*.app' | head -1)
      exe=$(find "$app/Contents/MacOS" -type f | head -1)
      "$exe" >/dev/null 2>&1 &
      APP_PID=$!;;
  esac
  say "launched pid=$APP_PID"
}

stop_apps() {
  # Recorded ppids are the app processes (python's parent) — kill those,
  # plus our own launch handles. Path-scoped only, never by name.
  local pids=""
  [ -f "$LOG" ] && pids=$(grep -o '"ppid": [0-9]*' "$LOG" | grep -o '[0-9]*' | sort -u)
  # Path-scoped sweep: survivors whose argv carries the install path or
  # the watchdog flag — relaunches the pid records miss (a zombie once
  # blocked an NSIS /S upgrade on the Windows leg and poisoned the run).
  local mine
  mine=$(pgrep -f "$WORK/app" 2>/dev/null; pgrep -f 'pylair-upwatch' 2>/dev/null) || true
  for p in $pids $APP_PID $mine; do
    [ -n "$p" ] && kill "$p" 2>/dev/null || true
  done
  sleep 2
  for p in $pids $APP_PID $mine; do
    [ -n "$p" ] && kill -9 "$p" 2>/dev/null || true
  done
  APP_PID=""
}

snapshot() { # snapshot <name> — preserve state dir + accept log as evidence
  local d="$EVIDENCE/$1"
  mkdir -p "$d"
  cp -a "$DATA_DIR/." "$d/" 2>/dev/null || true
}

reset_state() {
  stop_apps
  rm -rf "$DATA_DIR"
  mkdir -p "$DATA_DIR/accept"
}

corrupt_file() { # replace content in place but as a NEW inode — safe
  # even while a process has the old one open/exec'd (POSIX unlink
  # semantics; ETXTBSY never hits a rename).
  local f=$1
  printf 'CORRUPTED-ACCEPTANCE' > "$f.tmp"
  mv -f "$f.tmp" "$f"
}

serve() {
  (cd "$HTDOCS" && exec python3 -m http.server "$PORT" >/dev/null 2>&1) &
  SERVE_PID=$!
  wait_for 30 "http server on :$PORT" "curl -sf http://127.0.0.1:$PORT/latest/latest.json >/dev/null || curl -sf -o /dev/null http://127.0.0.1:$PORT/"
}

boots_count() { ls "$STATE_DIR/boots" 2>/dev/null | wc -l | tr -d ' '; }

# ---- scenarios ------------------------------------------------------------

scenario_a() {
  say "── scenario a: v1 -> v2 confirms"
  local s=$SECONDS
  reset_state; set_offer v2.0.0; install_tag v1.0.0; launch
  wait_log 420 "v1 records the v2 offer" "install" "2.0.0"
  wait_file 420 "watchdog wrote update-ok" "$STATE_DIR/update-ok"
  wait_log 60 "on_update_confirmed fired for 2.0.0" "confirmed" "2.0.0"
  assert_true "on_update_confirmed fired exactly once" \
    "[ \$(log_count confirmed '2.0.0') -eq 1 ]"
  assert_true "pending cleared on confirm" "[ ! -f '$STATE_DIR/pending.json' ]"
  assert_true "update.state sequence none->pending->confirmed" \
    "[ \"\$(state_seq)\" = 'none pending confirmed ' ]"
  wait_log 60 "health().update surfaces ready" "health" '\"state\":\"ready\"'
  note_t "scenario a $((SECONDS - s))s"
  snapshot "a-confirm"
}

scenario_b() {
  say "── scenario b: v1 -> v2-slow confirms despite heartbeat"
  local s=$SECONDS
  reset_state; set_offer v2.1.0; install_tag v1.0.0; launch
  wait_log 420 "v1 records the v2-slow offer" "install" "2.1.0"
  wait_file 120 "progress.json appears" "$STATE_DIR/progress.json"
  # Progress must keep the window open past heartbeat_ms (10s): sample
  # mtimes ~20s — live writes mean they differ.
  local m1 m2
  m1=$(stat -c %Y "$STATE_DIR/progress.json" 2>/dev/null || stat -f %m "$STATE_DIR/progress.json")
  sleep 8
  m2=$(stat -c %Y "$STATE_DIR/progress.json" 2>/dev/null || stat -f %m "$STATE_DIR/progress.json")
  assert_true "progress.json advanced during slow ready ($m1 -> $m2)" \
    "[ '$m1' != '$m2' ]"
  wait_file 300 "v2-slow confirmed (update-ok)" "$STATE_DIR/update-ok"
  assert_true "no rollback happened" "[ ! -f '$STATE_DIR/rollback-reason.txt' ]"
  wait_log 60 "on_update_confirmed fired for 2.1.0" "confirmed" "2.1.0"
  note_t "scenario b $((SECONDS - s))s"
  snapshot "b-slow-confirm"
}

scenario_c() {
  say "── scenario c: v2 -> v3 rolls back readiness_failed"
  local s=$SECONDS
  reset_state; set_offer v3.0.0; install_tag v2.0.0; launch
  wait_log 420 "v2 records the v3 offer" "install" "3.0.0"
  wait_file 300 "rollback reason written" "$STATE_DIR/rollback-reason.txt"
  assert_true "reason is readiness_failed" \
    "grep -q 'readiness_failed' '$STATE_DIR/rollback-reason.txt'"
  assert_true "v2 restored (outcome)" \
    "grep -q 'restored 2.0.0' '$STATE_DIR/rollback-reason.txt'"
  assert_true "v3 on the skip list" \
    "grep -q '3.0.0' '$STATE_DIR/skipped.json'"
  # The relaunched v2 needs real boot+record time: a bare wait_log on
  # the tag matches the INITIAL boot's line instantly — the count probe
  # is the actual evidence.
  wait_for 300 "restored v2 booted (second 2.0.0 boot record)" \
    "[ \$(log_count boot '2.0.0') -ge 2 ]"
  wait_log 300 "restored v2's fresh check is skipped -> null" "check" '"data": "null"'
  assert_true "health().update surfaces rolled-back + lastRollback" \
    "log_has health 'rolled-back'"
  note_t "scenario c $((SECONDS - s))s"
  snapshot "c-readiness-rollback"
}

scenario_d() {
  say "── scenario d: v2 -> v3-crash rolls back crash_loop"
  local s=$SECONDS
  reset_state; set_offer v4.0.0; install_tag v2.0.0; launch
  wait_log 420 "v2 records the v4 offer" "install" "4.0.0"
  wait_file 120 "pending armed" "$STATE_DIR/pending.json"
  # The driver's own relaunch is boot #1 (v4 exits ~1.5s in). Drive two
  # more launches of the crashed install — each writes a boots/ marker.
  wait_for 180 "boot #1 marker" "[ \$(boots_count) -ge 1 ]"
  launch; wait_for 180 "boot #2 marker" "[ \$(boots_count) -ge 2 ]"
  launch; wait_for 180 "boot #3 marker" "[ \$(boots_count) -ge 3 ]"
  wait_file 300 "rollback reason written" "$STATE_DIR/rollback-reason.txt"
  assert_true "reason is crash_loop" \
    "grep -q 'crash_loop' '$STATE_DIR/rollback-reason.txt'"
  assert_true "v2 restored (outcome)" \
    "grep -q 'restored 2.0.0' '$STATE_DIR/rollback-reason.txt'"
  wait_for 300 "restored v2 booted (second 2.0.0 boot record)" \
    "[ \$(log_count boot '2.0.0') -ge 2 ]"
  note_t "scenario d $((SECONDS - s))s"
  snapshot "d-crash-loop"
}

scenario_e() {
  say "── scenario e: tampered cache -> verify_failed, no restore"
  local s=$SECONDS
  reset_state; set_offer v3.0.0; install_tag v2.0.0; launch
  wait_log 420 "v2 records the v3 offer" "install" "3.0.0"
  wait_file 120 "pending armed + cache written" "$STATE_DIR/pending.json"
  local artifact
  artifact=$(find "$STATE_DIR/cache" -type f ! -name 'upwatch-*' | head -1)
  [ -n "$artifact" ] || { fail "no cached artifact under $STATE_DIR/cache"; }
  corrupt_file "$artifact"
  say "corrupted $artifact"
  wait_file 300 "rollback reason written" "$STATE_DIR/rollback-reason.txt"
  assert_true "restore refused (verify_failed)" \
    "grep -q 'verify_failed' '$STATE_DIR/rollback-reason.txt'"
  # v3 stays installed — nothing restored it. It still boots (into its
  # broken startup), and its health().update reports the aborted
  # rollback through lastRollback.
  wait_log 300 "v3 booted (still installed)" "boot" "3.0.0"
  wait_log 300 "app reports verify_failed via health().update" \
    "health" "verify_failed"
  note_t "scenario e $((SECONDS - s))s"
  snapshot "e-tamper-abort"
}

# macOS evidence: the staged watchdog copy IS the app binary signed
# ad-hoc with the hardened runtime — record its signature info and the
# fact that it executed (the upwatch.log lines it wrote are the proof
# it ran outside the .app bundle).
darwin_evidence() {
  [ "$OS" = "darwin" ] || return 0
  local out="$EVIDENCE/macos-codesign.txt" copy
  for copy in "$EVIDENCE"/*/updater-state/cache/upwatch-* "$STATE_DIR/cache/upwatch-"*; do
    [ -f "$copy" ] || continue
    {
      echo "── staged watchdog copy: $copy"
      codesign -dv --verbose=4 "$copy" 2>&1
      file "$copy"
    } >> "$out" 2>&1
  done
  say "codesign evidence -> $out"
}

# ---- driver ---------------------------------------------------------------

say "acceptance posix runner: OS=$OS WORK=$WORK PORT=$PORT"
serve
for sc in "$@"; do "scenario_$sc"; done
stop_apps
darwin_evidence
kill "$SERVE_PID" 2>/dev/null || true
say "timings:"; cat "$TIMINGS" 2>/dev/null || true
if [ "$FAILED" -ne 0 ]; then echo "── FAILURES ──"; cat "$WORK/FAILURES"; exit 1; fi
say "all scenarios green"
