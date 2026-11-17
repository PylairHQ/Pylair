# AGENTS.md — working agreements for coding agents

Read `PROJECT_BRIEF.md` first — it defines goals, milestones, and the
working agreements (section 11) that apply to every change. This file is
the operational layer: layout, commands, invariants, and release rules.
Everything below is derived from the repo itself — if a fact here
disagrees with the code or a workflow file, the code wins; fix this file.

## What this is

Pylair is a desktop framework for Python and local-AI applications: a
Tauri shell + a managed, bundled CPython runtime + a typed bridge
(`@expose`d Python functions callable from TypeScript), with streaming,
cancellation, crash supervision and one-command installers for Windows,
macOS and Linux. See README.md for status; it is a release candidate.

Naming lives in `product.json` — read it in tooling, never hard-code.

## Layout

| Path | What |
|---|---|
| `crates/tauri-plugin-pylair` | Rust plugin: supervisor, stdio protocol, Tauri commands + permissions |
| `python/pylair` | `pylair` PyPI package: `@expose`, protocol server, task runner, workers, and the wrap-mode launcher (`pylair.wrap`) |
| `packages/client` | `@pylair/client` — typed TS calls, `AsyncIterable` streams, cancel |
| `packages/cli` | `pylair` CLI — dev, build, doctor, codegen |
| `packages/create-pylair` | scaffolder (`create-pylair`) + `templates/` |
| `templates/` | scaffold templates — **not** a pnpm workspace member (see below) |
| `examples/reference-app` | the M6 acceptance app — **not** a workspace member (own lockfile, simulates a real standalone install) |
| `docs/` | protocol-v1, cancellation, codegen, build, security, wrap-mode brief, `decisions/` records, `ideas.md` |
| `integrations/ollama` | optional `pylair-ollama` package (never a core dependency) |
| `scripts/` | `dogfood-windows.ps1` — lifecycle containment dogfood |
| `spikes/` | `m0-packaging`, `w0-wrap` — throwaway spikes kept for evidence |
| `.github/workflows/` | `ci.yml`, `acceptance-registry.yml`, `publish-crates.yml`, `release.yml` (PyPI), `publish-npm.yml` |

## Toolchain (all pinned in-repo)

Node 22 (`.nvmrc`), pnpm 10.18.3 (`packageManager` in root package.json,
via corepack), Rust 1.92.0 (`rust-toolchain.toml`, rustfmt + clippy),
uv + Python 3.12 (`requires-python >= 3.12`, PEP 695 syntax — CI installs
3.12 explicitly: cli codegen tests spawn `python -m pylair.codegen`, e2e
tests spawn the real runtime).

## Build / lint / test — exactly what CI runs

CI is `.github/workflows/ci.yml` (three OSes per leg: `ubuntu-22.04`,
`windows-latest`, `macos-latest`). Local commands are identical.

**JS (all `packages/*`):**

```sh
pnpm install --frozen-lockfile
pnpm build       # pnpm -r --if-present build      → tsc -p tsconfig.json
pnpm typecheck   # pnpm -r --if-present typecheck  → tsc --noEmit
pnpm test        # pnpm -r --if-present test       → node --test
```

**Rust (`crates/`):**

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo build
cargo test
```

On Linux, Tauri needs system deps first:
`sudo apt-get install -y libwebkit2gtk-4.1-dev libappindicator3-dev librsvg2-dev patchelf`

**Python — run inside each package dir (`python/pylair`,
`integrations/ollama`):**

```sh
uv sync --frozen
uv run ruff check src tests
uv run ruff format --check src tests
uv run pytest -q
```

The W0 spike test also runs in CI: `cd spikes/w0-wrap && uv run --no-project python test_kill_tree.py`.

**Heavier checks** (don't run casually — they build installers):
`acceptance` job in `ci.yml` scaffolds an app and proves the shipped
installer on a clean machine; `acceptance-registry.yml` does the same
from packed tarballs (`source=pack`, default) or the live registry
(`source=registry`).

## Invariants — do not break these

- **`docs/protocol-v1.md` and `docs/cancellation.md` are contracts.**
  Change the doc in the same commit as the code — never one without the
  other. The supervisor, the Python runtime and the client all implement
  what these files say.
- **Registry versions are immutable.** A version once published to
  crates.io / PyPI / npm cannot be overwritten — never republish a
  version with different content; bump it. Publish workflows only verify
  that the repo manifest already equals the input version and tag the
  commit after a successful upload; versions are bumped by normal commits.
- **Scaffold pins + tripwire tests.** `packages/create-pylair/src/index.ts`
  pins the registry versions a scaffolded app gets (`__PYLAIR_PY__`,
  `__PYLAIR_RS__`). `packages/create-pylair/test/index.test.js` tripwires
  them: the generated `pyproject.toml` must carry the pinned
  `pylair==…`, and the template's shipped `src-tauri/Cargo.lock` must
  contain `tauri-plugin-pylair` at the pinned crate version. Bump the
  crate/PyPI/npm manifests, the scaffolder pins, and the lock **together**
  or `pnpm test` goes red.
- **Template `Cargo.lock` regenerates against the published crate.**
  It ships verbatim inside the create-pylair npm tarball. Procedure
  (release train step): after the new crate version is live on
  crates.io, scaffold a registry-mode app (`npx create-pylair@next`),
  let cargo resolve `tauri-plugin-pylair` there, copy the resolved
  `src-tauri/Cargo.lock` back over
  `packages/create-pylair/templates/default/src-tauri/Cargo.lock`,
  commit it. Never hand-edit the lock.
- **Workspace exclusions are deliberate.** `templates/*` and
  `examples/reference-app` are NOT pnpm workspace members — scaffolded
  apps must resolve standalone, and reference-app simulates what a user
  actually installs. Do not add them to `pnpm-workspace.yaml`.
- **Supervisor/protocol changes need fault-injection tests**, not just
  happy-path (PROJECT_BRIEF §11). Existing harnesses:
  `crates/tauri-plugin-pylair/tests/{supervisor_faults,lifecycle_stress,protocol_fuzz,limits,event_scoping,exact_outcomes,window_destroyed,hardening_step0,release_spec,plugin_setup}.rs`.
- **Keep README "Known limitations" honest.** If your change removes a
  limitation, adds one, or changes a guarantee (cancellation mode,
  ownership scoping, limits, containment), update README and
  `docs/security.md` in the same PR.

## Release rules — humans dispatch publishes, never agents

- `publish-crates.yml`, `release.yml` (PyPI) and `publish-npm.yml` are
  `workflow_dispatch`-only for a reason. **Agents never dispatch them,**
  and don't change publish auth, secrets or environments
  (OIDC trusted-publisher config, repo environments/secrets — crates.io
  publishes use OIDC trusted publishing, no registry token secret)
  unless the owner explicitly asks for that change.
- **The release train order** (from HANDOFF.md): crates.io
  `tauri-plugin-pylair` → PyPI `pylair` / `pylair-ollama` (release.yml's
  `package` input) → regenerate + commit the template `Cargo.lock` →
  pack-mode acceptance CI green on that commit → npm trio under
  `--tag next` → registry acceptance `source=registry`. A human performs
  each publish step; an agent's job stops at green CI and a prepared
  bump commit.
- CI signing uses repository secrets the owner configures; until then
  builds ship unsigned — don't commit certs, keys or tokens.

## CI gotchas (learned the hard way)

- **Windows `pwsh` steps only report the LAST native command's exit
  code.** A failed `ruff`/`uv`/`pnpm` mid-block is masked if a later
  command succeeds (a real CI escape — see the `shell: bash` comments in
  ci.yml and acceptance-registry.yml). Use `shell: bash` for multi-command
  steps (it runs `-e`), or check exit codes explicitly like the existing
  pwsh steps do (`$inst.ExitCode`, `$LASTEXITCODE`). `Start-Process`
  never propagates exit codes — capture `-PassThru` and check.
- **Pack-mode acceptance resolves the scaffolded app's pinned versions
  from the LIVE registries.** Between a pin-bump commit and the actual
  publishes, `uv sync`/`cargo` can't find the pins — pack-mode is
  expected-red in that window. Green returns only after the crate + PyPI
  publishes land (hence the train order).
- **macOS legs starve.** `macos-latest` runners queue and crawl; a slow
  or late-starting macOS leg is normal capacity starvation, not a
  failure signal — wait before declaring a leg dead.
- The second Windows leg under `pwsh` in acceptance-registry exists to
  catch tar-ordering bugs (git-bash resolves GNU tar, pwsh resolves
  System32 bsdtar). Don't merge it away.

## Containment & lifecycle tests — what "no orphans" means here

- `tests/window_destroyed.rs` + the installed-app
  `PYLAIR_SELF_TEST_WINDOW_CLOSE=1` self-test (all three OS legs in both
  acceptance workflows) cover main-thread `WindowDestroyed` — the rc.3
  window-close panic was found by manual dogfood; it stays covered.
- `PYLAIR_SELF_TEST_WEBVIEW=1` drives the real bridge end-to-end
  (typed call, ordered stream, cancellation) on installed apps.
- `scripts/dogfood-windows.ps1` runs the lifecycle dogfood: clean close,
  hard kill, restart — and asserts no survivor processes. CI runs it in
  tests-only mode (`-ExePath`) on the Windows acceptance leg; run the
  full mode locally for a shipped-app check.
- **Platform-gated code is only proven on its own OS leg.**
  `PR_SET_PDEATHSIG`/process-group kills (Linux), Job Objects (Windows),
  ppid watchers (POSIX) — a green macOS run says nothing about Linux
  code. Report Linux results as Linux results; the ubuntu legs are the
  proof (HANDOFF.md's standing rule).

## Commits and PRs

- Conventional prefixes as in `git log` (`feat:`/`fix:`/`chore:`/`test:`/
  `docs:`/`style:`), one concern per commit; milestone-internal prefixes
  like `RC.8A:` appear for in-flight hardening batches.
- Small, focused PRs — one concern each, every PR includes tests.
- Do not commit secrets, signing certs or API keys. Build unsigned until
  the owner wires signing secrets.
- Decisions that aren't obvious go in `docs/decisions/` as a short
  numbered record; ideas that aren't in scope go in `docs/ideas.md`.
