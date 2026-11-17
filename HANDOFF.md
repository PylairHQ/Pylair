# Handoff

Last rewritten 2026-11-17, against `main` at `9ffffab` (after PR 89).
The previous contents were the rc.6 CI-fix handoff; that work landed long
ago and the document had gone stale. What is kept here is what is still
true: where the rc.20 train stands, the release order, and the standing
rules that AGENTS.md points at. Anything not listed here is in git history.

## Where the tree is

rc.17 is published: the crate at `0.1.0-rc.13`, `pylair` `0.1.0rc10`,
`pylair-ollama` `0.1.0rc3` and the npm trio at `0.1.0-rc.17`.

rc.18 is published: the crate `0.1.0-rc.14` on crates.io and the npm
trio at `0.1.0-rc.18`.

rc.19 is published: npm trio at `0.1.0-rc.19` (npm-only; the crate and
PyPI were unchanged). It shipped the bundled-CPython bump from PR 84:
python-build-standalone `20261110`, CPython `3.12.16` / `3.13.18`.

rc.20 train is in progress: the crate `0.1.0-rc.15` is published on
crates.io (PR 89 bumped it), and this pins PR moves everything else. It
is a full train (same shape as PRs 82/83), changing the crate for
per-function permissions (PR 88): the crate gains `fn_scope` (command
scopes on `python_call` and `python_call_stream`, `PERMISSION_DENIED`
`-32050`); `@pylair/client` gains `PermissionDeniedError` and the
self-test's `fn-permission` check; `@pylair/cli` gains the capability
checks in `pylair build` and `pylair doctor`. Versions in the repo
(checked in the files, not against the registries) and the state of each:

| Package | Version in the repo | rc.20 state |
|---|---|---|
| `tauri-plugin-pylair` (crate) | `0.1.0-rc.15` | published |
| `pylair` (PyPI) | `0.1.0-rc.10` | unchanged, skipped (Python unchanged) |
| `pylair-ollama` (PyPI) | `0.1.0-rc.3` | unchanged, skipped |
| `@pylair/client`, `@pylair/cli`, `create-pylair` (npm) | `0.1.0-rc.20` | next to publish |

Scaffold pins for rc.20: `tauri-plugin-pylair = "=0.1.0-rc.15"`,
`pylair==0.1.0rc10`, `@pylair/client` and `@pylair/cli` `0.1.0-rc.20`;
the template `Cargo.lock` carries the crates.io checksum for rc.15 (the
plugin entry is the only line that moved). Remaining order: pack-mode
acceptance green on the pins commit, then the npm trio under `--tag
next`, then registry acceptance with `source=registry`. PyPI (step 2
below) is skipped for rc.20.

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

- PR 88: per-function permissions (crate `fn_scope`, client
  `PermissionDeniedError`, CLI capability checks, self-test
  `fn-permission` check, `docs/decisions/0004-per-function-permissions.md`).

`python/` and `integrations/` have not changed since rc.16, so step 2
below is skipped for rc.20.

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
