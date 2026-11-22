# Contributing to Pylair

Thanks for helping. Pylair is a release candidate: correctness and honest
documentation matter more than speed. Small, focused pull requests are
the norm here.

## Dev setup

Toolchain, all pinned in the repo:

- **Node 22** (`.nvmrc`) and **pnpm 10.18.3** (`packageManager` in the root
  `package.json`, via corepack)
- **Rust 1.92.0** (`rust-toolchain.toml`, with rustfmt and clippy)
- **uv** and **Python 3.12** (`requires-python >= 3.12`)
- On Windows, the Visual Studio Build Tools "Desktop development with
  C++" workload
- On Linux, Tauri's system deps:
  `sudo apt-get install -y libwebkit2gtk-4.1-dev libappindicator3-dev librsvg2-dev patchelf`

```sh
pnpm install            # JS workspace deps
pnpm build              # build all TypeScript packages
cargo build             # Rust workspace
cd python/pylair && uv sync   # Python env (fetches managed CPython)
```

## Running tests

These are exactly what CI runs:

```sh
pnpm typecheck          # tsc --noEmit, all packages
pnpm test               # node --test, all packages

cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test

cd python/pylair
uv run ruff check src tests
uv run ruff format --check src tests
uv run pytest -q
```

Heavier checks (`acceptance` jobs in `.github/workflows/ci.yml`) build and
install real packages on clean machines; they run in CI rather than
locally.

## How CI treats docs-only changes

`scripts/ci-changes.mjs` decides whether a diff is documentation only:
every changed path must be under `docs/`, a `*.md`/`LICENSE*` file at the
repository root, or a `*.md` file under `.github/`. Anything else, or any
trouble working the diff out, runs the full suite. Markdown elsewhere is
not docs-only: package READMEs ship in the npm tarballs and
`python/pylair/README.md` is the PyPI long description.

## Pull requests

- **Small and focused.** One concern per PR, every PR includes tests.
- **Conventional commit prefixes** as in `git log`: `feat:`, `fix:`,
  `chore:`, `test:`, `docs:`, `style:`.
- **Contracts move with code.** `docs/protocol-v1.md` and
  `docs/cancellation.md` describe what the supervisor, the Python runtime
  and the client actually do; change the doc in the same commit as the
  code.
- **Docs style: plain prose, no em dashes.** Keep sentences honest and
  short.
- **Keep "Known limitations" honest.** If your change removes a
  limitation, adds one, or changes a guarantee, update `README.md` and
  `docs/security.md` in the same PR.
- **No secrets.** Do not commit credentials, signing certs or API keys.

For security issues, please do not open a public PR: see
[SECURITY.md](SECURITY.md).
