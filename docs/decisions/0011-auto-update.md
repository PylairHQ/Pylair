# 0011: Auto-update for Pylair apps (bundle updates + readiness-gated rollback)

Status: proposed (spike: `spikes/u0-updater` on the `u0-updater` branch;
CI evidence in §Evidence index)

## Question

Pylair apps ship as installers (NSIS / `.app` / AppImage) built by
`pylair build`. Once installed they are frozen: there is no supported
path to deliver a new version short of asking users to reinstall. This
decision designs auto-update on top of `tauri-plugin-updater` — the
same plugin the Tauri ecosystem uses — plus the Pylair-specific piece
upstream lacks: **readiness-gated rollback**. A Pylair app's "is the new
version actually working" signal already exists (app-defined readiness,
decision 0007); the update design here spends it on automatically
restoring the previous version when a new build cannot reach ready.

Eight questions, answered with spike evidence below.

## Evidence index

- Throwaway workflow `u0-updater.yml` (removed before merge):
  - **Run `45347893012` — all three legs green** (ubuntu-22.04,
    windows-latest, macos-latest): v1→v2 update + marker asserts,
    v2→v3(broken)→v2 readiness-gated rollback, skip-list re-offer
    suppression — `PASS` on every check.
    <https://github.com/PylairHQ/Pylair/actions/runs/45347893012>
  - Iteration runs with instructive failures: `45338629835`
    (exec bits / endpoint scheme), `45340587621` (conf-vs-Cargo
    version divergence, headless display), `45344186015` (NSIS passive
    mode + AppImage re-extract cost).
- Artifact sizes / timings: §1 table (from the run's step output).
- Spike tree: `spikes/u0-updater/{app,serve.py,build.sh,ci.sh,README.md}`.

## 1. Full-bundle updates with tauri-plugin-updater

**What the spike proves.** `app/` is a scaffold-shaped Tauri v2 app:
`backend/` and `python/` are bundled as resources under the same
`resources/{backend,python}` layout the create-pylair template uses, so
an artifact diff exercises the same payload classes a real Pylair app
ships (bundled interpreter payload + backend code + frontend). v1 and
v2 differ in `backend/app.py`'s `VERSION` and
`python/pylair-runtime.txt`; the app dumps both to the state dir on
every boot. A green `assert_marker 0.2.0` after update means the
*installed* bundle — Rust binary, webview assets, bundled backend and
Python payload — was replaced as a unit.

- **Keys**: `tauri signer generate -w key -p ""` produces a minisign
  keypair; pubkey goes into `plugins.updater.pubkey` in
  `tauri.conf.json`, private key into `TAURI_SIGNING_PRIVATE_KEY` at
  build time. Throwaway per-run keys in CI; nothing committed.
- **Artifacts**: `bundle.createUpdaterArtifacts: true` gives
  `*.AppImage` + `.sig` (linux), `*.app.tar.gz` + `.sig` (macOS),
  `*-setup.exe` + `.sig` (Windows NSIS). The spike builds only those
  targets (`--bundles appimage|app|nsis`).
- **Manifest**: `latest.json` `{version, notes, pub_date, platforms:
  {target: {signature, url}}}`; the spike's `serve.py` also emits a
  `sha256` per platform (extension field — the update plugin ignores
  unknown keys; the spike uses it as the skip-list key).
- **Flow**: `app.updater().check()` → `Update` →
  `download_and_install()` → `handle.restart()`. The spike drives it
  from Rust (`PYLAIR_UPDATE_TEST=1`), no JS dependency needed.

**Findings per OS** (run `45347893012`, all legs green):

| leg | artifact | size | builds (v1/v2/v3) | update+ready | rollback wall |
|---|---|---|---|---|---|
| ubuntu-22.04 | `.AppImage` + `.sig` | **80M** | 294s/114s/113s | **67s** | **78s** |
| windows-latest | `-setup.exe` + `.sig` | **3.2M** | 266s/51s/50s | **10s** | **47s** |
| macos-latest | `.app.tar.gz` + `.sig` | **5.0M** | 199s/27s/31s | **6s** | **47s** |

Rollback wall time is dominated by the 45s readiness deadline
(`PYLAIR_ROLLBACK_DEADLINE_MS`, env-tunable) plus restore+relaunch:
~2s on macOS/Windows, ~31s on Linux (an 80M AppImage re-extracts the
restored copy before `main` runs). A real app's deadline should cover
its slowest plausible readiness, e.g. model load.

**Integration findings the run surfaced** (each cost an iteration):

- `plugins.updater.endpoints` **must be https** — plain `http://` is
  rejected at plugin init unless
  `plugins.updater.dangerousInsecureTransportProtocol: true`. That flag
  is correct for a local dev/CI server only; the developer surface in
  §7 keeps https mandatory for shipped apps.
- The updater compares the remote version against **`tauri.conf.json`'s
  `version`** (baked into the bundle), not `Cargo.toml` — the spike's
  own markers read `env!("CARGO_PKG_VERSION")` and lied until both were
  patched. Scaffold builds must keep the two in sync.
- `bundle.createUpdaterArtifacts: true` is required or the signed
  updater payloads (`.sig`) are not produced.
- **Windows**: `download_and_install` never returns on Windows — it
  ShellExecutes `-setup.exe /S /UPDATE /R` then `exit(0)` inside the
  call. Any log/watchdog wiring must be armed *before* the call.
  Default `installMode: passive` shows an NSIS progress window that can
  hang on agent/headless sessions — `installMode: "quiet"` (`/S`) is
  the right default for unattended updates; relaunch happens via the
  installer's `/R` flag (`restart_after_install` defaults true).
- **Windows relaunch loses the environment**: the NSIS installer
  relaunches the exe with the desktop/explorer environment — env-var
  state does not survive. Anything the relaunched process needs must
  travel in argv (the updater forwards the app's own args via `/ARGS`)
  or in files; the spike carries `--state-dir` for exactly this reason.
- **Linux AppImage**: the artifact *is* the runnable file; the updater
  rewrites it at the `APPIMAGE` path and `restart()` re-execs. Under
  `APPIMAGE_EXTRACT_AND_RUN=1` `current_exe` is the extracted mount —
  rollback bookkeeping must record `APPIMAGE`, not `current_exe`.
  Restoring = overwrite + chmod; the ~30s cold re-extract is the
  dominating cost on this platform.
- **macOS**: `.app.tar.gz` replaces the `.app` in place at its current
  path; `restart()` re-runs the binary inside the new bundle. Unsigned
  dev-built `.app`s work locally (no quarantine attr); shipped builds
  need real signing + notarization regardless.

## 2. Readiness-gated rollback

Tauri keeps no previous version — once install runs, the old build is
gone. The spike prototypes the missing half:

```
check() ok ──▶ fetch rollback-<from>.json, download current-version
               artifact into a cache dir OUTSIDE the install dir
          ──▶ write pending.json {from, to, to_sha256, artifact,
               sha256, sig, install_path, deadline}
          ──▶ spawn watchdog (detached) ──▶ install + restart
watchdog:  wait for ready-<to> within deadline
           ├─ ready           → update-ok, clear pending, exit
           ├─ ≥MAX_BOOTS boots without ready → rollback("crash_loop")
           └─ deadline        → rollback("deadline")
rollback:  re-verify cached artifact (sha256 + recorded sig; production:
           minisign verify vs embedded pubkey) → add {version,sha256} to
           skipped.json → restore artifact per OS → relaunch → record
           rollback-reason.txt
```

Two details the spike enforces that a real implementation must keep:

- **Skip-list write precedes relaunch.** The restored app re-checks on
  boot; without the entry already on disk it would be re-offered the
  build that just failed → infinite update loop. (`upwatch.rs` writes
  `skipped.json` before spawning the restored binary.)
- **The watchdog must outlive the app.** It lives outside the install
  dir and is spawned detached; on Windows (where install runs with the
  app closed) it is the *only* component that can observe the new
  version's first boot. State dir = app-data dir (`updater-state/`);
  the actor = the watchdog process itself — not the installer, not the
  next boot of the app (by then the verdict may already be "dead").

**Proof** (run `45347893012`): v3 carries `BROKEN = True` in
`backend/app.py`; the app exits(3) before writing `ready-0.3.0`. The
watchdog's deadline fires, it re-verifies the cached v2 artifact,
restores it (per-OS path above), writes `restored 0.2.0` into
`rollback-reason.txt`, and a subsequent check logs
`skip: 0.3.0 on skip list`. Observed end-to-end on all three legs:
v3 boot → crash → deadline → verify → skip-list → restore →
`boot v0.2.0` → `ready v0.2.0` — and the restored build re-checked,
found the same offer, and skipped it.

## 3. Downgrade mechanics

- **Comparator**: upstream `check()` only surfaces `version > current`;
  the rollback path never calls `check` — it reinstalls a cached
  artifact the watchdog already trusts. The only comparator needed is
  *for the skip list and manifest*: exact `{version, artifact-hash}`
  identity (the spike keys on `version` + manifest `sha256`; the `.sig`
  blob is equally good identity). No semver arithmetic beyond
  upstream's own `semver.gt` for the forward direction.
- **Re-offer loop**: prevented by `skipped.json` ([{version, sha256}])
  in the app-data dir, consulted before `download_and_install`. A
  "skip this version" user action can reuse the same mechanism; the
  file is per-user state that survives reinstalls.

## 4. Data safety

What a new bundle can change that the old one can't read:

- **Model-store sidecars** (`spikes/m9-models`, decision 0009): a new
  schema version for model manifests/pins — old build can't parse.
- **App data dir migrations**: anything `on_startup` rewrites —
  settings schema, cache layout, SQLite files.
- **Warm-worker/session state**: transient, low risk, but a mid-update
  generation is killed with its pool.

**Proposed rule**: *migrations run only after the update is confirmed
ready — and must be backwards-compatible while pending.* Concretely:

1. During `pending` (watchdog armed), the new build may read but must
   not write any migrated state; `on_startup` migrations wait for a
   `ready`-ack from the update machinery (or run on second boot after
   `update-ok`).
2. A new version that *must* migrate writes to a versioned copy (e.g.
   `settings.v2.json`), leaving `settings.v1.json` intact for the
   rollback target.
3. Pylair surfaces this as: (a) a scaffold rule in docs + template
   comments ("migrations belong behind `pylair.onReady`, not import
   time"), and (b) `pylair doctor`/selftest flagging app-data writes
   that happen before first `ready` under an update marker. Enforcement
   is advisory for v1 — the mechanical guarantee is §2.

## 5. Code-only updates (Python without reinstall) — assessment

Not prototyped. Findings:

- A `.py` payload inside a signed macOS `.app` cannot be swapped without
  breaking code signature — code-only updates need the Python payload
  moved *outside* the bundle (app-data dir) and signed separately.
- That means: a second signature scheme (same minisign keys can sign
  zip/tar payloads), a downloader + verifier in the supervisor, payload
  versioning, and rollback coupling (a bad `.py` payload must roll back
  the payload, possibly under an unchanged host — the same watchdog
  pattern applies at payload granularity).
- Notarization: out-of-bundle payloads escape Gatekeeper entirely —
  which is precisely the security property to preserve via mandatory
  signature verification.
- **Recommendation: later.** Ship bundle updates + rollback first (they
  update everything uniformly); code-only payloads become interesting
  when backend sizes make full-bundle downloads wasteful, and the
  machinery (skip list, readiness gate, signature check) is reusable
  by then. Never recommend payload-only as a substitute: without the
  readiness gate it's strictly less safe than bundle update.

## 6. Keys and hosting

- **Generation/storage**: `tauri signer generate` (minisign). Private
  key lives in a CI secret (`TAURI_SIGNING_PRIVATE_KEY` +
  `_PASSWORD`); pubkey is embedded in the shipped `tauri.conf.json`.
- **Cost of losing the private key**: all future updates are rejected —
  clients pin the pubkey. Recovery = ship a new installer whose
  embedded pubkey is new *signed by the old key* — impossible if the
  key is gone, so: users must reinstall manually. Losing key ⇒ reinstall
  campaign; mitigations: HSM/KMS storage, offline backup, split-k
  custody.
- **Rotation**: supported path is "update signed by old key embeds new
  pubkey" — clients accept the new key after that update. Requires the
  old key still alive. No in-band revocation; treat the private key as
  long-lived and precious.
- **Hosting**: static `latest.json` on GitHub Releases is sufficient —
  one asset per release (`latest.json`) + the artifact + `.sig` files,
  endpoint `…/releases/latest/download/latest.json`. A dynamic endpoint
  is only needed for staged rollouts or per-user channels; keep static
  until then. **Channels**: separate manifest files per channel
  (`latest-stable.json`, `latest-beta.json`) and a scaffold knob for
  which endpoint ships — cheap and reviewable.

## 7. Developer surface

- **CLI** (`packages/cli`, new commands):
  - `pylair keys init` — wraps `tauri signer generate`, writes pubkey
    into the app's `tauri.conf.json`, prints private-key storage
    instructions.
  - `pylair update manifest` — scans `target/release/bundle` for signed
    artifacts, emits `latest.json` (+ optional `--channel`).
  - `pylair build --sign` — wires `TAURI_SIGNING_PRIVATE_KEY` env +
    `createUpdaterArtifacts`.
- **Client API** (`@pylair/client`):
  `pylair.update.check() -> {version, notes} | null`,
  `update.downloadAndInstall({onEvent})` — progress events mirror
  upstream's (`Started {contentLength}`, `Progress {chunkLength}`,
  `Finished`), then `update.relaunch()` / `update.installAndRelaunch()`.
- **Health/readiness**: new readiness component `updating`
  (`pending|installing|ready|rolled-back`) so apps can render "update
  pending verification"; watchdog verdicts land in `health()` under
  `update: {state, lastRollback?}`.
- **Scaffold opt-in**: `create-pylair --with-updater` (or template
  question) injects the plugin dep, `plugins.updater` conf block,
  capability permission, and a generated `latest.json` endpoint TODO.
  Default stays off — unsigned dev apps shouldn't self-update.
- **Security section** (docs + README):
  - Signature verification is mandatory and upstream-enforced (no
    bypass flag; the spike's `.sig` check is the whole trust anchor).
  - `https` required outside dev; the spike's `http://127.0.0.1` is
    dev-only and stays behind the env override.
  - Compromised endpoint ≠ compromised clients: worst case is *no
    update* or a *valid* update withheld — an attacker cannot push an
    unsigned/forged bundle. The private key is the single point of
    compromise; protect per §6.
  - Rollback re-verifies the cached artifact (minisign vs embedded
    pubkey) before reinstalling — a tampered cache never executes.

## 8. Implementation split + acceptance plan

PRs, in order:

1. **Spike merge** (this PR): doc + `spikes/u0-updater`.
2. **CLI + scaffold**: `pylair keys init`, `pylair update manifest`,
   `--with-updater` template option, docs/security.md section.
3. **Client + plugin**: `pylair.update` API surface, `updating`
   readiness component, `update` field in `health()`.
4. **Watchdog**: supervisor-side watchdog binary (Rust, shipped in the
   bundle like the spike's `upwatch`), pending-state protocol,
   per-OS restore paths.
5. **Acceptance**: new `acceptance-update.yml` legs — build v1/v2/v3
   signed with CI throwaway keys, serve manifest locally, assert
   update + rollback + skip-list on ubuntu + windows; macOS as a step
   inside an existing macOS job (capacity rule) once proven.

Runtime cost estimate (measured, run `45347893012`): per leg ≈
`3 × tauri build` + proofs ≈ **~9min ubuntu** (294+114+113s builds,
167s proofs), **~7min windows** (266+51+50s builds, ~2min proofs),
**~5min macOS** (199+27+31s builds, ~2min proofs) — dominated by the
cold first build; warm builds are ~¼ the time because deps compile
once in a shared `target/`. An acceptance job per OS at these numbers
is affordable; on macOS it belongs as a step inside an existing macOS
job (capacity rule). A real Pylair app additionally builds the bundled
CPython + backend venv, roughly doubling build time but not the
update/rollback mechanics.

## Alternatives considered

- **Tauri's built-in updater alone, no rollback**: rejected — Pylair
  apps can ship backend changes that fail `on_startup`; without §2 a
  bad release bricks installs until manual reinstall.
- **Electron-style delta updates (Squirrel/nuget)**: heavier infra,
  Tauri has no first-class delta path for NSIS/AppImage; static
  full-bundle artifacts + signing are simpler and sufficient at Pylair
  app sizes (see §1 table).
- **MSIX/Store distribution**: orthogonal; the updater model here is
  for direct-download installs (the scaffold's default).
