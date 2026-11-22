# 0000: Repository and toolchain decisions

## Context

Kickoff decisions for the monorepo scaffold. Sources: docs/maintainers/PROJECT_BRIEF.md plus
the first owner Q&A (2026-11-11).

## Decisions

- **Monorepo layout** per docs/maintainers/PROJECT_BRIEF.md section 6: `crates/` (Cargo
  workspace), `packages/` (pnpm workspace), `python/` (uv-managed),
  `integrations/`, `templates/`, `examples/`, `docs/decisions/`.
- **Toolchain pins**: Node 22 (`.nvmrc`), pnpm 10.18.3 (`packageManager`
  field, corepack-compatible), Rust 1.92.0 (`rust-toolchain.toml` with
  rustfmt + clippy), Python 3.12 (`.python-version`, resolved via uv).
- **Templates excluded from the pnpm workspace**: scaffolded apps must
  install from the registry standalone, not ride the monorepo lockfile.
- **Naming**: `product.json` at repo root is the single source of truth for
  product name, CLI binary, package scope. Directory/package names still
  embed `pylair`; renaming later is a mechanical pass. `Pylair` is a
  working title — see docs/maintainers/PROJECT_BRIEF.md section 1.
- **License**: deferred by owner. No LICENSE file; packages marked
  `private`/unpublished (`publish = false` on crates, `private` root
  package). Revisit before any public release.
- **Linux installer**: AppImage first for clean-machine CI tests (no root
  needed, self-contained); deb added after M0.
- **Minimum OS versions**: macOS 12+, Windows 10 1809+, Ubuntu 22.04
  (glibc 2.35 baseline). CI matrix: `macos-latest`, `windows-latest`,
  `ubuntu-22.04`.

## Consequences

- Every tool reading product naming should go through `product.json`; the
  rename cost is concentrated in directory/package names, which a script
  can rewrite.
- `uv sync` in `python/pylair` downloads a python-build-standalone
  interpreter — the same distribution mechanism M0 bundles, so local dev
  dogfoods the packaging path from day one.
- Rust crate is a compilable stub with no dependencies; real plugin deps
  (tauri, tokio, serde) arrive with M0/M1 to keep early CI fast.
