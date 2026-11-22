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

`python/` and `integrations/` had not changed since rc.16 until app
readiness (PR 129) — the next train's PyPI step is back on; step 2
below was skipped for rc.20 only.

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
4. Pack-mode acceptance CI green on that commit.
5. npm trio under `--tag next`.
6. Registry acceptance with `source=registry`.

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
