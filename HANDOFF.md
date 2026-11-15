# Handoff

Last rewritten 2026-11-14, against `main` at `a458e05` (after PR 57).
The previous contents were the rc.6 CI-fix handoff; that work landed long
ago and the document had gone stale. What is kept here is what is still
true: where the rc.16 train stands, the release order, and the standing
rules that AGENTS.md points at. Anything not listed here is in git history.

## Where the tree is

Manifests on `main` carry the rc.16 bump for the crate, `pylair` and
`pylair-ollama`; the npm packages are at rc.16 (checked in the files, not against
the registries; npm publishes after the pins PR merges):

| Package | Version in the repo |
|---|---|
| `tauri-plugin-pylair` (crate) | `0.1.0-rc.12` |
| `pylair` (PyPI) | `0.1.0-rc.10` |
| `pylair-ollama` (PyPI) | `0.1.0-rc.3` |
| `@pylair/client`, `@pylair/cli`, `create-pylair` (npm) | `0.1.0-rc.16` |

Merged for rc.16 so far, none of it published yet:

- PR 53, J: CI and supply-chain hardening (pinned actions, input validation).
- PR 55, L: polish (real `--version`, `OLLAMA_HOST` normalization, signing
  config warning, macOS CI-only note).
- PR 56, K1: runtime robustness (shared `grace_ms` shutdown budget,
  dedicated ping slot, bounded inbound queue with control priority,
  bounded `taskkill`).
- PR 57, K2: Windows runtime is created suspended, assigned to the Job
  Object, then resumed (`docs/design/windows-job-object-race.md`).
- PR M: test hygiene (flaky `stream_credit` tests, bytecode test env,
  this file, a duplicated heading in `docs/cancellation.md`).

The crate (0.1.0-rc.12), `pylair` (0.1.0rc10) and `pylair-ollama`
(0.1.0rc3) are published. `pylair-ollama` needed its own bump to ship the
`OLLAMA_HOST` normalization and publishes separately via `release.yml`
(`package` = `pylair-ollama`). The pins PR (scaffold pins, template
`Cargo.lock`, npm trio at 0.1.0-rc.16) is the last commit before the npm
publish; the steps below still describe the order.

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
