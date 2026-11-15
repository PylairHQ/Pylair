# Handoff

Last rewritten 2026-11-14, against `main` at `a458e05` (after PR 57).
The previous contents were the rc.6 CI-fix handoff; that work landed long
ago and the document had gone stale. What is kept here is what is still
true: where the rc.17 train stands, the release order, and the standing
rules that AGENTS.md points at. Anything not listed here is in git history.

## Where the tree is

rc.16 is published: the crate at `0.1.0-rc.12`, `pylair` `0.1.0rc10`,
`pylair-ollama` `0.1.0rc3` and the npm trio at `0.1.0-rc.16`.

rc.17 is in progress. Steps 1 and 3 are done: the crate `0.1.0-rc.13` is
published on crates.io, and the pins PR has regenerated the template
`Cargo.lock` against it. Next: pack-mode acceptance on the pins commit,
then the npm trio at `0.1.0-rc.17`, then registry acceptance. Versions in
the repo (checked in the files, not against the registries) and the state
of each:

| Package | Version in the repo | rc.17 state |
|---|---|---|
| `tauri-plugin-pylair` (crate) | `0.1.0-rc.13` | published |
| `pylair` (PyPI) | `0.1.0-rc.10` | unchanged, not released |
| `pylair-ollama` (PyPI) | `0.1.0-rc.3` | unchanged, not released |
| `@pylair/client`, `@pylair/cli`, `create-pylair` (npm) | `0.1.0-rc.17` | to publish after the pins PR merges |

Scaffold pins for rc.17: `tauri-plugin-pylair = "=0.1.0-rc.13"`,
`pylair==0.1.0rc10`, `@pylair/client` and `@pylair/cli` `0.1.0-rc.17`.

Merged since rc.16 (76ebcdc):

- PR 62, R1: `supervisor.rs` split into `crates/tauri-plugin-pylair/src/supervisor/`
  (pure move).
- PR 63, R2: client self-test moved to `src/selftest.ts`, bridge plumbing
  to `src/internal.ts` (pure move).
- PR 64: `stream_credit` retry waits on runtime reads, not a timer (test only).
- PR 65, R2b: the client loads the installed-webview self-test on demand.
- PR 66, R3: CLI `build.ts` split into `src/build/` (pure move).
- PR 67: the CLI resolves `--dir` to an absolute path for build, dev and
  doctor.
- PR 68, rc.17 version bump: crate `0.1.0-rc.13`; the reference app's
  `uv.lock` refreshed (it still pinned its two local packages at `0.0.0`).
- rc.17 pins: scaffold pins, template `Cargo.lock` against the published
  crate, npm trio at `0.1.0-rc.17`, README and npm README install lines.

`python/` and `integrations/` have not changed since rc.16, so step 2
below is skipped for rc.17.

## Release order

Humans dispatch every publish; an agent's job stops at green CI and a
prepared bump commit (AGENTS.md, "Release rules").

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
