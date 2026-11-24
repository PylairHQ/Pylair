# 0011: Auto-update for Pylair apps (bundle updates + readiness-gated rollback)

Status: accepted (spike: `spikes/u0-updater` on the `u0-updater` branch;
CI evidence in §Evidence index). Implementation lands after 0.1.0.

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

**Fidelity caveat — sizes understate a real app.** The spike's `python/`
is a marker file, not a bundled CPython. Scaling by the 0001 m0 numbers
(scaffolded app ≈ 69 MiB interpreter + ~28 MiB env inside the bundle):

| leg | spike artifact | real scaffolded-app estimate | torch-class app |
|---|---|---|---|
| ubuntu | 80M | **~120-140M** | ~300M+ |
| windows | 3.2M | **~50-60M** (≈ 0001's 47.8M dmg class) | ~300M |
| macOS | 5.0M | **~50-60M** | ~300M |

Download time scales linearly with size (negligible on the spike's local
server; on WAN at ~10MB/s a 60M artifact is ~6s, 300M is ~30s). The
AppImage restore cost in §2 scales the same way — re-extract of a
restored ~130M AppImage ≈ 45-60s, not 31s.

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
- **macOS App Translocation**: a quarantined app launched from a DMG or
  before being moved to a permanent location runs translocated at
  `/private/var/folders/…/AppTranslocation/<uuid>/<App>.app` on a
  read-only mount — an in-place update cannot write there. The update
  path must detect it (current exe path under `AppTranslocation/`) and
  refuse with a clear message ("move the app to /Applications (or any
  owned location) and retry"), surfaced as a distinct
  `UpdateError::NotUpdatable` on the client API — not a generic failure.

## 2. Readiness-gated rollback

Tauri keeps no previous version — once install runs, the old build is
gone. The spike prototypes the missing half:

```
check() ok ──▶ fetch rollback-<from>.json, download current-version
               artifact into a cache dir OUTSIDE the install dir
          ──▶ write pending.json {from, to, to_sha256, artifact,
               sha256, sig, install_path, session_id, knobs}
          ──▶ spawn watchdog (detached) ──▶ install + restart
watchdog:  wait for ready-<to, session_id> (see "the real signal")
           ├─ ready              → update-ok, clear pending, exit
           ├─ readiness failed   → rollback("readiness_failed")
           ├─ ≥max_boots boots without ready → rollback("crash_loop")
           ├─ silence > heartbeat_ms with no progress → rollback("no_sign_of_life")
           └─ past hard_cap_ms   → rollback("hard_cap")
rollback:  re-verify cached artifact (sha256 + recorded sig; production:
           minisign verify vs embedded pubkey) → add {version,sha256} to
           skipped.json → restore artifact per OS → relaunch → record
           rollback-reason.txt
```

**Deadline semantics — never a fixed wall clock.** A fixed deadline
rolls back healthy updates whose first boot legitimately takes minutes
(a model download, a big `on_startup` load). The production watchdog
rolls back on *evidence of failure*, not elapsed time:

| knob | default | meaning |
|---|---|---|
| `heartbeat_ms` | 60_000 | max silence: no `ready`, no `failed`, and no readiness-progress event for this long ⇒ dead |
| `max_boots` | 3 | boot markers for the pending version without a ready ⇒ crash loop |
| `hard_cap_ms` | 600_000 | absolute ceiling for the pending window, **app-set** in the update config (this is the only wall clock; generous defaults — a model download on a slow link must fit) |

While the app's readiness surface reports *live* component progress
(`health().app.components` advancing, 0007), the window stays open until
`hard_cap_ms`. Silence (`heartbeat_ms`) or an explicit `failed` is what
kills — a stuck load and a dead start are distinguishable states, and
the watchdog treats only the dead one as fatal.

**The real readiness signal.** The spike app writes `ready-<ver>`
itself. In production the **host** writes it — the same signal the
webview already sees: when `health().app` reaches `ready` (0007) the
host records `ready-<ver>` in the update state dir containing
`{version, artifact_sha256, session_id}`. `session_id` is a per-pending
nonce the app generates when arming `pending.json`; the watchdog accepts
a ready marker only when **version AND session** match — a stale
`ready-0.2.0` left by an earlier generation can never satisfy a pending
`0.3.0` window, and the marker's `artifact_sha256` is checked against
the manifest value the app saw at check time.

**Trust anchor.** The watchdog's verification key is **compiled into
the watchdog binary** (`include_bytes!`/const — written by the build
the same way `pubkey` enters `tauri.conf.json`), never read from the
state dir, which is attacker-influenceable app data. Likewise
`install_path`: `pending.json` carries it for readability, but the
watchdog restores to **its own resolved install location** (the path it
would compute from the running exe/bundle) and *aborts* if the pending
value disagrees — a state-dir write must never be able to redirect the
restore to a hostile path. The cached artifact is verified before
restore (minisign vs the compiled pubkey + recorded sha256).

**Windows per-machine installs.** Per-user installs (the scaffold's
`nsis.installMode: "currentUser"` default — kept) install and roll back
silently. Per-machine installs (`allUsers`, Program Files) need
elevation for both update and rollback: the update install itself
prompts UAC once (acceptable — the user initiated it), but the
*unattended* rollback cannot silently reinstall. Behaviour:

- per-user → watchdog restores silently as prototyped;
- per-machine → the watchdog cannot elevate: it records
  `rollback-pending-approval` in the state dir and the next interactive
  launch surfaces **"update failed, restore the previous version?"**
  (prompted reinstall shows one UAC prompt, or a "download previous
  installer" fallback). This asymmetry is documented in the scaffold's
  install-mode choice; `currentUser` remains the recommended default
  partly for this reason.

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
spike watchdog runs a fixed deadline (the stand-in for the evidence
knobs above — a crashed-before-ready boot is "no sign of life"), fires,
re-verifies the cached v2 artifact,
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

**Proposed rule**: *migrations run only after the update is confirmed —
and must be backwards-compatible while pending.* "Wait for a ready-ack"
is circular — readiness requires `on_startup`, which is exactly where
migrations want to run. The mechanism that breaks the circle is an
update-state surface the app can read *during* a pending first boot:

```python
pylair.update.state() -> "none" | "pending" | "confirmed"
# and/or a hook invoked once the watchdog clears pending:
pylair.update.on_update_confirmed(fn)
```

- `none` — normal boot, no pending window.
- `pending` — this boot is being watched: the app must run
  **read-only** w.r.t. migrated state (`on_startup` loads, serves
  readiness, does not migrate).
- `confirmed` — the watchdog saw `ready` and cleared pending; the
  hook/state flips and migrations may run (this boot or next —
  `on_update_confirmed` fires either way once confirmation lands).

So the app's startup contract becomes: `on_startup` → report readiness
components as usual → *if* `state() == "pending"`, defer all state
writes; migrate on the `confirmed` hook or on the next `none`/`confirmed`
boot. A migration that can't wait defers the *write*, not the load —
old-format state keeps reading fine (that's the compatibility rule,
below).

**Model-store sidecars (0009) stay readable by the previous version.**
Rule: sidecar schemas are **versioned fields, additive-only, and
ignore-unknown-keys**. A v2 that needs new sidecar fields adds them
with defaults; the v1 parser must ignore keys it doesn't know (the same
rule the protocol already enforces on manifests and frames). The escape
hatch for a genuinely incompatible schema is a **new file name** —
`manifest.v2.json` beside `manifest.v1.json` — never an in-place
rewrite; the rollback target keeps reading its own file. A frozen
"sidecars can't change" rule is rejected: it would freeze the model
store at v1 semantics forever; versioned fields + ignore-unknown covers
the common cases, new-file covers the rest.

Enforcement: advisory for v1 (scaffold docs + a `pylair doctor` check
flagging app-data writes before `ready` under a pending marker); the
mechanical guarantee stays §2's rollback.

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
  Errors surface distinctly: `UpdateError::NotUpdatable` (e.g. macOS
  App Translocation) carries the user-facing remedy.
- **Python API** (`pylair` package): `pylair.update.state() ->
  "none"|"pending"|"confirmed"` and
  `pylair.update.on_update_confirmed(fn)` — the migration gate of §4,
  fed from the state dir the watchdog writes.
- **Health/readiness**: new readiness component `updating`
  (`pending|installing|ready|rolled-back|rollback-pending-approval`) so
  apps can render "update pending verification" — and, for per-machine
  installs, the "rollback needs approval" prompt of §2. Watchdog
  verdicts land in `health()` under `update: {state, lastRollback?}`.
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

Runtime cost estimate (measured spike legs, then scaled for a real
app — the spike carries a marker `python/`, a scaffolded app bundles
~97 MiB of interpreter + env and builds a backend venv): spike legs ran
~9min ubuntu / ~7min windows / ~5min macOS, dominated by the cold first
`tauri build` (~5min) — warm builds are ~¼ of that. A real-app leg adds
the venv + bundle stages (roughly +50-100% build time per the 0001
numbers) → **~12-15min ubuntu, ~10-12min windows, ~8-10min macOS** per
update+rollback leg. Downloads/restore scale with the §1 real-size
estimates (130-300M artifacts ≈ 45-60s AppImage re-extract on Linux
rollback; WAN downloads dominate end-user time, not CI time). On macOS
the leg belongs as a step inside an existing macOS job (capacity rule).

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
