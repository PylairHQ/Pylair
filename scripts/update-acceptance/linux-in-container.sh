#!/usr/bin/env bash
# Update acceptance — Linux container entry (decision 0011, PR 4).
# Runs INSIDE the clean ubuntu:22.04 container: xvfb for the GUI apps,
# a shared X server (per-launch xvfb-run dies under relaunches — the
# watchdog's restore-spawn and app.restart() are new processes outside
# the wrapper), then the POSIX scenario runner. NO system webkit is
# installed: the AppImage's bundled libs are the whole claim the
# Linux watchdog-spawn arm makes (the cached artifact runs the
# watchdog with its own libs).
#
# Expects: /work mounted (WORK), /work/htdocs (served tree), and this
# script's dir mounted read-only at $SCRIPTS (default /scripts).

set -eo pipefail
export DEBIAN_FRONTEND=noninteractive

sed -i 's|http://archive.ubuntu.com|http://azure.archive.ubuntu.com|g; s|http://security.ubuntu.com|http://azure.archive.ubuntu.com|g' /etc/apt/sources.list
for _ in 1 2 3; do
  apt-get -o Acquire::Retries=3 -o Acquire::http::Timeout=30 update -qq && break || sleep 10
done
# xvfb + python3 (http.server) + the host baseline an AppImage never
# bundles by convention (X/GL/font plumbing — linuxdeploy excludes it;
# the bundled WebKitGTK stack is still the claim under test). The list
# is exactly the sonames readelf shows as NEEDED-but-not-bundled.
apt-get -o Acquire::Retries=3 -o Acquire::http::Timeout=30 install -y -qq \
  xvfb python3 ca-certificates curl \
  libfontconfig1 libfreetype6 libharfbuzz0b libfribidi0 libexpat1 \
  libgmp10 libgpg-error0 libcom-err2 \
  libx11-6 libx11-xcb1 libxcb1 libwayland-client0 \
  libegl1 libgl1 libgl1-mesa-dri libdrm2 libgbm1 libgles2 \
  fonts-dejavu-core >/dev/null

export HOME=/work/home
mkdir -p "$HOME"

# One shared X server for the entire run. Fail loudly if it did not
# come up — a dead DISPLAY silently produces the "app writes nothing"
# failure mode that once burned the whole 90-minute job budget.
Xvfb :99 -screen 0 1280x800x24 &
XVFB_PID=$!
export DISPLAY=:99
sleep 1
kill -0 "$XVFB_PID" 2>/dev/null || { echo "FATAL: Xvfb :99 failed to start"; exit 2; }

# AppImage extract timings for the PR body — each launch pays a
# re-extract under APPIMAGE_EXTRACT_AND_RUN=1. Bounded: a stuck
# extractor must not stall the run.
APPIMAGE=$(find /work/htdocs/installable/v1.0.0 -name '*.AppImage' | head -1)
if [ -n "$APPIMAGE" ]; then
  echo "── v1 AppImage extract timing"
  timeout 120 /usr/bin/time -v "$APPIMAGE" --appimage-extract >/dev/null 2>/work/extract-time.txt || \
    { time timeout 120 "$APPIMAGE" --appimage-extract >/dev/null; } 2>>/work/extract-time.txt
  rm -rf squashfs-root
fi

export WORK=/work HTDOCS=/work/htdocs
exec bash "${SCRIPTS:-/scripts}/update-acceptance/scenarios-posix.sh" "$@"
