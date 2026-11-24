# update-acceptance — end-to-end auto-update acceptance harness

The PR-4 acceptance legs for decision 0011. `.github/workflows/
acceptance-update.yml` drives this directory: it scaffolds a real
`create-pylair --with-updater --local .` app, builds five version
variants with `pylair build --sign`, serves their manifests over
loopback http, and runs real install/update/rollback scenarios on each
OS — asserting from the `updater-state/` file contract plus the app's
own update surface (`pylair.update.state()`,
`pylair.update.on_update_confirmed`, `health().update`), never just logs.

## Layout

- `fixtures/App.tsx` — the webview driver that replaces the scaffold's
  demo UI: waits for runtime ready, records `health().update`, runs
  `update.check()` → `downloadAndInstall()` → `relaunch()` — the user
  path. Every step lands in `<app-data>/accept/log.jsonl` via the
  backend's `accept_record`.
- `fixtures/backend.py.tmpl` — the recorder backend every variant
  carries: `accept_record`/`accept_version` exposes, the
  `pylair.update.state()` transition watcher, and a `@@VARIANT_BODY@@`
  slot `build-version.mjs` fills per build.
- `fixtures/crash-hook.rs` — inserted into `lib.rs` for the `crash`
  variant: the app exits ~1.5 s after boot, so the watchdog sees boot
  markers and nothing else.
- `patch-conf.mjs` — the ONLY CI-allowed product deviation: asserts the
  scaffold still ships https placeholders, then patches
  endpoints/rollbackUrl to `http://127.0.0.1:<port>`, sets
  `dangerousInsecureTransportProtocol`, the short watchdog knobs
  (`plugins.pylair.update`), the build version, and
  `bundle.windows.nsis.installMode` for per-machine variants.
- `build-version.mjs` — one variant build: conf patch → backend fixture
  → `pylair build --sign` → stage `htdocs/releases/<tag>/` (artifact +
  `.sig` + `latest.json`) and `htdocs/installable/<tag>/`. On macOS it
  also re-signs the `.app` with the hardened runtime, repacks the
  updater tarball from it, and re-signs the tarball.
- `scenarios-posix.sh` — scenarios a–e for Linux (in-container) and
  macOS (host).
- `linux-in-container.sh` — the clean `ubuntu:22.04` entry: xvfb +
  python3 only (deliberately no system webkit — the AppImage's own libs
  are the Linux watchdog-spawn claim), one shared `Xvfb`, then the
  POSIX runner.
- `scenarios-windows.ps1` — scenarios a–e plus f, the per-machine
  `rollback-pending-approval` leg.

## Scenarios

| # | Flow | Asserts |
|---|------|---------|
| a | v1 → v2 | offer→install→relaunch; `update.state` sequence `none→pending→confirmed`; `on_update_confirmed` exactly once; `update-ok` written; `health().update.state == "ready"` |
| b | v1 → v2-slow | ~45 s of component progress outlives `heartbeatMs` (10 s) → still confirmed; `progress.json` advances mid-window |
| c | v2 → v3 (broken `on_startup`) | `readiness_failed` rollback; v2 restored + relaunched; `lastRollback` in `health().update`; v3 in `skipped.json`; restored v2's fresh check offers nothing |
| d | v2 → v3-crash | exits before ready × `maxBoots` → `crash_loop` rollback + restore |
| e | v2 → v3, cache corrupted after arming | `verify_failed` — the watchdog refuses to restore; v3 stays installed; `health().update.lastRollback` reports it |
| f | v2pm → v3pm per-machine (Windows) | `rollback-pending-approval` recorded; no silent restore — v3pm is still the installed binary |

Versions: 1.0.0 (v1), 2.0.0 (v2), 2.1.0 (v2-slow), 3.0.0 (v3),
4.0.0 (v3-crash); Windows adds 8.0.0/9.0.0 per-machine. Each release's
`latest.json` lives at `releases/<tag>/latest.json` so the scaffold's
`v{{version}}` rollbackUrl resolves exactly like GitHub Releases.

## Running locally (Windows)

```ps1
node packages/create-pylair/dist/index.js app-under-test --with-updater --local . --identifier com.pylair.acceptupd
echo "packages: []" > app-under-test/pnpm-workspace.yaml
cd app-under-test; pnpm install --no-frozen-lockfile; uv sync; cd ..
pnpm --dir app-under-test exec pylair keys init --key-path "$env:TEMP\upd.key" --no-password
$env:TAURI_SIGNING_PRIVATE_KEY = Get-Content "$env:TEMP\upd.key" -Raw
mkdir htdocs/{latest,releases,installable}
node scripts/update-acceptance/build-version.mjs --app app-under-test --variant base --version 1.0.0 --tag v1 --bundles nsis --out htdocs
pwsh -File scripts/update-acceptance/scenarios-windows.ps1 -Work .\accept-work -Htdocs .\htdocs -Scenarios a
```
