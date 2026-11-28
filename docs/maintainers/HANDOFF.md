# Handoff

Last rewritten 2026-11-17, updated for the rc.21 train against `main`
at `db4a123` (after PR 97). What is kept here is what is still true:
where each train stands, the release order, and the standing rules that
AGENTS.md points at. Anything not listed here is in git history.

## Where the tree is

rc.17 is published: the crate at `0.1.0-rc.13`, `pylair` `0.1.0rc10`,
`pylair-ollama` `0.1.0rc3` and the npm trio at `0.1.0-rc.17`.

rc.18 is published: the crate `0.1.0-rc.14` on crates.io and the npm
trio at `0.1.0-rc.18`.

rc.19 is published: npm trio at `0.1.0-rc.19` (npm-only; the crate and
PyPI were unchanged). It shipped the bundled-CPython bump from PR 84:
python-build-standalone `20261110`, CPython `3.12.16` / `3.13.18`.

rc.20 is published: the crate `0.1.0-rc.15` on crates.io and the npm
trio at `0.1.0-rc.20` (`pylair` and `pylair-ollama` unchanged on PyPI).
It shipped per-function permissions (PR 88): `fn_scope` command scopes,
`PermissionDeniedError`, and the `build`/`doctor` capability checks.

rc.21 is published: the crate `0.1.0-rc.16` on crates.io, `pylair`
`0.1.0rc11` on PyPI, and the npm trio at `0.1.0-rc.21`
(`pylair-ollama` unchanged at `0.1.0-rc.3`). It shipped wrap mode (W1
to W4, PRs 92 to 96, 99) and flipped `WRAP_IN_PUBLISHED_VERSIONS`, so
`--local` is no longer needed for wrap scaffolds.

rc.22 is published: the crate `0.1.0-rc.17` on crates.io and the npm
trio at `0.1.0-rc.22` (`pylair` and `pylair-ollama` unchanged on PyPI).
It shipped the wrap-window drag-drop fix (PR 100, `drag_drop_enabled =
false` in `install_window`), atomic pidfile writes, and the CI
docs-only narrowing (PR 103).

rc.23 is published: npm trio at `0.1.0-rc.23` (crate and PyPI
unchanged — no crates.io publish, no PyPI publish, no template
`Cargo.lock` change). It shipped the CLI output redesign (PR 105,
including the doctor native-toolchain check and the shared `ui/`
module), the hdiutil retry (PR 106) and the create-pylair prompts +
wrap framework variants (PR 107).

rc.24 train is in progress, per-window fairness (PRs 110-115,
`docs/decisions/0005`): queue, task, stream and isolated-worker
admission are split per live window with a guaranteed minimum share;
an over-share window's calls get `LIMIT_EXCEEDED` with
`data.limit = "window_share"` naming the owner; the total stream-byte
buffer stays global by design; `PYLAIR_PER_WINDOW_FAIRNESS=0` restores
pure global caps. PyPI is skipped this train (`python/` unchanged).
The crate `0.1.0-rc.18` is published on crates.io; this pins commit
moves the npm trio to `0.1.0-rc.24`, the scaffold crate pin to
`=0.1.0-rc.18`, and regenerates both template `Cargo.lock` files
against the published crate. Registry acceptance with
`expect_version 0.1.0-rc.24` then arms the window-fairness check for
the first time against a published crate (`src/supervisor/fairness.rs`
first ships in rc.18).

| Package | Version in the repo | rc.24 state |
|---|---|---|
| `tauri-plugin-pylair` (crate) | `0.1.0-rc.18` | published |
| `pylair` (PyPI) | `0.1.0-rc.11` | unchanged, skipped |
| `pylair-ollama` (PyPI) | `0.1.0-rc.3` | unchanged, skipped |
| `@pylair/client`, `@pylair/cli`, `create-pylair` (npm) | `0.1.0-rc.24` | next to publish |

Remaining order for rc.24: pack-mode acceptance green on this commit,
then the npm trio under `--tag next`, then registry acceptance with
`source=registry` and `expect_version 0.1.0-rc.24`.

The next train is runtime health (PRs 118-124, `docs/decisions/0006`),
crate `0.1.0-rc.19` plus npm `0.1.0-rc.25`: `health()` returns the
host-observed snapshot through the new `python_health` command (state,
generation, restart accounting, transport + loop liveness verdicts,
load totals, the calling window's fairness row) and `onHealth`
delivers `pylair://health` only on a liveness-verdict change. `self`
is scoped to `window.label()`, never JS input; health polling is
read-only — the fairness live set is joined on calls only, so a
poller reserves no share. The installed-webview self-test gained a
`health` check (clean skip when the resolved crate predates rc.19,
mandatory on `--local` legs and — via the acceptance-registry version
gate — on pack/registry legs once the resolved crate carries
`python_health`). PyPI is skipped this train (`python/` unchanged).

The crate `0.1.0-rc.19` is published on crates.io; this pins commit
moves the npm trio to `0.1.0-rc.25`, the scaffold crate pin to
`=0.1.0-rc.19`, and regenerates both template `Cargo.lock` files
against the published crate. Remaining order for rc.25: pack-mode
acceptance green on this commit, then the npm trio under `--tag
next`, then registry acceptance with `source=registry` and
`expect_version 0.1.0-rc.25` — the first run where
`PYLAIR_HEALTH_EXPECTED` arms (`python_health` first ships in
rc.19), so a `health` skip fails the leg on every OS.

The app-readiness train (PRs 128-133, `docs/decisions/0007`) is the
first since rc.21 to touch all three registries. The crate
`0.1.0-rc.20` is published on crates.io and `pylair 0.1.0rc12` on PyPI
(the first PyPI publish since rc.21); `pylair-ollama` stays
`0.1.0-rc.3`. What they ship: `@on_startup` hooks report named
components `loading`/`ready`/`failed`, coalesced into `app.component`
notifications; `@expose(requires=…)` gates calls on them (a `failed`
requirement answers `INTERNAL` with `data.component`); the host keeps
a bounded per-generation component map in `health().app` and emits
`pylair://app` (`onApp`) on phase transitions only — progress reads
stay poll-only. This pins commit moves the npm trio to `0.1.0-rc.26`
and the scaffold pins to `rs =0.1.0-rc.20` / `py pylair==0.1.0rc12`
(the first `py` pin change since rc11), with both template
`Cargo.lock` files regenerated against the published crate. Remaining
order:

1. pack-mode acceptance green on this commit, then the npm trio under
   `--tag next`;
2. registry acceptance with `source=registry` and
   `expect_version 0.1.0-rc.26` — the first run where
   `PYLAIR_APP_READINESS_EXPECTED` arms (it requires BOTH the rc.20
   crate and the 0.1.0rc12 package resolved), so an `app-readiness`
   skip fails the leg on every OS.

The perf + warm-workers train (PRs 137-138, 139-144) ships TWO
registry bumps — `pylair 0.1.0rc13` AND the crate `0.1.0-rc.21` —
because the crate changed since rc.20: #137 added the
`python_bench_report` command and its permission; #138 reworked the
host↔Python framing in `supervisor/wire.rs`, `spawn.rs` and
`session.rs` (the 64 KiB round trip halved and the serialization
speedups — docs/performance.md). The PyPI side carries the transport
write-loop fix (a single raw `write()` truncated frames under signal
load on POSIX) plus warm isolated workers (`docs/decisions/0008`):
`worker={"mode": "warm"}` keeps a persistent per-function pool — the
bench headline is `spawn_cold_process` hundreds of ms/call vs
`spawn_warm_reuse` under a millisecond on all three OS legs. POSIX
cancels interrupt-first (SIGINT inside a `started`-handshake window,
plain `cancelled` ack and the same member survives); Windows is
kill-only (CTRL_BREAK cannot reach a console-less worker).
`prewarm: True` spawns the first member at startup and reports the
managed `worker:<fn>` readiness component; `requires=("worker:<fn>",)`
gates on it. The installed-webview self-test gained a `warm-workers`
check — mandatory on `--local` legs and, on pack/registry legs, armed
by `PYLAIR_WARM_EXPECTED` only when the resolved pylair is
`0.1.0rc13` or later. npm needs `0.1.0-rc.27` (the selftest client
changed).

The crate `0.1.0-rc.21` is published on crates.io and `pylair
0.1.0rc13` on PyPI; `pylair-ollama` stays `0.1.0-rc.3`. This pins
commit moves the npm trio to `0.1.0-rc.27`, the scaffold pins to
`rs =0.1.0-rc.21` / `py pylair==0.1.0rc13`, and regenerates both
template `Cargo.lock` files against the published crate (checksum
`8b949ad3…f585`). Remaining order:

1. pack-mode acceptance green on this commit — expected-red until the
   npm publish lands is NOT in play here (scaffolded pins resolve
   crate rc.21 + pylair rc13, both live), so legs should be green;
2. the npm trio under `--tag next`;
3. registry acceptance with `source=registry` and
   `expect_version 0.1.0-rc.27` — the first run where
   `PYLAIR_WARM_EXPECTED` arms (resolved pylair 0.1.0rc13), so a
   `warm-workers` skip fails the leg on every OS.

## Wrap mode W1 (launcher + supervisor), on branch `wrap-w1`

Wrap mode ships in 0.1.0. W1 is the plumbing only; nothing user-facing
(CLI, config keys, window, navigation) exists yet.

What it adds:

- `python -m pylair.wrap --cmd <argv...> [--ready-path /]` (new package
  `pylair.wrap`): reuses the runtime's frame-fd hygiene and transport,
  picks a loopback port (5 bind retries), exports `PYLAIR_PORT`, replaces
  `{port}` (and a bare `python` becomes the launcher's own interpreter),
  waits for TCP then non-5xx on the ready path, sends `ready {url}`, then
  probes every 10s and exits 5 after 3 consecutive misses (4 when the
  server exits, 3 when it cannot start, 2 for bad usage). Tuning flags
  `--probe-interval`, `--probe-timeout`, `--max-misses` exist for tests.
- Crate: `RuntimeKind::{Bridge, Web}` on `SupervisorConfig::kind` (default
  Bridge, unchanged), `SupervisorConfig::ready_timeout` (default 120s,
  clamped 1s..=1h), `Builder::runtime_kind` / `Builder::ready_timeout`,
  `Handle::{kind, ready_url, subscribe_ready_url}`. Ready is reached on
  `ready`, the url is validated (`util::validate_ready_url`), and the
  generation's url is cleared when it ends. `pylair://state` payloads gain
  `url` while a web server is up; `python_state` returns `{state, url}`
  for the web kind and the bare string for the bridge kind.
- `docs/protocol-v1.md` section 4.1 is the contract; `docs/security.md`
  section 9 has the honest status table.

For W2 (CLI, config, window):

- Wire it with `Builder::runtime_kind(RuntimeKind::Web)` and a spawn spec
  for `python -m pylair.wrap --cmd ...`. Navigate to `ready_url()` when the
  state is `ready`; a restart is a NEW url, so re-navigate on every
  `ready` and show the restarting page in between.
- `python_state`'s web shape is a plain-JSON union by design (bridge
  unchanged). If W2 prefers a dedicated command it needs a permission
  file; do not widen `pylair:default`.
- The launcher cannot force a server to bind loopback only. The command
  (or the W3 adapter) must pass `127.0.0.1`; document it in the recipes.
- The `pylair` PyPI package and the crate both change, so the 0.1.0 train
  has a PyPI publish and a crate publish. No versions were bumped here.

Test notes: `crates/tauri-plugin-pylair/tests/web_runtime.rs` (fake
runtimes plus the real launcher), `python/pylair/tests/test_wrap.py`,
and `python/pylair/tests/wrap_server.py` (the misbehaving test server).
These start many Python processes; if one fails while a build is
saturating the machine, rerun it before treating it as real.

## Wrap mode W2 (CLI + config + window), on branch `wrap-w2`

Builds on W1 (PR 92) and W3 (PR 93, the adapters). Everything a wrap app
needs: config, CLI, the window.

- **Config and CLI.** `[tool.pylair] mode` and `[tool.pylair.wrap]`
  (`packages/cli/src/wrapconfig.ts`, strict validation, every error names the
  key). `pylair wrap [entry]` / `pylair init --mode wrap` initialise an
  existing project (manifest required, never overwrites a file);
  `create-pylair --mode wrap` scaffolds a FastAPI command-mode sample (no
  React). `pylair dev`, `build` (with `--stage-only`) and `doctor` handle
  wrap apps. See `docs/build.md`, "Wrap mode".
- **Plugin.** `Builder::wrap()`, one line in the app's `lib.rs`: reads
  `pylair-wrap.json`, starts the launcher, builds the window from
  `RunEvent::Ready` (see the decision record), local starting and restarting
  pages, navigation lock, downloads, Back / Reload / Home menu. Decisions are
  pure functions in `crates/tauri-plugin-pylair/src/wrap/policy.rs`.
- **The launcher interface is generated, not implemented here.**
  `python -m pylair.wrap --framework F [--entry E] [--ready-path P] [--cmd ...]`.
  `--ready-path` is passed only when `ready-path` is set in the config: the
  adapters have their own health paths (gradio `/config`, streamlit
  `/_stcore/health`) and forcing `/` would undo that. W3's launcher
  implements the interface; the installed self-test was run against it for
  command (FastAPI), gradio and streamlit apps.
- **Installed self-test.** `PYLAIR_SELF_TEST_WRAP=<marker>` on an installed app
  checks: ready, the window at the app url, GET returns the marker, the
  navigation lock refuses an unknown scheme in place and as a new window,
  and 10 IPC calls from the wrapped page are all denied (the permanent no-IPC
  check). Optional: `PYLAIR_SELF_TEST_WRAP_RESTART=1` (dev only: restarts the
  server, expects the restarting page and a new url) and
  `PYLAIR_SELF_TEST_WRAP_DOWNLOAD=<path>|<text>` (needs `downloads =
  "downloads"`). W3 wires it into the acceptance matrix.

Before the 0.1.0 train can ship wrap mode:

- `create-pylair --mode wrap` and `pylair wrap` refused `--local`-less
  runs until the rc.21 pins PR flipped `WRAP_IN_PUBLISHED_VERSIONS` in
  `packages/create-pylair/src/scaffold.ts`: versions before rc.21 lack
  wrap mode. The flag stays for future template features that outrun
  the published versions.
- The crate gained a dependency (`rfd`, Save As dialog), so the default
  template's `Cargo.lock` regenerates in the usual post-publish step, and the
  wrap template ships no `Cargo.lock` yet: generate and commit one after the
  crate is live, with the test that pins it (as for the default template).
- `@pylair/cli` does not depend on `create-pylair`: `pylair wrap` runs the
  scaffolder (`create-pylair@<same version>` through npx, or a checkout's
  build with `--local`) so templates and pins cannot drift.

Merged since rc.17 (d7daaaf):

- PR 70, R4: `lib.rs` split into modules (pure move).
- PR 74: `run_child` carved into `run/` — handshake, `Session`, the
  command handlers (pure move).
- PR 75: kill and reap the runtime on reader EOF before reporting
  Restarting.
- PR 76: tombstone an abandoned stream so a late cancel reports
  completed.
- PR 79: a stale download completion can no longer touch the next
  attempt's `.part`.
- PR 81: `.part` cleanup and the progress sink can no longer throw
  inside a download event handler.
- PR 71, 72, 73, 78, 80: supervisor test hardening (run_child
  characterisation and guards, deterministic kill ordering, orphan
  failure names the survivor, startup-noise robustness).
- PR 77: the lane-full limit values come from the channel capacities.
- PR 69, rc.17 pins: scaffold pins, template `Cargo.lock`, npm trio at
  `0.1.0-rc.17` — the tail of the rc.17 train, landed after the bump.
- PR 82, rc.18 version bump: crate `0.1.0-rc.14`; the reference app's
  `uv.lock` needed no refresh (`uv lock --check` clean).
- PR 83, rc.18 pins: scaffold pins, template `Cargo.lock` against the
  published crate (plugin entry only), npm trio at `0.1.0-rc.18`, README
  and npm README install lines.
- PR 84: bundled CPython bump to python-build-standalone `20261110`
  (`3.12.16` / `3.13.18`); RUSTSEC-2024-0370 disclosed in
  `docs/security.md`; release checklist now checks the PBS pin.
- PR 98, 99, 101, 102: rc.20 to rc.22 train commits (bumps, pins,
  template `Cargo.lock` regenerations).
- PR 100: wrap windows accept file drops (native drag-drop disabled);
  atomic pidfile writes.
- PR 103: docs-only CI detection narrowed so shipped Markdown does not
  skip the suite.
- PR 105: CLI output redesign, shared `ui/` module, `pylair doctor`
  native-toolchain check (vswhere on Windows, `xcode-select` on macOS,
  `cc` + `webkit2gtk-4.1` on Linux).
- PR 106: `hdiutil` retries on transient resource-busy failures during
  dmg repackaging.
- PR 107: create-pylair wordmark + TTY prompts, wrap framework
  variants, `pylair wrap`/`init` framework prompts; `ui/` shared by
  build-time copy so create-pylair stays dependency-free.
- PRs 110-115: per-window fairness for shared runtime limits
  (`docs/decisions/0005`). Queue, task, stream and isolated-worker
  admission are split per live window with a guaranteed minimum share;
  an over-share window's new calls get `LIMIT_EXCEEDED` with
  `data.limit = "window_share"`. `PYLAIR_PER_WINDOW_FAIRNESS=0` restores
  pure global caps; the total stream-byte buffer stays global by
  design. The two-window self-test proves the real-webview path (win-b
  subscribes through `WindowRegistry` and gets its share while main is
  over share). Crate-side work plus a client selftest/doc touch: the
  next release needs a crate bump (`0.1.0-rc.18`), scaffold pins and
  the template `Cargo.lock`, and the npm trio; `python/` is unchanged,
  so no PyPI step.

- PRs 118-124: runtime health API (`docs/decisions/0006`). `health()`
  (the `python_health` command) returns a host-observed snapshot —
  state, generation, restart accounting, transport + loop liveness
  verdicts, global load totals and the calling window's own fairness
  row — and `pylair://health` (`onHealth`) emits only when a verdict
  *changes*. The installed-webview self-test gains a `health` check
  through the real bridge: clean skip when the resolved crate predates
  `python_health` (first published crate carrying it is
  `0.1.0-rc.19`), mandatory on `--local` legs. Crate-side plus client
  API and selftest/docs: the next release is the crate at
  `0.1.0-rc.19` plus the npm trio at `0.1.0-rc.25` with scaffold pins
  and the template `Cargo.lock`; `python/` is unchanged, so no PyPI
  step.
- PR 88: per-function permissions (crate `fn_scope`, client
  `PermissionDeniedError`, CLI capability checks, self-test
  `fn-permission` check, `docs/decisions/0004-per-function-permissions.md`).
- PRs 128-132, plus 133 (rc.20/rc12 bump): app-defined readiness
  (`docs/decisions/0007`). Python reports named components through
  `@on_startup` hooks + `loading`/`ready`/`failed`, coalesced into
  `app.component` notifications (first post-hello flush carries one
  `loading` frame per declared component); `@expose(requires=…)`
  gates calls on them (a `failed` requirement answers `INTERNAL`
  with `data.component`; cancelling the wait is `TASK_CANCELLED`).
  The host keeps a bounded per-generation component map (32 names,
  64/256-char truncation, progress outside `[0,1]` → `null`) in
  `health().app` and emits `pylair://app` on phase transitions only;
  the client gained `HealthApp`/`AppChange` types and `onApp`. The
  installed-webview self-test gained an `app-readiness` check —
  mandatory on `--local` legs and, on pack/registry legs, only when
  BOTH the resolved crate (≥ `0.1.0-rc.20`) and the resolved `pylair`
  (≥ `0.1.0rc12`) carry the feature (`PYLAIR_APP_READINESS_EXPECTED`,
  mirroring `PYLAIR_HEALTH_EXPECTED`). All three registries move —
  see the train paragraph above.
- PR 134: rc.26 pins — crate `0.1.0-rc.20`, pylair `0.1.0rc12`, npm
  trio at `0.1.0-rc.26`.
- PR 135: repo front-door docs refresh.
- PR 136: deterministic window-fairness flood in the self-test.
- PR 137: opt-in three-layer latency benchmark — `bench.yml` on
  workflow_dispatch, webview/rust/python `*-bench:` JSONL per OS,
  baseline numbers in `docs/performance.md`.
- PR 138: host↔Python latency wins (perf-2) — the 64 KiB round trip
  cut roughly in half; see the before/after tables in
  `docs/performance.md`.
- PRs 139-144: warm isolated workers (`docs/decisions/0008`).
  `@expose(worker={"mode": "warm", "pool": N})` serves calls from a
  long-lived per-function pool — lazy spawn, per-member FIFO, live
  members counted against `max_isolated_workers`, crash-loop stop at
  3 consecutive deaths, `WORKER_DIED` carrying a bounded stderr tail.
  POSIX cancels interrupt-first: the member announces its raise
  window with a `{"started": true}` frame, the runtime `killpg`s
  SIGINT inside `cancel_grace_ms` and a surviving member serves the
  next call; Windows is kill-only (`GenerateConsoleCtrlEvent` cannot
  reach console-less workers). `prewarm: True` spawns the first
  member at startup and reports the managed `worker:<fn>` readiness
  component (import+resolve handshake on the private channel);
  `requires=("worker:<fn>",)` gates on it. A partial-frame transport
  bug (single raw `write()` truncating under signals) was found and
  fixed on the way. PR 144 added the real-webview `warm-workers`
  self-test check (same-pid reuse, POSIX interrupt survival vs
  Windows terminated+new-pid), the unmanaged `worker:<fn>` sweep
  diagnostic, the cold-vs-warm bench case with three-OS numbers in
  `docs/performance.md`, and the docs pass. The check's POSIX side
  later learned the contract's second verdict: silence past
  `cancel_grace_ms` is the kill path by design (the verdict is
  parent-side — a starved runner's event-loop lag is
  indistinguishable from a stuck call), so POSIX accepts
  `cancelled`-and-same-pid or `terminated`-and-new-pid and asserts
  whichever aftermath the ack claims. A later macOS loop wedge
  (`_queue.Empty` at the full hang guard, member absent from `ps`,
  runtime loop parked in `select`) traced to a real kill-path bug:
  a member exiting to a zombie makes `killpg` on its group raise
  `PermissionError` — macOS differs from Linux (ESRCH) here — and it
  escaped `_kill_proc`'s `ProcessLookupError`-only catches, the
  exception handler's cleanup and the task itself, so the terminal
  frame was never sent (a cancel surfaced as
  `termination_in_progress`). Signal errors are now contained at every
  `_killpg`/`proc.kill` site and the cleanup awaits before `_finish`
  are guarded — a task can never end without a terminal.
  `PYLAIR_KILLPG_FAULT=eperm` reproduces it anywhere — a test-only
  fault hook, honoured only for that exact value, POSIX-only
  (`_killpg` is never reached on Windows).
- PR 148: #142 — the runtime entered drain but never exited on
  Windows + newest CPython: the tp-writer thread sits inside a
  synchronous `write()` on a pipe the host never drains, and
  `close_write()` then blocked the main thread on the pending write
  (proven with a `PYLAIR_DEBUG_HANG` faulthandler probe — new
  permanent diagnostic). `_finish` now bounds the writer join and,
  when it is still inside `write()`, leaves the dead channel open and
  exits the process instead; `_finish_executor` applies the same
  bound to the default executor, which `asyncio.run` teardown would
  otherwise join unbounded (3.12) or for 300 s (3.15). The next PyPI
  train carries it.
- PR 150: model manager PR 1/3 (decision 0009 §2) — the host hands the
  app data dir to the runtime as `PYLAIR_APP_DATA_DIR` at every spawn
  that has an AppHandle (`app_spawn_spec` dev + release, wrap
  `spawn_spec` dev + release); the dir is created if missing, a
  resolve/mkdir failure logs once and leaves it unset, never fatal.
  Debug builds only honor an inherited override (same precedent as
  `PYLAIR_APP_MODULE`); packaged apps always get the resolved dir.
  Isolated/warm workers inherit it through the runtime env. Crate
  bumps in the next train; `pylair` PR 2 adds `pylair.models`.
- PR 151: model manager PR 2/3 (decision 0009 §3-§7) —
  `pylair.models` itself: `models.model()` declarative autostart,
  `fetch()`/`path()`/`dir()`, `.part` + sidecar storage under
  `PYLAIR_APP_DATA_DIR`, Range/If-Range resume, ≤5 redirects with
  cross-origin sensitive-header stripping, https-only, jittered
  retries, kernel lock with shared progress, cancel-leaves-resumable-
  `.part`, and readiness integration including the late-registration
  sweep hint. `path()` is strict-check only (one stat, never a
  re-hash); `fetch`/autostart own the verify-and-repair pass.
- PR 152: model manager PR 3/3 (decision 0009 §8) — the
  installed-webview `models` self-test check plus `docs/models.md`.
  The fixture runs a loopback-only HTTP Range server inside the
  bundled runtime, fetches + verifies a known blob, proves the second
  fetch is the sidecar fast path (one server hit), resolves the same
  path read-only inside a warm worker, then removes the model from the
  app data dir. `--local` legs always carry the fixture and grep-fail
  on a `"models":"skipped"` verdict; pack/registry legs append it and
  arm `PYLAIR_MODELS_EXPECTED=1` only when the resolved pylair is
  ≥ `0.1.0rc14` AND the resolved crate is ≥ `0.1.0-rc.22` (the side
  that passes `PYLAIR_APP_DATA_DIR`).
- PR 153: warm-worker interrupt hardening — a second SIGINT landing
  while the "outside the raise window" diagnostic was mid-print
  re-entered the buffered stderr writer, and the RuntimeError escaped
  the serve loop's channel reads into a worker exit (`_WarmWorkerDead`
  upstream — the CI flake `test_warm_interrupt_as_call_returns`
  guards). The diagnostic now writes through `os.write` on the raw fd;
  `test_warm.py` gained a death-order watcher and a 400-signal
  idle-burst test. Ships in rc14.
- The next release is the full three-registry train. The bump commit
  (PR 154) takes the crate to `0.1.0-rc.22` (PR 150's
  `PYLAIR_APP_DATA_DIR` to the runtime) and `pylair` to `0.1.0rc14`
  (PR 148's Windows exit-hang fix — the bounded writer join on a
  stalled host plus the bounded executor join at shutdown — PR 151's
  `pylair.models`: declarative `models.model()` autostart,
  `fetch()`/`path()`, resumable verified downloads into the app data
  dir with readiness progress, cross-origin credential stripping,
  https-only — and PR 153's warm-worker reentrancy fix).
  `pylair-ollama` stays `0.1.0-rc.3`. The crate `0.1.0-rc.22` is
  published on crates.io (checksum `206da96c…4cc39`) and `pylair
  0.1.0rc14` on PyPI. This pins commit (PR 155) moves the npm trio
  to `0.1.0-rc.28`, the scaffold pins to `rs =0.1.0-rc.22` /
  `py pylair==0.1.0rc14`, and regenerates both template `Cargo.lock`
  files against the published crate. Remaining order:

  1. pack-mode acceptance green on this commit (scaffolded pins
     resolve crate rc.22 + pylair rc14, both live);
  2. the npm trio under `--tag next`;
  3. registry acceptance with `source=registry` and
     `expect_version 0.1.0-rc.28` — the first run where
     `PYLAIR_MODELS_EXPECTED` arms (resolved pylair ≥ 0.1.0rc14 AND
     crate ≥ 0.1.0-rc.22), so a `models` skip fails the leg on every
     OS.

`python/` and `integrations/` had not changed since rc.16 until app
readiness (PR 129) — the next train's PyPI step is back on; step 2
below was skipped for rc.20 only.
- PRs 157-163: blob URLs (`docs/decisions/0010`), the bulk-binary
  transport. Five-plus-one PRs total: 157+158 crate (`pylair-blob`
  scheme handler, per-generation dirs, owner binding,
  `python_release_blob`, bounded reads), 159 `pylair.Blob` (per-send
  handles, `$b64` fallback, task-less-frame refusal), 160 client
  `BlobHandle` (`url`, ranged `stream()`, `mediaUrl()`, `release()`),
  162 templates/reference-app/docs, 163 the crate follow-up (limit
  rejections reach the caller — `LIMIT_EXCEEDED` + `data.limit` on the
  terminal, streams end + producer cancel, malformed registers as
  `INTERNAL`; the fair-share bound realigned to the 0005 formula — the
  shipped one was unreachable; orphaned handles released with the
  replaced frame; rejected payloads unlinked post-containment), and
  the last one the installed-app acceptance check. The `blob`
  self-test check fetches a fixed-payload `make_blob` through the real
  webview (full 200, ranged 206 + Content-Range, `stream()`
  reassembly, `<img>` load, `mediaUrl()` blob: URL, `release()` →
  refused) and proves owner binding: win-b reads the id through the
  `selftest_blob_*` Python-state relay (same pattern as
  `b_seen`/`holds_active` — it keeps win-b's report timing independent
  of the owner's progress) and its fetch must refuse. `--local` legs
  always carry the fixture and grep-fail on a `"blob":"skipped"`
  verdict; pack/registry legs append it and arm
  `PYLAIR_BLOB_EXPECTED=1` only when the resolved crate is ≥
  `0.1.0-rc.23` AND pylair ≥ `0.1.0rc15` AND `@pylair/client` ≥
  `0.1.0-rc.29`. The rc.32 train shipped: npm trio
  `0.1.0-rc.32`, scaffold pins `pylair==0.1.0rc18` / crate
  `=0.1.0-rc.25`. The bump commit (PR 199) took the crate to
  `0.1.0-rc.26` and `pylair` to `0.1.0rc19` for the pydantic v2
  runtime and codegen — crate `0.1.0-rc.26` is on crates.io
  (checksum `703f22c0…4e09`) and `pylair 0.1.0rc19` on PyPI.
  The pins commit (PR 200) moved the scaffold pins by editing
  `packages/cli/src/compat.json` alone and regenerated both
  template `Cargo.lock` files against the published crate; this
  commit moves the npm trio to `0.1.0-rc.33` (package.json
  versions, the compat.ts pairing comment, the README install
  lines and the upgrading.md pairing — the split was the rc.33
  dry-run lesson: the publish workflow checks all three
  package.json versions match the input). The rc.34 train:
  the bump commit (PR 208) took the crate to `0.1.0-rc.27` and
  `pylair` to `0.1.0rc20` for multi-file model snapshots (decision
  0013: fetch, lifecycle, GC and the `pylair models resolve` pin
  helper), the dev-reload visibility batch and the 0009 preflight
  banked-bytes fix; crate `0.1.0-rc.27` is on crates.io (checksum
  `39e327d5…0ddc`) and `pylair 0.1.0rc20` on PyPI. This pins commit
  moves the npm trio to `0.1.0-rc.34` in one pass (compat.json, all
  three package.json versions, the compat.ts pairing comment, both
  template `Cargo.lock` files against the published crate checksum,
  the regenerated golden, the README install lines and the
  upgrading.md pairing); the rc.33 split does not repeat. The rc.35
  train: the bump commit (PR 213) took the crate to `0.1.0-rc.28`
  and `pylair` to `0.1.0rc21` for external binaries (decision 0014:
  the crate's `PYLAIR_BINARIES` resolution and release env stripping
  in PR 210, the CLI's declaration, hash-verified staging,
  `binaries.json` manifest, licence notices and doctor report in PR
  211, and `pylair.binary()` / `pylair.proc` with tree-kill on cancel
  plus the unread-`PIPE` warning in PR 212 - single-file binaries
  only, directory units land in a later release); crate `0.1.0-rc.28`
  is on crates.io (checksum `752d6079…a5f7`) and `pylair 0.1.0rc21`
  on PyPI. This pins commit moves the npm trio to `0.1.0-rc.35` in
  one pass (compat.json, all three package.json versions, the
  compat.ts pairing comment, both template `Cargo.lock` files against
  the published crate checksum, the regenerated golden, the README
  install lines and the upgrading.md pairing). Remaining order:

  1. pack-mode acceptance green on this commit (scaffolded pins
     resolve crate rc.28 + pylair rc21, both live);
  2. the npm trio under `--tag next`;
  3. registry acceptance with `source=registry` and
     `expect_version 0.1.0-rc.35`; `PYLAIR_BLOB_EXPECTED` keeps
     arming (all three resolved versions are already past their
     floors), so a `blob` skip still fails the leg on every OS.

  The rc.36 train is npm-only. PR 215 shipped directory binary units
  (decision 0014 PR B): declaration, safe archive extraction,
  manifest-verified directory sources, per-OS loader rewriting and
  dependency validation, unit signing, doctor size reporting and the
  installed-app acceptance fixture. All of that landed in the npm-owned
  CLI/scaffold/test surface; the crate's `shape: "unit"` manifest
  resolution and the Python `pylair.binary`/`pylair.proc` API already
  shipped in rc.35, and neither `crates/` nor `python/` changed since
  then. Crate `tauri-plugin-pylair` therefore stays `0.1.0-rc.28` and
  `pylair` stays `0.1.0rc21`: there is no crates.io or PyPI publish in
  this train. This pins commit moves only the npm trio to
  `0.1.0-rc.36` (compat.json, all three package.json versions, the
  compat.ts pairing comment, the regenerated golden, the package README
  install lines and the upgrading.md pairing); both template
  `Cargo.lock` files stay untouched because the crate pin and checksum
  do not move. Remaining order:

  1. pack-mode acceptance green on this commit (the scaffolded crate
     rc.28 + pylair rc21 pins are already live);
  2. the npm trio under `--tag next`;
  3. registry acceptance with `source=registry` and
     `expect_version 0.1.0-rc.36`.

  The rc.37 train is PyPI-plus-npm, with the crate unchanged. PR 219
  bumped only `python/pylair` to `0.1.0rc22` and moved the learning-path
  changelog entries under the rc.37 tag; the PyPI publish was therefore
  pylair-only, and `tauri-plugin-pylair` stays `0.1.0-rc.28` with no
  crates.io republish. This pins commit moves the npm trio to
  `0.1.0-rc.37` in one pass (compat.json, all three package.json
  versions, the compat.ts pairing comment, the regenerated golden, the
  package README install lines and the upgrading.md pairing). It is the
  first pins train that also moves `examples/learn`: all five steps pin
  npm rc.37 and pylair rc.22, their package-locks regenerate against the
  packed rc.37 tarballs, their uv.lock files move only pylair, and their
  Cargo.toml/Cargo.lock crate pins stay at rc.28. Both template
  `Cargo.lock` files are untouched for the same reason. The scaffold and
  learning examples also constrain `@tauri-apps/api` and
  `@tauri-apps/cli` to the 2.12 minor line because Tauri's installer
  build rejects an npm API a minor ahead of the locked crate. Because npm
  rc.37 is not published when this pins PR runs, `check-learn-ts` packs
  the checkout's client and cli tarballs and serves them through a
  loopback npm-registry shim while reproducing the examples' registry
  install; the committed package-lock integrities are therefore the exact
  bytes npm publish must preserve. Remaining order:

  1. pack-mode acceptance green on this commit (the scaffolded crate
     rc.28 + pylair rc.22 pins are already live);
  2. the npm trio under `--tag next`;
  3. registry acceptance with `source=registry` and
     `expect_version 0.1.0-rc.37`.

## Wrap mode W3 (Gradio and Streamlit adapters), on branch `wrap-w3`

W3 adds the framework adapters on top of the W1 launcher. Still no
user-facing surface: W2 owns config keys, `pylair wrap`, the window and
navigation. The launcher's command mode is unchanged.

What it adds:

- `python -m pylair.wrap --framework {command|gradio|streamlit}
  [--entry app.py] [--cmd <argv...>] [--ready-path P]`. Bare
  `--cmd` still means command mode; `--framework command --cmd` is
  identical. The other frameworks require `--entry` and reject `--cmd`.
- `pylair.wrap._gradio`: sets `GRADIO_ANALYTICS_ENABLED=False`, patches
  `gradio.Blocks.launch` so `server_name=127.0.0.1`,
  `server_port=$PYLAIR_PORT`, `inbrowser=False`, `share=False` and a
  blocking foreground launch win over anything the entry passes, then
  runs the entry via `runpy.run_path(run_name="__main__")`. Covers
  `Interface`/`ChatInterface` (they inherit `Blocks.launch`). Entries
  that never call `launch()` are served if they define a `demo` Blocks;
  anything else exits 4 with a named error.
- `pylair.wrap._streamlit`: invokes `streamlit.web.cli.main` (the
  console_script entry, stable across 1.x) with `run ENTRY
  --server.address 127.0.0.1 --server.port $PYLAIR_PORT --server.headless
  true --browser.gatherUsageStats false --server.fileWatcherType none
  --server.scriptHealthCheckEnabled true`, plus the
  `STREAMLIT_BROWSER_GATHER_USAGE_STATS=false` env. Multipage apps work:
  Streamlit finds `pages/` next to the entry itself.
- Health paths are per-framework so a slow page hydration is never a
  probe miss: Gradio `/config`, Streamlit `/_stcore/health`.
  `/_stcore/script-health-check` (enabled by the flag above) executes
  the entry once in a throwaway session; it exists for tests and later
  `doctor`, and is deliberately NOT the steady-state probe.
- Version policy lives in `pylair.wrap._shared`: supported majors are
  gradio 5.x and streamlit 1.x (the `wrap` test dependency group pins
  the same majors). An off-pin or unparseable version logs a warning
  and still runs; a missing framework is a clean exit-3 error. The
  frameworks come from the app's env, never from pylair's deps.

For W2: the adapters are plain `pylair.wrap` modules spawned as the
server command, so the existing launcher/supervisor wiring carries them
unchanged. If `doctor` wants a "does the entry run" check later,
`/_stcore/script-health-check` and gradio `/config` are the cheap
proofs. The IPC probe and navigation policy remain W2.

Test notes: `python/pylair/tests/test_wrap_adapters.py` plus the fixture
apps under `tests/wrap_apps/`; they need `uv sync --group wrap` and skip
without it, so the default suite stays light. CI runs them in the
`wrap-adapters` leg on Linux and Windows and as a suite of the `macOS
checks` job on macOS.

## CI layout (macOS runner diet)

macOS runners are capped at about five at a time, and a push used to ask
for sixteen (eleven in `ci.yml`, five in `acceptance-registry.yml`), so
jobs queued for fifteen minutes and timed out. Coverage is unchanged;
jobs were moved and merged (details in AGENTS.md, "Build / lint / test"):

- `ci.yml`: one `macOS checks` job (JS, Rust, Python, wrap-adapters suites
  in sequence) and one `wrap acceptance (macos-latest)` job (three
  frameworks in sequence); Linux and Windows keep one job per suite or
  framework. Python edge dropped macOS. The M0 spike is its own workflow,
  weekly and on demand.
- A `changes` job skips the heavy jobs for docs-only changes. Required
  checks are gated per step so they keep reporting (see AGENTS.md).
- `acceptance-registry.yml`: push runs Linux and Windows only; a manual
  dispatch keeps the whole matrix. A `plan` job builds the matrices.
  The wrap matrix dropped macOS too: three per-framework legs starved
  on the hosted arm64 pool (two were cancelled on acquisition in one
  day), so `wrap-acceptance-macos` runs command/gradio/streamlit in
  sequence via `scripts/wrap-acceptance/run-macos-registry.sh` — the
  same shape as ci.yml's consolidated job, parametrized on SOURCE
  (pack/registry) and EXPECT (expect_version). Dispatch macos-latest
  jobs: 4 → 2 (the bridge `acceptance` leg is the other).
- If macOS legs still starve, the next lever is the bridge `acceptance`
  job's two macOS legs (scaffold, reference), which were left as they were.

## Release order

Humans dispatch every publish; an agent's job stops at green CI and a
prepared bump commit (AGENTS.md, "Release rules").

Before starting: check the bundled-interpreter pin is current
(`PBS_TAG`/`PBS_VERSIONS`/`PBS_SHA256` in `packages/cli/src/build/pbs.ts`
against the newest python-build-standalone release). pip-audit does not
cover the bundled CPython.

1. crates.io: `tauri-plugin-pylair`.
2. PyPI: `pylair` and `pylair-ollama` (`release.yml`, `package` input).
3. Regenerate `packages/create-pylair/templates/default/src-tauri/Cargo.lock`
   against the published crate and commit it (the lock ships verbatim in
   the npm tarball; scaffold a registry-mode app, let cargo resolve, copy
   the lock back; never hand-edit it).
4. Move the learning examples in the same pins PR. Every
   `examples/learn/*/package.json`, `src-tauri/Cargo.toml`,
   `pyproject.toml` and `requirements.txt` must match the new compat tuple.
   Regenerate each `package-lock.json`; update each `uv.lock`; and in all
   three `src-tauri/Cargo.lock` files move the `tauri-plugin-pylair`
   version and checksum lines together. The cli suite's learning-example
   pin test is the tripwire.
5. Pack-mode acceptance CI green on that commit.
6. npm trio under `--tag next`.
7. Registry acceptance with `source=registry`.

Between a pin-bump commit and the actual publishes, pack-mode acceptance
is expected red (it resolves the pins from the live registries).

## Standing rules

- Report Linux results as Linux results. Platform-gated code (pdeathsig,
  process groups, Job Objects, ppid watchers) is proven only on its own
  OS leg; a green macOS run says nothing about Linux or Windows code.
- macOS is CI-tested only (README, Known limitations).
- `docs/protocol-v1.md` and `docs/cancellation.md` are contracts: change
  the doc in the same commit as the code.
- Do not publish npm until CI is green on the pins commit.

## Known flags for release notes

- `cancel()` returns a `CancelOutcome`, not a boolean (breaking for
  callers written as `if (await cancel(t))`). Introduced in the rc.6
  train; the template has no cancel usage.
- Windows: the runtime now starts suspended and is resumed after job
  assignment. If the host dies in the microseconds between spawn and
  assignment the suspended runtime is left behind as an inert process
  (`docs/security.md` section 7).

## Historical

The rc.6 handoff (Linux verification script, release order for
`0.1.0-rc.3` crate and PyPI, npm `0.1.0-rc.6`) is superseded and was
removed from this file; recover it from git history if needed.
