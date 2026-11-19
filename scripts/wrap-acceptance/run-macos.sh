#!/usr/bin/env bash
# One framework of the macOS wrap acceptance: scaffold, install deps, build
# the .app, run the installed wrap self-test. Run from the repo root, after
# the workspace packages are built:
#
#   CARGO_TARGET_DIR=<dir> scripts/wrap-acceptance/run-macos.sh <command|gradio|streamlit>
#
# ci.yml's `wrap-acceptance-macos` job runs this three times in sequence on
# ONE macOS runner (macOS runners are the scarce ones), so the Rust target dir
# is shared across the three builds through CARGO_TARGET_DIR: tauri and its
# dependencies compile once. The Linux and Windows legs of `wrap-acceptance`
# keep one job per framework.
set -euo pipefail

FW="${1:?usage: run-macos.sh <command|gradio|streamlit>}"
: "${CARGO_TARGET_DIR:?CARGO_TARGET_DIR must name the shared Rust target dir}"
export APP_DIR=wrap-app APP_BIN=wrap-app
# Gradio embeds the page title in the config `GET /` serves; FastAPI serves the
# marker itself; Streamlit renders over a websocket, so its static shell
# (title "Streamlit") is what a GET can see.
MARKER=PYLAIR-WRAP-OK
[ "$FW" = streamlit ] && MARKER=Streamlit
export MARKER

echo "::group::[$FW] scaffold and prepare"
rm -rf "$APP_DIR" "$CARGO_TARGET_DIR/release/bundle"
node packages/create-pylair/dist/index.js "$APP_DIR" --mode wrap --local .
# The app lives inside this checkout; without its own workspace root pnpm
# installs the monorepo instead of the app.
echo "packages: []" > "$APP_DIR/pnpm-workspace.yaml"
node scripts/wrap-acceptance/prepare.mjs "$APP_DIR" "$FW"
echo "::endgroup::"

echo "::group::[$FW] install app deps and pylair build"
(
  cd "$APP_DIR"
  pnpm install --no-frozen-lockfile
  uv sync
  pnpm exec pylair build --bundles app
)
echo "::endgroup::"

echo "[$FW] installed wrap self-test"
APP="$(find "$CARGO_TARGET_DIR/release/bundle/macos" -name '*.app' | head -1)"
[ -n "$APP" ] || { echo "no .app under $CARGO_TARGET_DIR/release/bundle/macos" >&2; exit 1; }
LOG="$(mktemp)"
TRIGGER="$(mktemp)"
# A clean environment, run from outside the checkout (as the bridge leg
# does): no Python on PATH, nothing of the project visible.
cd /tmp
env -i HOME="$HOME" PATH=/usr/bin:/bin \
  PYLAIR_SELF_TEST_WRAP="$MARKER" PYLAIR_SELF_TEST_WRAP_RESTART=1 PYLAIR_SELF_TEST_WRAP_CRASH=1 \
  PYLAIR_RESTART_TRIGGER="$TRIGGER" \
  perl -e 'alarm 600; exec @ARGV' "$APP/Contents/MacOS/$APP_BIN" 2>&1 | tee "$LOG"
grep -q "wrap-self-test: ok" "$LOG" || { echo "[$FW] no 'wrap-self-test: ok' verdict" >&2; exit 1; }
echo "[$FW] ok"
