#!/usr/bin/env bash
# U0 spike build: ./build.sh <ver> [--broken]
# Patches version into tauri.conf.json + markers, injects $PUBKEY, builds
# signed bundles (needs TAURI_SIGNING_PRIVATE_KEY[_PASSWORD] in env) and
# collects the updater artifacts + sigs into dist/<ver>/.
set -euo pipefail

VER="$1"
BROKEN="${2:-}"
cd "$(dirname "$0")"
SRC=app/src-tauri
OUT=dist/$VER
mkdir -p "$OUT"
PY=python3; command -v python3 >/dev/null 2>&1 || PY=python

"$PY" - "$VER" "$BROKEN" <<'PY'
import json, re, sys
ver, broken = sys.argv[1], sys.argv[2] == "--broken"

conf = json.load(open("app/src-tauri/tauri.conf.json"))
conf["version"] = ver
json.dump(conf, open("app/src-tauri/tauri.conf.json", "w"), indent=2)

app = open("app/backend/app.py").read()
app = re.sub(r'VERSION = ".*"', f'VERSION = "{ver}"', app)
app = re.sub(r'BROKEN = \w+', f'BROKEN = {"True" if broken else "False"}', app)
open("app/backend/app.py", "w").write(app)

open("app/python/pylair-runtime.txt", "w").write(f"runtime-marker {ver}\n")
PY

# Inject the throwaway pubkey (generated per CI run, kept out of git).
if [ -n "${PUBKEY:-}" ]; then
  "$PY" - "$PUBKEY" <<'PY'
import json, sys
conf = json.load(open("app/src-tauri/tauri.conf.json"))
conf["plugins"]["updater"]["pubkey"] = sys.argv[1]
json.dump(conf, open("app/src-tauri/tauri.conf.json", "w"), indent=2)
PY
fi

case "$(uname -s)" in
  Linux)  BUNDLES=appimage ;;
  Darwin) BUNDLES=app ;;
  *)      BUNDLES=nsis ;;
esac

cd "$SRC"
T0=$(date +%s)
npx --yes @tauri-apps/cli@2 build --bundles "$BUNDLES"
echo "[build] $VER tauri build: $(( $(date +%s) - T0 ))s" >&2
cd ../..

# Collect updater artifacts + sigs.
case "$BUNDLES" in
  appimage)
    cp "$SRC"/target/release/bundle/appimage/*.AppImage "$SRC"/target/release/bundle/appimage/*.sig "$OUT"/ ;;
  app)
    cp "$SRC"/target/release/bundle/macos/*.app.tar.gz "$SRC"/target/release/bundle/macos/*.sig "$OUT"/ ;;
  nsis)
    cp "$SRC"/target/release/bundle/nsis/*-setup.exe "$SRC"/target/release/bundle/nsis/*.sig "$OUT"/ ;;
esac
cp "$SRC"/target/release/upwatch* "$OUT"/ 2>/dev/null || true
ls -la "$OUT"
