# Handoff

Last rewritten 2026-11-14, against `main` at `a458e05` (after PR 57).
The previous contents were the rc.6 CI-fix handoff; that work landed long
ago and the document had gone stale. What is kept here is what is still
true: where the rc.18 train stands, the release order, and the standing
rules that AGENTS.md points at. Anything not listed here is in git history.

## Where the tree is

rc.17 is published: the crate at `0.1.0-rc.13`, `pylair` `0.1.0rc10`,
`pylair-ollama` `0.1.0rc3` and the npm trio at `0.1.0-rc.17`.

rc.18 is in preparation. Versions in the repo (checked in the files, not
against the registries) and the plan for each:

| Package | Version in the repo | rc.18 plan |
|---|---|---|
| `tauri-plugin-pylair` (crate) | `0.1.0-rc.14` | released as `0.1.0-rc.14` |
| `pylair` (PyPI) | `0.1.0-rc.10` | unchanged, not released |
| `pylair-ollama` (PyPI) | `0.1.0-rc.3` | unchanged, not released |
| `@pylair/client`, `@pylair/cli`, `create-pylair` (npm) | `0.1.0-rc.17` | released as `0.1.0-rc.18`; the bump lands in the pins PR |

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
- rc.18 version bump: crate `0.1.0-rc.14`; the reference app's `uv.lock`
  needed no refresh (`uv lock --check` clean).

`python/` and `integrations/` have not changed since rc.16, so step 2
below is skipped for rc.18. The pins PR (scaffold pins, template
`Cargo.lock` against the published crate, npm trio at `0.1.0-rc.18`, the
README and npm README install lines) follows the crate publish; the steps
below still describe the order.

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
