#!/usr/bin/env bash
# U0 spike CI driver: builds v1/v2/v3, serves them locally, and proves
#   1) v1 -> v2 full-bundle update (bundled backend + runtime replaced)
#   2) v2 -> v3(broken on_startup) -> readiness-gated rollback to v2
#   3) v3 lands on the skip list and is not re-offered
# Usage: ./ci.sh   (per-OS; workflow supplies toolchain + deps)
set -euo pipefail
cd "$(dirname "$0")"
OS=$(uname -s)

STATE=$PWD/out/state
rm -rf out dist
mkdir -p "$STATE" dist
export PYLAIR_STATE_DIR=$STATE
export PYLAIR_UPDATE_TEST=1
export PYLAIR_READY_DELAY_MS=1500
export PYLAIR_EXIT_AFTER_READY_MS=5000
export PYLAIR_ROLLBACK_DEADLINE_MS=25000
export PYLAIR_MAX_BOOTS=3
export PYTHONUTF8=1

# Pick a python that actually runs (windows git-bash sees MS Store stubs).
PY=""
for c in python3 python py; do
  "$c" -c "import sys" >/dev/null 2>&1 && { PY=$c; break; }
done
[ -n "$PY" ] || { echo "no working python"; exit 1; }
fail() { echo "FAIL: $*" | tee -a "$STATE/results.txt"; tail -60 "$STATE/log.txt" 2>/dev/null; exit 1; }
wait_for() { # wait_for <file> <seconds>
  local f=$1 t=$2
  for _ in $(seq "$t"); do [ -e "$f" ] && return 0; sleep 1; done
  return 1
}
say() { echo; echo "== $* =="; }

# ---------- throwaway keys ----------
say "keys"
mkdir -p out/keys
if ! npx --yes @tauri-apps/cli@2 signer generate -w "$PWD/out/keys/u0.key" -p ""; then
  printf '\n' | npx --yes @tauri-apps/cli@2 signer generate -w "$PWD/out/keys/u0.key"
fi
export TAURI_SIGNING_PRIVATE_KEY
TAURI_SIGNING_PRIVATE_KEY="$(cat out/keys/u0.key)"
export TAURI_SIGNING_PRIVATE_KEY_PASSWORD=""
export PUBKEY
PUBKEY="$(cat out/keys/u0.key.pub)"
echo "pubkey: $PUBKEY"

# ---------- builds ----------
say "build v1 v2 v3"
bash ./build.sh 0.1.0
bash ./build.sh 0.2.0
bash ./build.sh 0.3.0 --broken
{ uname -m | sed 's/arm64/aarch64/'; } > dist/ARCH
WATCHDOG=$(find "$PWD/dist/0.1.0" \( -name 'upwatch' -o -name 'upwatch.exe' \) | head -1)
export PYLAIR_WATCHDOG_BIN=$WATCHDOG
say "artifacts"
du -h dist/*/* | sort -k2 | tee "$STATE/sizes.txt"

# ---------- server ----------
say "serve"
"$PY" serve.py 8377 & SRV=$!
echo 0.2.0 > dist/CHANNEL
ok=""
for _ in $(seq 15); do curl -sf http://127.0.0.1:8377/latest.json -o "$STATE/latest-v2.json" && ok=1 && break; sleep 1; done
[ -n "$ok" ] || fail "update server never answered"
cat "$STATE/latest-v2.json" | head -c 400; echo

# ---------- per-OS install + launch ----------
APP=""
case "$OS" in
  Linux)
    cp dist/0.1.0/*.AppImage "$STATE/app.AppImage"; chmod +x "$STATE/app.AppImage"
    APP="$STATE/app.AppImage"
    launch() { APPIMAGE_EXTRACT_AND_RUN=1 "$APP" & }
    ;;
  Darwin)
    mkdir -p "$STATE/appdir"
    tar xzf dist/0.1.0/*.app.tar.gz -C "$STATE/appdir"
    APP=$(find "$STATE/appdir" -name 'u0-updater-spike.app' -maxdepth 2)/Contents/MacOS/u0-updater-spike
    chmod +x "$APP" || true
    launch() { "$APP" & }
    ;;
  *)
    setup=$(find "$PWD/dist/0.1.0" -name '*-setup.exe' | head -1)
    MSYS2_ARG_CONV_EXCL='*' "$setup" /S || true
    for i in $(seq 90); do
      exe=$(find "$LOCALAPPDATA" -name 'u0-updater-spike.exe' 2>/dev/null | head -1)
      [ -n "$exe" ] && break
      sleep 1
    done
    [ -n "${exe:-}" ] || fail "installed exe never appeared"
    APP=$exe
    launch() { "$APP" & }
    ;;
esac

assert_marker() { # assert_marker <ver>
  grep -q "VERSION = \"$1\"" "$STATE/backend-marker.txt" || fail "backend marker not $1: $(cat "$STATE/backend-marker.txt" | head -5)"
  grep -q "runtime-marker $1" "$STATE/backend-marker.txt" || fail "runtime marker not $1"
  echo "PASS marker $1" >> "$STATE/results.txt"
}

# ---------- proof 1: v1 -> v2 ----------
say "proof 1: v1 -> v2 update"
T0=$SECONDS
launch
wait_for "$STATE/ready-0.2.0" 300 || fail "v2 never reported ready after update"
echo "v1->v2 update+ready: $((SECONDS-T0))s" | tee -a "$STATE/results.txt"
sleep 8  # let the restarted v2 settle + write marker dump before it exits
assert_marker 0.2.0
echo "PASS v1->v2 update" >> "$STATE/results.txt"

# ---------- proof 2: v2 -> v3 broken -> rollback ----------
say "proof 2: v2 -> v3(broken) -> rollback v2"
echo 0.3.0 > dist/CHANNEL
T0=$SECONDS
launch
wait_for "$STATE/rollback-reason.txt" 240 || fail "no rollback within 240s"
echo "v2->v3->v2 rollback: $((SECONDS-T0))s" | tee -a "$STATE/results.txt"
cat "$STATE/rollback-reason.txt" | tee -a "$STATE/results.txt"
grep -q "restored 0.2.0" "$STATE/rollback-reason.txt" || fail "rollback did not restore 0.2.0"
grep -q '"version": "0.3.0"' "$STATE/skipped.json" || fail "v3 not on skip list"
wait_for "$STATE/ready-0.2.0" 5 || true
sleep 8
assert_marker 0.2.0
echo "PASS rollback to v2 + skip list" >> "$STATE/results.txt"

# ---------- proof 3: v3 not re-offered ----------
say "proof 3: v3 on skip list, not re-offered"
: > "$STATE/check3.log" 2>/dev/null || true
launch
sleep 12   # let check() run; v3 must hit the skip gate
if grep -q "skip: 0.3.0 on skip list" "$STATE/log.txt"; then
  echo "PASS skip list stops re-offer" >> "$STATE/results.txt"
else
  tail -40 "$STATE/log.txt"; fail "v3 was not skipped (or check never ran)"
fi

say "results"
cat "$STATE/results.txt"
echo
echo "== state dir tail =="
tail -50 "$STATE/log.txt"
kill $SRV 2>/dev/null || true
