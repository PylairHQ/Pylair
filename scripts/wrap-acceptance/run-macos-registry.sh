#!/usr/bin/env bash
# One framework of the registry-acceptance macOS wrap run: scaffold the wrap
# app (packed create-pylair tarball in pack mode, `create-pylair@next` in
# registry mode), install deps, optionally assert expect_version, build the
# .app and run the installed wrap self-test. Run from the repo root. In pack
# mode ./packs must already hold the npm-pack tarballs:
#
#   SOURCE=pack|registry EXPECT=<version|empty> CARGO_TARGET_DIR=<dir> \
#     scripts/wrap-acceptance/run-macos-registry.sh <command|gradio|streamlit>
#
# acceptance-registry.yml's `wrap-acceptance-macos` job runs this three times
# in sequence on ONE macOS runner — the per-framework legs starved on the
# hosted arm64 pool (acquisition cancelled twice in one day), the same reason
# ci.yml consolidated first. The Rust target dir is shared through
# CARGO_TARGET_DIR: tauri and its dependencies compile once. The Linux and
# Windows legs of `wrap-acceptance` keep one job per framework.
set -euo pipefail

FW="${1:?usage: run-macos-registry.sh <command|gradio|streamlit>}"
: "${SOURCE:?SOURCE must be pack or registry}"
: "${CARGO_TARGET_DIR:?CARGO_TARGET_DIR must name the shared Rust target dir}"
export APP_DIR=wrap-app APP_BIN=wrap-app
# Gradio embeds the page title in the config `GET /` serves; FastAPI serves the
# marker itself; Streamlit renders over a websocket, so its static shell
# (title "Streamlit") is what a GET can see.
MARKER=PYLAIR-WRAP-OK
[ "$FW" = streamlit ] && MARKER=Streamlit
export MARKER

echo "::group::[$FW] scaffold ($SOURCE) and prepare"
rm -rf "$APP_DIR" "$CARGO_TARGET_DIR/release/bundle"
if [ "$SOURCE" = pack ]; then
  rm -rf cp-src
  mkdir -p cp-src
  # Plain tar: all paths are relative so there's no colon for GNU tar to
  # misparse, and --force-local doesn't exist on macOS's BSD tar.
  tar -xzf packs/create-pylair-*.tgz -C cp-src
  node cp-src/package/dist/index.js "$APP_DIR" --mode wrap
else
  npx -y create-pylair@next "$APP_DIR" --mode wrap
fi
node scripts/wrap-acceptance/prepare.mjs "$APP_DIR" "$FW"
echo "::endgroup::"

echo "::group::[$FW] install app deps"
(
  cd "$APP_DIR"
  if [ "$SOURCE" = pack ]; then
    # Tarball FIRST: the scaffolded @pylair/cli pin points at a version that
    # isn't on the registry yet, so npm install 404s.
    npm install ../packs/pylair-cli-*.tgz
  fi
  npm install
  uv sync
)
echo "::endgroup::"

if [ -n "${EXPECT:-}" ]; then
  echo "::group::[$FW] assert resolved package versions match expect_version"
  (
    cd "$APP_DIR"
    # Input reaches the shell via env only, never expression syntax inside
    # run: (script-injection surface). Validate shape once.
    if [[ "$EXPECT" == *$'\n'* || "$EXPECT" == *$'\r'* ]] || \
       [[ ! "$EXPECT" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]]; then
      echo "invalid expect_version input: '$EXPECT'"
      exit 1
    fi
    fail=0
    check() { # $1 label, $2 resolved version
      if [ "$2" != "$EXPECT" ]; then
        echo "::error::$1 resolved to '$2' but expect_version is '$EXPECT' - the registry served something else"
        fail=1
      else
        echo "$1 resolved to $2 as expected"
      fi
    }
    check "@pylair/cli" "$(node -p "require('./node_modules/@pylair/cli/package.json').version")"
    # create-pylair is the scaffolder, not an app dep; pack mode keeps the
    # unpacked tarball under cp-src, registry mode asks npm what the @next
    # tag resolved to.
    if [ "$SOURCE" = pack ]; then
      check "create-pylair" "$(node -p "require('../cp-src/package/package.json').version")"
    else
      check "create-pylair" "$(npm view create-pylair@next version)"
    fi
    exit $fail
  )
  echo "::endgroup::"
fi

echo "::group::[$FW] pylair build"
( cd "$APP_DIR" && npx pylair build --bundles app )
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
