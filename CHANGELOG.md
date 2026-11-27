# Changelog

Pylair versions each registry independently: the crate
(`tauri-plugin-pylair`), the PyPI packages (`pylair`, `pylair-ollama`) and
the npm trio (`@pylair/client`, `@pylair/cli`, `create-pylair`) each carry
their own number. Entries below are keyed by the npm release tag, which is
what the GitHub releases page publishes.

## Unreleased

### Multi-file model snapshots: lifecycle and public API (decision 0013)

- **`models.fetch_snapshot()` / `models.model_snapshot()` / snapshot-aware
  `models.path()`** — directory-shaped models are now a public,
  supported feature. Pin a full Hugging Face commit sha (or an explicit
  `{path, sha256, size}` manifest for any other host) and get back an
  immutable, fully verified revision directory. The declarative
  `model_snapshot()` mirrors `model()`: import-time registration,
  managed readiness, auto-start after hello, `requires=(name,)` gating
  unchanged. Workers resolve with `path()` and never fetch.
- **Upgrades never flicker.** Each revision is immutable under
  `<data>/models/<name>/<revision>/`; `current` is a small file
  switched atomically. A `Path` your app already holds keeps reading
  the same revision for the life of the process — no mixed-revision
  loads — while new `path()` calls land on the new revision the instant
  the pointer moves. During an upgrade the readiness component stays
  `ready` with progress in its detail; a failed upgrade leaves the old
  revision serving and reports the failure in place.
- **Reader locks and GC.** `path()` holds a shared byte-range lock per
  revision for the process lifetime, rechecking `.ready` under the
  lock; GC (at fetch boundaries and once at startup, never blocking)
  exclusively probes that lock, deletes `.ready` first and skips
  anything in use — including revisions mmap'd or held by other live
  processes, which are collected once the holder exits. `HF_TOKEN` is
  adopted when no explicit Authorization header is given (never logged,
  still stripped on cross-origin redirects) and `HF_ENDPOINT` selects a
  mirror.

### Multi-file model snapshots: fetch core (decision 0013, internal groundwork)

- **`pylair._snapshots` — internal, no public API yet.** The fetch core
  for directory-shaped models lands behind the package wall: tree
  resolution against the Hugging Face API at a pinned commit sha
  (Link-header pagination walked), the two-kind verifier (raw sha256
  for LFS files, git blob sha1 over the framed payload for small
  files), revision-addressed staging (`.partial/` with per-file
  `.parts`, `.manifest.json`, `.verify.json`, `.ready` written last,
  one completion rename), fan-out 4 across files with prompt
  cancellation, exact-byte progress callbacks, disk preflight and
  cross-process fetch locking. Explicit `{path, sha256, size}`
  manifests serve non-HF sources under a synthetic revision key.
  Per-file transfer reuses the 0009 download machinery, refactored
  (not forked) so both callers share it. The public
  `models.fetch_snapshot`/`path()` surface, the `current` pointer,
  reader locks, GC and readiness wiring are the next PR.
- **User-visible 0009 fix riding the refactor**: the single-file disk
  preflight now needs `size` minus the bytes already banked in `.part`,
  not the whole file again. A resumed download no longer demands room
  for the full file when most of it is already on disk.

### `pylair dev` reload visibility, watch config and manual restart (DX item 10)

- **Watch configuration**: `[tool.pylair.watch]` in pyproject.toml —
  `include` (default `["**/*.py"]`) and `ignore` glob arrays, relative to
  the source dir. Built-in ignores always apply: venvs, `__pycache__`,
  `node_modules`, `.git`, editor temp files and `.pylair/` itself. The dev
  header prints the effective set. One watcher implementation now serves
  both bridge and wrap mode.
- **Reload visibility in the terminal**: `reload: backend/app.py changed,
  restarting Python` on each trigger, `reload: ready in 340ms
  (generation 4)` when the new runtime is up, a named line when Python
  exits during startup, and a named line when the crash-loop breaker
  opens. The plugin writes `.pylair/runtime-status.json` at every state
  transition — dev-only (debug builds, `PYLAIR_RUNTIME_STATUS` set by the
  CLI), never in release builds.
- **Manual restart**: `pylair restart` from a second terminal, or `r` +
  Enter in the dev terminal (line-buffered, TTY only). A manual restart
  resets the crash-loop breaker — and while the breaker is parked open,
  ANY trigger (a file save counts — one human attempt is not a loop)
  brings the runtime back. An external `pylair restart` narrates in the
  dev terminal (`reload: manual restart requested, restarting Python`).
  The inspector's dev-reload entry reads "manual restart".

## npm-v0.1.0-rc.33 (2026-11-26)

- **Pydantic v2 codegen and client types (decision 0012, PR 2 of 2).**
  Models emit one interface each — `FooInput`/`Foo` pairs when validation
  and serialization aliases split the wire shape, keyed by exact alias,
  with computed fields on the output side and serializer-owned fields
  typed from the serializer's return annotation. Shared types finalize
  in codegen: timedelta/UUID/Decimal/Path as string, enums as literal
  unions, tuples as fixed-length tuple types. No client runtime change —
  the shared types are strings or plain arrays on the wire, needing no
  decoding.

- **Pydantic v2 models in the typed bridge (runtime, decision 0012).**
  Exposed functions may take and return pydantic v2 models when the app
  uses pydantic — strict validation with pre-coerced wire forms, the
  same structured `INVALID_PARAMS` errors as dataclasses, by-alias
  output with computed fields. pylair never requires or imports
  pydantic itself; v1 models, RootModel and `model_serializer` shapes
  fail with named errors at decoration. Shared types now wire identically
  in plain params, dataclasses, TypedDicts and pydantic fields:
  timedelta (ISO-8601 durations), UUID, Decimal, Path, enums by value,
  tuples/sets from arrays. TypeScript codegen lands in a follow-up PR.

- **TypedDict support in the typed bridge.** A `TypedDict` (typing or
  typing_extensions) used as a param, return or nested field is now a
  first-class wire shape: codegen emits a deduped TS `interface` with
  optional keys as `key?: T` and keys kept exactly as written (they are
  wire names — no camelCasing, reserved words need no renaming, keys
  that aren't valid identifiers emit quoted). `total=False`,
  `Required[]`/`NotRequired[]` and inheritance between TypedDicts are
  respected, `ReadOnly[]` is accepted with no runtime effect. Input
  coercion validates the object per the declared key types (nested
  dataclasses, `BigInt` and datetimes work inside) and rejects missing
  required keys, wrong types — and unknown keys, stricter than
  dataclasses on purpose: a TypedDict is a closed wire shape — with the
  exact `error.data.errors` path (`args.opts.limit`). The value stays a
  plain dict, and the declared return type drives `BigInt` widening
  inside it like any other container. Generic subscription
  (`Page[int]`) and recursive TypedDicts fail decoration with a clear
  unsupported error rather than degrading silently.

- **Errors that explain themselves.** Python exceptions in exposed
  functions now carry the formatted traceback in `error.data.traceback`
  — dev builds only, since the host opts in under
  `cfg(debug_assertions)` via `PYLAIR_DEV_TRACEBACKS` and release
  builds never leak source paths — trimmed to the app's own frames and
  the client's `console.error` shows it in Python's own format.
  Calling a misspelled function returns `data.suggestions` with close
  matches and a message ending `did you mean 'x'?`; parameter
  validation failures carry `data.errors`, a structured list of
  `{path, expected, received}` entries locating the exact offending
  field (`args.items[2].name`) without ever echoing the value. The
  client types all three on `PythonError` and gains `isCancelled(e)`
  plus the `ErrorCode` constants — the template's cancellation check no
  longer writes the `-32010` literal.
- **The dev inspector correlates what happened** (`pylair dev
  --inspect`). A new timeline view interleaves calls, lifecycle events,
  Python stderr, codegen results and permission denials in exact order
  (one shared sequence counter), filterable by kind, every row
  expandable. Lifecycle entries record runtime spawn (generation, pid),
  exit (code or signal), restarts and why, breaker trips, dev reloads
  naming the changed file, warm-pool member spawn/death/reap and the
  loop-stall warnings. Python stderr lines are recorded
  generation-tagged in their own bounded ring (a log burst never evicts
  a lifecycle event); a call row expands to the lines emitted "during
  this call" — by time window, never claimed as causation. Failed calls
  explain themselves: error code and message always, plus traceback,
  suggestions and per-field errors when the runtime enriches them
  (dev-only optional `error.data` fields — displayed when present,
  never required). `pylair dev` writes `.pylair/codegen-status.json`
  after every stub regeneration; the last failure is a banner on top of
  the inspector and every run a timeline entry. Capability denials are
  recorded with the command, window and origin plus a hint naming the
  permission identifier and `src-tauri/capabilities/`. Everything keeps
  the inspector's rules: dev builds only, compiled out of release,
  bounded lossy rings, try-lock drops, never the events channel.

## npm-v0.1.0-rc.32 (2026-11-26)

Crate `0.1.0-rc.25`, `pylair 0.1.0rc18` on PyPI, npm `0.1.0-rc.32`.

- **The first-run experience.** The default template demonstrates the
  typed bridge, `pylair build` owns codegen, and missing prerequisites
  are named before the long build finds out the hard way. The
  scaffolded `App.tsx` now calls typed stubs from `src/api.generated.ts`
  (one typed call, one typed stream with a Cancel button wired to an
  `AbortController`) instead of raw `call`/`stream` strings. The
  generated file is explicitly ephemeral — gitignored, headed "do not
  edit, not committed", regenerated by `pylair dev` and now also by
  `pylair build` before the frontend typecheck, so a fresh clone
  typechecks without any manual step. Build-time codegen uses the
  bundled interpreter with the staged site-packages and fails the build
  with the python error if the app module cannot be imported.
  `npx create-pylair` ends with a short warning naming any missing
  prerequisites (uv, Rust, MSVC/Xcode CLT/webkit2gtk) and their one-line
  installs — scaffolding still succeeds. `pylair dev` runs the same
  `pylair doctor` checks *before* `tauri dev` and stops with the same
  list instead of failing minutes into the Rust compile
  (`--skip-preflight` bypasses), and a first `pylair dev` now says up
  front that the initial compile takes several minutes. README gained a
  per-OS **Prerequisites** section before the quickstart.
- **Live inspector for `pylair dev`** (dev builds only):
  `pylair dev --inspect` opens a separate read-only window with a live
  table of the last 1000 calls (function, window, mode, latency,
  outcome — ok / error / cancelled with verdict / limit with cap —
  request/response sizes, stream chunk counts), per-function p50/p95
  latency and error counts, the health panel and warm-pool status.
  `--inspect-payloads` adds truncated payload previews (never blob
  bytes). Compiled only into debug builds behind `PYLAIR_INSPECT=1`;
  release builds contain no inspector code. See docs/inspector.md.
- **`pylair.warm()` / `pylair.isolated()`** — typed `worker=` config
  helpers for `@expose` (`worker=warm(pool=2, prewarm=True)`), so
  editors autocomplete the options and bad values fail at the helper,
  not inside the decorator. `worker` is typed `WorkerConfig | dict`;
  the dict form is unchanged (the helpers build and validate through
  the same path). Docs show the helpers first.
- **Version alignment tooling** (DX batch 2): one compatibility manifest
  (`packages/cli/src/compat.json`, vendored into create-pylair — the
  scaffold pins now derive from it, so a release bump edits one place);
  `pylair doctor` gains a Versions section reporting what the app
  actually uses (declared + lockfile-resolved per ecosystem) with a
  compatible/mismatched verdict; `pylair upgrade --check` compares the
  app's set against the newest manifest set and prints the exact
  per-file edits, the lockfile commands and the manual upgrade notes for
  every release in between — offline, read-only, exit codes 0/1/2 for
  scripts (`--online` additionally flags a stale CLI). Backfill covers
  rc.24..rc.31. [docs/upgrading.md](docs/upgrading.md).

- **State events can no longer be dropped.** `pylair://state`,
  `pylair://exposes`, `pylair://health` and `pylair://app` now travel
  per-kind latest-state slots beside the bounded events channel instead
  of `try_send` on the channel itself: a stderr flood (or any bulk
  pressure) used to be able to drop a readiness or liveness transition
  and strand a webview on "loading" forever. Re-entries into the same
  state may coalesce under load (crash-loop churn stays bounded), but
  distinct lifecycle transitions always arrive in order, the newest
  state always reaches every subscriber, and the supervisor never
  blocks. `pylair://stderr` stays lossy
  by design (drop-marked), and a newer generation's report retires an
  older generation's undelivered entries.
- `health()` now carries `pools`: per-function warm-pool status
  (size, live/busy/idle members, queue depth, consecutive deaths,
  crash-loop state, `idle_ttl_s`, reaps) reported by the runtime on
  each pong — for every app, no inspector needed.
- Fixes: clippy now covers the `updater` feature in CI (and the upwatch
  zombie-process lint is fixed) (#183); acceptance-update scenario e
  waits on product boot evidence instead of a timing guess (#184); the
  macOS wrap nav-lock self-test no longer assumes the webview asks
  (#185).

## npm-v0.1.0-rc.31 (2026-11-25)

Crate `0.1.0-rc.24`, `pylair 0.1.0rc17` on PyPI, npm `0.1.0-rc.31`.

- **Auto-update** (decision 0011): opt-in via
  `create-pylair --with-updater` — signed update artifacts, a
  readiness-gated rollback watchdog, `health().update` and a
  `pylair.update`/`update.*` app surface. Default scaffolds are
  unchanged.
- `pylair keys init`: wraps `tauri signer generate` — private key to a
  user-picked path that can never land in git (verified via
  `git check-ignore`), pubkey into `plugins.updater.pubkey`, CI-secret and
  backup instructions. Refuses to overwrite an existing pubkey without
  `--force` (rotation needs the old key).
- `pylair update manifest`: scans the bundle output for signed updater
  artifacts and writes `latest.json` (`--base-url`, `--channel`,
  `--notes`, `--insecure-dev`); fails on a missing `.sig`, versioned
  filenames that disagree with `tauri.conf.json`, or a non-https base url.
- `pylair build --sign`: produces signed updater artifacts (requires
  `TAURI_SIGNING_PRIVATE_KEY`). Builds of updater-enabled apps now check
  `tauri.conf.json` version == `src-tauri/Cargo.toml` version.
- `create-pylair --with-updater` (interactive question, default no):
  injects the updater plugin dep, `plugins.updater` conf block,
  `updater:default` capability permission, `createUpdaterArtifacts` and a
  `*.key` gitignore. Default scaffolds unchanged — a golden test pins it.
- [docs/updates.md](docs/updates.md): setup walkthrough, channels and the
  security model.
- Host update machinery (PR 2 of 5): a new `updater` cargo feature on
  `tauri-plugin-pylair` (off by default; `--with-updater` scaffolds now
  compile it via `features = ["updater"]`) adds `python_update_check /
  _download / _install / _relaunch`. Install arms the watchdog file
  contract first — `pending.json` (`from`, `to`, `to_sha256`, cached
  `artifact`/`sha256`/`sig`, `install_path`, `session_id`, knobs),
  rollback cache outside the install dir, `update-ok` cleared — since
  `download_and_install` never returns on Windows. App Translocation
  refuses `NotUpdatable`; per-machine Windows installs proceed (the
  UAC prompt is the user's own) — only a rollback records
  `rollback-pending-approval`. Readiness signals land too: per-boot `boots/`
  markers, `ready-<version>` on `health().app` ready (keyed on the
  pending `session_id`), `failed-<version>` on a failed component, and
  `health().update` + an `updating` readiness component
  (`pending|installing|ready|rolled-back|rollback-pending-approval`).
- `pylair.update` (Python): `state()` → `"none"|"pending"|"confirmed"`
  and `on_update_confirmed(fn)` for deferred migrations on the pending
  first boot.
- `@pylair/client` `update.*`: `check()`, `downloadAndInstall({onEvent})`
  (upstream Started/Progress/Finished shapes), `relaunch()`;
  `health().update` typing; `UpdateError` with stable `updateCode`
  (`NotUpdatable`, `NeedsApproval`, `SignatureInvalid`, …).
- `update manifest`: a `universal-apple-darwin` build lists its artifact
  under both `darwin-aarch64` and `darwin-x86_64`.
- Rollback watchdog (PR 3 of 5): the app binary carries a hidden
  `--pylair-upwatch` mode; `python_update_install` copies the exe into
  `updater-state/cache/` and spawns it detached before `install()`. It
  waits for `ready-<to>` + `session_id`, rolls back on a failed marker,
  `maxBoots` boots without ready, `heartbeatMs` of silence (live
  component progress — the update gate writes `progress.json` on a
  timer while newer component reports keep landing — extends the
  window to `hardCapMs`), or the hard cap. Rollback verifies the cached artifact (sha256 + minisign vs the
  compiled-in pubkey), refuses an `install_path` mismatch, writes the
  skip-list entry before relaunching, restores per OS, and records
  `rollback-reason.txt` → `health().update.lastRollback`; per-machine
  installs record `rollback-pending-approval` instead of a silent
  restore. Single instance per window (state-dir lock), bounded
  `upwatch.log`.
- `plugins.updater.rollbackUrl` replaces the endpoint-strip rollback
  manifest fetch: a `{{version}}` template resolving to each release's
  own `latest.json`. Unset or unfetchable ⇒ install refuses — no
  rollback path, no install. `create-pylair --with-updater` injects a
  placeholder.
- Update acceptance harness (PR 4 of 5): `scripts/update-acceptance/`
  builds seven real signed variants of a scaffolded `--with-updater`
  app (base, good, slow-confirm, broken-on-startup, crash-loop, and a
  per-machine good/broken pair), stages them under a loopback-served
  `htdocs/` release tree, and drives install→update→confirm/rollback
  scenarios a–f per OS (`scenarios-windows.ps1`, `scenarios-posix.sh`
  with `linux-in-container.sh` for dockerized Linux). A `FAILURES`
  file, per-scenario state snapshots, and `timings.log` are the
  evidence. `acceptance-update.yml` runs Windows + dockerized Linux on
  push and the full matrix on `workflow_dispatch`.
- `worker={"mode": "warm", "idle_ttl_s": N}`: a warm pool member left
  idle N seconds is reaped through the normal kill path — deliberately,
  so no `WORKER_DIED` and no crash-loop count — and the pool respawns
  lazily on the next call (no re-prewarm; `worker:<fn>` stays `ready`).
  The clock runs only while the member is idle: a call in flight is
  never interrupted by it (decision 0008 §4).
- `pylair.BigInt` + `{"$bigint": "<decimal>"}` wire tag: opt-in exact
  big integers in both directions — annotate a param/return or return
  `pylair.BigInt(n)` and codegen emits TS `bigint`; a JS `bigint`
  argument encodes as the same tag and decodes back to `pylair.BigInt`.
  The declared type decides: a `-> pylair.BigInt` result (also
  `Optional`/`list[BigInt]`, and `AsyncGenerator` yields for streams)
  widens plain ints before encoding, so `return a + b` tags without
  re-wrapping — non-int results keep their own encoding. Plain `int`
  is unchanged (numbers in range, clear error outside, now naming
  `pylair.BigInt`). Decode is defensive like the other tags
  (canonical decimal only, else plain data) with a 4096-digit cap.
- Event-loop stalls warn instead of only showing a diagnostic: a pong
  reporting `loop_stale_ms` past `loop_stall_warn` (new
  `SupervisorConfig` knob, default 5 s, `None` to silence) logs one
  stderr line per stall episode — the likely cause (blocking work in an
  `async def` stalls every call), the fix (plain `def` → worker thread,
  `asyncio.to_thread`, `worker="isolated"/"warm"` for heavy loads), how
  long the loop has been stalled and the in-flight call's fn name —
  plus a matching "recovered after Ns" line when the loop frees. Warnings
  are held while `health().app` reports a component `loading`
  (`on_startup` blocking is legitimate) and a stall that began then says
  so when it warns. `loop_stall_kill` stays opt-in: killing on a
  legitimate long block would become a restart loop and roll back good
  auto-updates.
- **Fixes the acceptance runs surfaced**: `--with-updater` scaffolds
  injected a `update_watchdog_entry` call that could not infer its
  runtime type (now `::<tauri::Wry>`) and never registered
  `tauri_plugin_updater` itself — so `app.updater()` panicked on
  `UpdaterState` not managed and every `check()` hung the webview's
  invoke forever. Scaffolds now register the upstream plugin. A
  `plugins.pylair` conf map (the update knobs) panicked at plugin init
  — the plugin now carries a typed config so the section deserializes.
  On Windows the watchdog resolved the install *directory* as the
  install unit, so a restored app could never relaunch (the install
  unit is the exe, matching AppImage-file / `.app`-bundle semantics
  elsewhere). Rollback-artifact verify decoded
  `plugins.updater.pubkey` as a bare minisign key — it is base64 of
  the whole `.pub` file, so every watchdog verify failed until the
  decode matched `tauri-plugin-updater`'s two-step. The rollback
  manifest lookup ran the *update* comparator (`release > current`)
  against the current version's own manifest — restore lookups now use
  an exact-version comparator. `install_mode` read
  `bundle.windows.nsis.installMode` out of `app.config()`, but
  `generate_context!` prunes `bundle.windows.nsis` from the embedded
  `Config` (its `ToTokens` impl forwards only `webview_install_mode`),
  so the runtime never sees it and a per-machine install silently
  restored instead of recording `rollback-pending-approval`. Install
  scope is now inferred from the install *location*: under
  `%ProgramFiles%` is definitionally per-machine, and a `both`-mode
  install resolved per-user is correctly treated as per-user.

## npm-v0.1.0-rc.30 (2026-11-24)

Crate `0.1.0-rc.23`, `pylair 0.1.0rc16` on PyPI, npm `0.1.0-rc.30`.

- **Fix**: a warm worker crash on macOS could leave the call without a
  response. A member that exits to a zombie makes `killpg` on its group
  raise `PermissionError` (EPERM), not `ProcessLookupError` — it escaped
  `_kill_proc`, the exception handler's cleanup and the task itself, so
  `WORKER_DIED` (or the cancel's `terminated` verdict, seen as
  `termination_in_progress`) never reached the caller. `_kill_proc`
  now tolerates both errors on every signal and still awaits the reap,
  and every call path runs its cleanup so the terminal frame is
  unconditional — a task never ends without answering.
- Warm-worker test determinism: the crash/death/respawn waits in
  `test_warm.py` keep their event waits (WORKER_DIED, pidfiles,
  respawn results) under one generous hang-guard bound instead of the
  10 s receive default that starved macOS runners blew. The
  `warm-workers` self-test check — and the interrupt-first cancel
  tests — now assert the whole 0008 §2 contract on POSIX: a response
  inside `cancel_grace_ms` must be `cancelled` + the same member,
  silence past the grace must be `terminated` + a respawned member;
  either pairing proves the documented path and a mismatched pairing
  still fails.
- CI: `acceptance-registry.yml` folds the three per-framework
  `macos-latest` wrap legs into one `wrap-acceptance-macos` job that
  runs command, gradio and streamlit in sequence
  (`scripts/wrap-acceptance/run-macos-registry.sh`, one framework per
  step) — matching the consolidation ci.yml already had. Hosted arm64
  acquisition cancelled two legs in a day; dispatch macos-latest jobs
  go 4 → 2. Linux/Windows keep per-framework legs; `macos-15-intel`
  untouched.

## npm-v0.1.0-rc.29 (2026-11-23)

Crate `0.1.0-rc.23`, `pylair 0.1.0rc15` on PyPI, npm `0.1.0-rc.29`.

- **Blob URLs** (decision 0010, PRs #156–#164): a `pylair-blob` URI
  scheme with an owner-bound registry serves Python `Blob` payloads to
  the owning webview only — ranged reads, hardened headers, per-blob /
  per-generation / per-window limits — with `pylair.Blob` values
  serializing as `{"$blob": {"id","mime","size"}}` and decoding to a
  client `BlobHandle` (`url`, `stream()`, `mediaUrl()`, `release()`),
  `$b64` downgrade on older hosts, template/reference-app CSP and
  `docs/blobs.md`, and an installed-webview `"blob"` self-test that
  gates pack/registry legs on the resolved versions.
- CI only: container self-tests pull apt packages from the Azure
  mirror with retries and explicit timeouts (#161).

- **Blob limit rejections reach the caller.** A refused `blob.register`
  (per-blob/per-generation/per-window caps — `LIMIT_EXCEEDED` with
  `data.limit` `"blob_bytes"`, `"blob_total"`, `"blob_handles"`,
  `"blob_bytes_share"` — or a malformed register, `INTERNAL`) no longer
  ships a `$blob` that fetches 404: the session records it per task and
  the task's terminal response becomes the error, naming the
  `PYLAIR_MAX_BLOB_*` knob and telling the developer to `release()`. In a
  stream the offending chunk ends it with the same error and the
  producer is cancelled the way a client cancel would. Ids that
  registered inside a frame that never reaches the page (an earlier tag
  in the same multi-blob frame, or a dropped terminal) are released
  immediately instead of leaking handle slots until the window closes,
  and a payload rejected after the containment check is unlinked on the
  spot so an over-limit result can't pile up uncounted disk on retries.
  The share bound now uses the real decision-0005 formula (pool minus
  peers' unclaimed reserves) — the old one could never fire — and
  `SupervisorConfig.blob_limits` can override the env knobs.
- **Stream cap check no longer double-writes blobs.** `_pump_stream`
  probes frame size with a placeholder `$blob` tag of the real length,
  so one Blob chunk writes exactly one file.
- **CSP: `media-src` keeps `'self'`.** The blob directives added in the
  templates/reference app had replaced the `default-src` fallback —
  bundled audio/video was blocked. `'self'` is restored alongside the
  blob sources in `csp` and `devCsp` of both `create-pylair` templates
  and the reference app.

- **Blob URLs, templates and docs.** The scaffolded and reference-app
  CSPs now allow the `pylair-blob` scheme in `connect-src`, `img-src`
  and `media-src` (both platform spellings: `pylair-blob:` and
  `http://pylair-blob.localhost`), plus `blob:` in `connect-src`/
  `img-src`/`media-src` for `mediaUrl()` object URLs — without it a
  `fetch` on the object URL is refused. The reference app calls `make_blob`
  and renders the handle in `#blob-img` (outcome in
  `data-blob-status`), releasing it on load. New `docs/blobs.md` covers
  `Blob` vs `Blob.from_path`, the client handle API, limits and
  lifetime, the `$b64` downgrade and the security model; READMEs and
  `docs/performance.md` point at it. `python_release_blob` already rides
  inside `pylair:default`, so no capability file changed.

- **Blob URLs, installed-app acceptance.** The installed-webview
  self-test gains a `"blob"` check (decision 0010 §7): the fixed-payload
  `make_blob` fixture decodes to a `BlobHandle`, then the real webview
  fetches the handle URL (200 + exact bytes), a `Range` request (206 +
  `Content-Range`), `stream()` reassembly, an `<img>` load, and a
  `mediaUrl()` `blob:` URL; `release()` then refuses the same URL, and
  the second window's fetch of it is refused by owner binding — relayed
  through new `selftest_blob_*` Python-state fixtures so neither
  window's timing couples to the other. `--local` legs fail on a
  `"blob":"skipped"` verdict; pack/registry legs arm
  `PYLAIR_BLOB_EXPECTED` only when the resolved crate is ≥ `0.1.0-rc.23`,
  pylair ≥ `0.1.0rc15` and `@pylair/client` ≥ `0.1.0-rc.29`.

- **Blob URLs, client side.** `@pylair/client` decodes
  `{"$blob": {"id","mime","size"}}` anywhere a value is valid — results,
  stream chunks, nested containers — into the exported `BlobHandle`:
  `.url` (the `pylair-blob:` scheme URL, platform form chosen like
  `convertFileSrc`), `.stream()` (a `ReadableStream` of sequential
  ranged fetches that advances by bytes received and stops exactly at
  `size`), `.mediaUrl()` (a `blob:` object URL via `stream()`, capped
  at 256 MiB — the Linux media path), `.arrayBuffer()`/`.blob()`/
  `.text()` conveniences, and `.release()` → `python_release_blob`
  (idempotent; readers on a released handle throw a clear error and a
  mediaUrl() object URL is revoked). `$b64` keeps decoding to
  `Uint8Array` — the documented downgrade on hosts without a blob dir.

- **Blob URLs, Python side.** `pylair.Blob` lands: `Blob(data, mime=...)`
  and `Blob.from_path(path, mime=..., copy=...)` write (atomic rename) or
  hardlink the payload into the host-provided `PYLAIR_BLOB_DIR` and
  serialize as `{"$blob": {"id","mime","size"}}` in results and stream
  chunks. The runtime flushes `blob.register` (id, path, mime, size,
  task_id) ahead of the carrying frame on the same FIFO writer, so the
  host's owner-bound registry always learns a blob before its tag
  arrives. Isolated and warm workers materialize blobs into the
  inherited dir and ride their registrations home in the result payload,
  re-bound to the parent's task. With `PYLAIR_BLOB_DIR` unset (older
  host) a Blob degrades to `$b64` when it fits the frame cap, else the
  call fails `LIMIT_EXCEEDED` naming the feature — never a silent drop.
  The reference app gains a `make_blob` fixture for the acceptance leg.

- **Blob URLs, host side (crate only).** The `pylair-blob` URI scheme is
  registered and a per-generation blob directory (`<app cache>/pylair-blobs/gen-N`)
  is exported to the backend as `PYLAIR_BLOB_DIR`, implementing the host
  half of `docs/decisions/0010-blob-urls.md`: `blob.register`/`blob.release`
  notifications bind blobs to their task's owning window (the owner comes
  from `TaskMeta.owner`, never from the payload), files are served to the
  owning webview only, with an origin gate, owner-label check, hand-rolled
  Range/206, `PYLAIR_BLOB_MAX_FULL_BODY` (64 MiB) and
  `PYLAIR_BLOB_MAX_RANGE_BYTES` (8 MiB) response caps, registered
  size+mtime verification (`410` on in-place source rewrites), exact-origin
  ACAO + `Access-Control-Expose-Headers` (a JS range client can read
  `Content-Range`) + `Vary: Origin` + `nosniff` on every response, the
  512 MiB/2 GiB/256-handle limits, and window-close + generation-end +
  startup-sweep cleanup. A new `python_release_blob` command backs client
  `release()`.

## npm-v0.1.0-rc.28 (2026-11-22)

Crate `0.1.0-rc.22`, `pylair 0.1.0rc14` on PyPI, npm `0.1.0-rc.28`.

- **Model download manager `pylair.models`** (decision 0009):
  `models.model()` declares a model at import — its download
  auto-starts after hello beside `@on_startup` hooks and reports a
  readiness component; `models.fetch()` is the idempotent manual form;
  `models.path()` resolves a verified file or raises `ModelNotReady`.
  Storage lives under `PYLAIR_APP_DATA_DIR` (the host now hands it down
  at spawn) as the verified file plus `.part`/`.json`/`.lock`
  companions — stdlib-only streaming, sha256 verify, atomic rename,
  cross-process lock. Ships with a `models` installed-webview self-test
  and `docs/models.md`; `PYLAIR_MODELS_ALLOW_INSECURE` is scoped to the
  self-test fetches.
- **Fix:** the runtime exits past a dead channel instead of hanging in
  `close_write` (#148, fixing #142).
- **Fix:** the warm worker's "interrupt outside the raise window"
  diagnostic now uses raw `os.write` — repeated SIGINT could re-enter
  the buffered stderr writer and kill the worker or surface a bogus
  call error (#153).

New projects pin `tauri-plugin-pylair = "=0.1.0-rc.22"` and
`pylair==0.1.0rc14`. Design: `docs/decisions/0009-model-manager.md`,
`docs/models.md`. PRs: #148, #149, #150, #151, #152, #153, #154, #155.

## npm-v0.1.0-rc.27 (2026-11-22)

Crate `0.1.0-rc.21`, `pylair 0.1.0rc13` on PyPI, npm `0.1.0-rc.27`.

- **Warm isolated workers.** `worker={"mode": "warm"}` keeps a
  per-function pool of long-lived workers, so imports and loaded models
  survive between calls. An isolated call to a module with a ~300 ms
  import drops from ~360-540 ms to well under 1 ms (p50, see
  `docs/performance.md`).
  - **Interrupt-first cancel on Linux and macOS:** a cancelled call
    gets `KeyboardInterrupt`, and the worker stays warm. Code stuck in
    native work is still killed after `cancel_grace_ms`. **Windows is
    kill-only:** a cancel kills the worker, and the pool respawns it.
  - **`prewarm=True`** starts the pool at launch and reports a
    `worker:<fn>` readiness component, so `requires=("worker:<fn>",)`
    and `health().app` work with no extra code.
  - Calls queue first in, first out per pool, crash loops stop after 3
    consecutive deaths, and `max_isolated_workers` now counts live
    worker processes.
  - Warm mode keeps module globals between calls, by design.
    `mode: "process"` is unchanged.
- **Faster bridge.** Function signatures are cached, outbound values
  are checked in a single pass instead of serialised and re-parsed, and
  each frame is written in one call. A 64 KiB round trip between the
  host and Python is about 2x faster, and argument coercion about 7x.
- **Transport fix.** A large frame write interrupted by a signal could
  be cut short and hang the reader. Writes now loop until every byte is
  out.
- **Latency benchmark.** An opt-in harness across the webview, the
  host-to-Python leg and Python dispatch, on all three OSes. The real
  per-call overhead is about 1-2 ms, almost all of it the webview IPC
  hop.

New projects pin `tauri-plugin-pylair = "=0.1.0-rc.21"` and
`pylair==0.1.0rc13`. Design: `docs/decisions/0008-warm-workers.md`,
`docs/performance.md`. PRs: #136, #137, #138, #139, #140, #141, #143,
#144, #145, #146.

## npm-v0.1.0-rc.26 (2026-11-22)

Crate `0.1.0-rc.20`, `pylair 0.1.0rc12` on PyPI, npm `0.1.0-rc.26`. The
first release since wrap mode to update all three registries.

- **App-defined readiness.** `@pylair.on_startup` hooks run after the
  handshake, so slow setup such as a model load no longer races the
  30-second hello timeout. `loading(name, progress)`, `ready(name)` and
  `failed(name)` report per component, are safe to call from any thread,
  and are coalesced so a progress loop cannot flood the bridge.
- **`@expose(requires=...)`** makes a function wait until its components
  are ready. A failed component answers `INTERNAL` with `data.component`;
  cancelling a waiting call works as usual, and a component no hook ever
  reports is marked failed once startup finishes.
- **`health().app`** shows each component's phase, progress and detail;
  **`onApp()`** fires on phase changes (`pylair://app`). Progress is
  poll-only. An older host ignores the new message; a newer host with an
  older Python reports `app: null`.
- Wrap mode is unchanged: a wrapped app's readiness is still its HTTP
  probe.

## npm-v0.1.0-rc.25 (2026-11-21)

Crate `0.1.0-rc.19`, npm `0.1.0-rc.25`; `pylair` stays `0.1.0rc11`.

- **Runtime health API.** `health()` in `@pylair/client` (and
  `Handle::health()` in Rust) returns a versioned snapshot: state and
  time in state, restarts and the last exit, liveness (pong age,
  round-trip time, missed pongs, event-loop staleness), load totals and
  the calling window's own usage and share. Two verdicts split what
  `unresponsive` used to blur together: `transport: ok | missing` and
  `loop: ok | busy`.
- **`onHealth()`** fires only when a verdict changes, so a "loading..."
  banner needs no polling.
- The `python_health` command is in `pylair:default`.
- Fairness refinement: a window counts toward per-window shares only
  once it calls Python.
- CI hardening: macOS self-test steps fail on a failing app exit
  (`pipefail`), and registry runs fail if the health check is skipped
  against a crate that has it.

## npm-v0.1.0-rc.24 (2026-11-20)

Crate `0.1.0-rc.18`, npm `0.1.0-rc.24`; `pylair` stays `0.1.0rc11`.

- **Per-window fairness.** Every live window keeps a small guaranteed
  share of the command queue, task slots, stream slots and isolated
  workers; a window over its share gets `LIMIT_EXCEEDED` with
  `data.limit = "window_share"`. Nothing already running is cut off.
  `PYLAIR_PER_WINDOW_FAIRNESS=0` restores pure global caps.
  Design: `docs/decisions/0005-per-window-fairness.md`.
- **CLI polish.** Unified status lines and prompt ticks across `pylair`
  and `create-pylair`, with plain/ASCII fallbacks.

## npm-v0.1.0-rc.23 (2026-11-20)

npm-only; crate stays `0.1.0-rc.17`, `pylair` `0.1.0rc11`.

- **New CLI look** shared by `pylair` and `create-pylair`, falling back
  to plain output in CI, with `NO_COLOR`, when piped, or with `--plain`.
- **`pylair doctor` checks the native toolchain**: vswhere/MSVC on
  Windows, `xcode-select` on macOS, `cc` + `webkit2gtk-4.1` on Linux.
- **`create-pylair` prompts redone**; wrap projects can start from a
  Gradio or Streamlit sample app.
- macOS builds retry `hdiutil` on "Resource busy".

## npm-v0.1.0-rc.22 (2026-11-19)

- **Wrap mode.** Turn an existing Python web app (FastAPI, Gradio,
  Streamlit, NiceGUI, Dash, Flask, Panel) into a desktop app with a
  bundled interpreter, a supervised server and native installers.
  `pylair wrap` converts an existing project; `create-pylair --mode wrap`
  starts a new one. Configure with `[tool.pylair] mode = "wrap"` and
  `[tool.pylair.wrap]`; see `docs/wrap.md`.
- Built-in `gradio` and `streamlit` adapters force loopback binding, the
  assigned port, no browser launch and no public share links; anything
  else runs as a `command` with `{port}`.
- The app runs on a random `127.0.0.1` port under the supervisor, with a
  starting page, a restarting page on crash, and recovery on a new port.

## Earlier release candidates

- **rc.20** (2026-11-17): per-function permissions. Capabilities can
  restrict which `@expose`d functions a window may call.
- **rc.19** (2026-11-17): bundled interpreter bumped to
  python-build-standalone 20261110 (CPython 3.12.16 and 3.13.18).
- **rc.18** (2026-11-17): reliability fixes in runtime supervision and
  the build step's download path. No API changes.
- **rc.17** (2026-11-15): bundled-Python download retries, resumes and
  reports progress; `pylair build --dir` relative-path fix.
- **rc.16** (2026-11-15): robustness and hardening under stress;
  shutdown, overload, liveness and Windows process cleanup.
- **rc.15** (2026-11-14): calls and streams fixes; plain calls no longer
  consume stream slots, and the initial stream credit travels on the
  control lane.
- **rc.14** (2026-11-14): secrets excluded from installers; stderr
  delivery opt-in per window; scaffolded `.gitignore` covers `.env`.
- **rc.13** (2026-11-14): per-call IPC channel replaces the shared
  `pylair://stream` event bus; a call's data reaches only its webview.
- **rc.12** (2026-11-13): Windows apps no longer open a console window
  (GUI subsystem in the template, enforced in CI).
- **rc.10** (2026-11-13): covers rc.9 and rc.10.
- **rc.8** (2026-11-12): bounded outcomes and enforceable limits.
- **rc.7** (2026-11-12): first published release candidate.

Some rc numbers have no GitHub release: rc.9 is covered by the rc.10
notes above, and rc.11 and rc.21 were consumed on other registries
between npm publishes.
