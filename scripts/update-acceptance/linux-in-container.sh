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
for i in 1 2 3; do
  apt-get -o Acquire::Retries=3 -o Acquire::http::Timeout=30 update -qq && break || sleep 10
done
# xvfb + python3 (http.server) only — deliberately no libwebkit2gtk.
apt-get -o Acquire::Retries=3 -o Acquire::http::Timeout=30 install -y -qq \
  xvfb python3 ca-certificates curl >/dev/null

export HOME=/work/home
mkdir -p "$HOME"

# One shared X server for the entire run.
Xvfb :99 -screen 0 1280x800x24 &
export DISPLAY=:99
sleep 1

# AppImage extract timings for the PR body — each launch pays a
# re-extract under APPIMAGE_EXTRACT_AND_RUN=1.
APPIMAGE=$(find /work/htdocs/installable/v1.0.0 -name '*.AppImage' | head -1)
if [ -n "$APPIMAGE" ]; then
  echo "── v1 AppImage extract timing"
  /usr/bin/time -v "$APPIMAGE" --appimage-extract >/dev/null 2>/work/extract-time.txt || \
    { time "$APPIMAGE" --appimage-extract >/dev/null; } 2>>/work/extract-time.txt
  rm -rf squashfs-root
fi

export WORK=/work HTDOCS=/work/htdocs
exec bash "${SCRIPTS:-/scripts}/update-acceptance/scenarios-posix.sh" "$@"
