# spikes/u0-updater — auto-update + readiness-gated rollback

Spike for `docs/decisions/0011-auto-update.md`. Evidence runs in the
throwaway `u0-updater.yml` workflow (removed before merge; run links in
the decision doc).

## Shape

- `app/` — a scaffold-shaped Tauri v2 app: `backend/` + `python/`
  bundled resources (same `resources` layout as the create-pylair
  template), `tauri-plugin-updater`, a static webview, and a self-test
  update flow in `main.rs` driven by env vars.
- `app/src-tauri/src/bin/upwatch.rs` — the rollback watchdog: spawned
  detached before install, waits for `ready-<ver>` within the deadline,
  else re-verifies the cached old artifact and restores it per OS, adds
  the failed build to `skipped.json`, relaunches, records the reason.
- `build.sh <ver> [--broken]` — patches version/markers, injects the
  throwaway pubkey, `tauri build`, collects signed artifacts to `dist/`.
- `serve.py` — local manifest server: `latest.json` + `rollback-<v>.json`
  generated per request from `dist/`, artifacts + `.sig` served inline,
  plus a `sha256` extension field (skip-list key).
- `ci.sh` — the whole proof: keys → 3 builds → serve → v1→v2 →
  v2→v3(broken)→rollback→skip-list assert. Run once per OS in CI.

## Env contract (spike only)

| var | meaning |
|---|---|
| `PYLAIR_STATE_DIR` | where markers/pending/cache/logs live |
| `PYLAIR_UPDATE_TEST=1` | run the check→install flow on boot |
| `PYLAIR_UPDATE_ENDPOINT` | override manifest URL |
| `PYLAIR_READY_DELAY_MS` | simulates app load time before ready |
| `PYLAIR_EXIT_AFTER_READY_MS` | self-exit so CI owns lifecycle |
| `PYLAIR_ROLLBACK_DEADLINE_MS` | watchdog readiness deadline |
| `PYLAIR_MAX_BOOTS` | crash-loop threshold |
| `PYLAIR_WATCHDOG_BIN` | path to compiled `upwatch` |
| `TAURI_SIGNING_PRIVATE_KEY(_PASSWORD)` | throwaway signing key |

`--broken` builds carry `BROKEN = True` in `backend/app.py`; the app
exits(3) before ready — the deliberately-bad v3.
