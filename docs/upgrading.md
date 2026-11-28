# Upgrading a Pylair app

How versioning works, how to see exactly what your app is behind on, and
how to upgrade — offline, without reading three changelogs.

## How versioning works

Pylair ships as three independent packages across three registries:

| Part | Registry | Pinned in |
|---|---|---|
| `@pylair/client` + `@pylair/cli` | npm | `package.json` (+ lockfile) |
| `tauri-plugin-pylair` | crates.io | `src-tauri/Cargo.toml` (+ `Cargo.lock`) |
| `pylair` | PyPI | `pyproject.toml` (+ `uv.lock`) |

**The numbers move independently and are not expected to match.** npm
`0.1.0-rc.36` pairs with crate `0.1.0-rc.28` and `pylair 0.1.0rc21` —
that is a compatible set, not skew. Each npm release tag names the crate
and PyPI versions that pair with it, and the CLI carries that mapping in
its bundled compatibility manifest (`packages/cli/src/compat.json` — the
one file the release train edits; scaffolds derive their pins from it).

A set that is *not* one release's combination is a real mismatch: the
bridge protocol, capabilities and wrap launcher evolve together, so a
crate ahead of or behind the npm/PyPI pair can misbehave in ways that
look like application bugs.

## `pylair doctor` — the Versions section

`pylair doctor` reports what your app actually uses: the declared
requirement plus the lockfile-resolved exact version per ecosystem.
Missing lockfiles read as *unresolved* (never guessed — install first);
`file:`/`path` dependencies read as local checkouts. The compatibility
line then says one of:

- **a compatible set** — every component matches one release's tuple
  (subsets are fine: a wrap app may not use the client);
- **mismatched** — not any single release's combination, naming which
  part is out of step and the exact version that pairs with the rest;
- **unjudged** — something is unresolved, so no verdict rather than a
  wrong one.

## `pylair upgrade --check`

Read-only (it never edits files). Compares your set with the newest set
the CLI's manifest knows:

```sh
npx pylair upgrade --check          # offline
npx pylair upgrade --check --online # also ask the npm registry
```

For an older-but-consistent set it prints:

- the exact before/after lines to change in `package.json`,
  `src-tauri/Cargo.toml` and `pyproject.toml` — only for components that
  actually change;
- the commands that re-resolve the lockfiles: `npm install` (or
  `pnpm install`), `cargo update -p tauri-plugin-pylair` in `src-tauri`,
  and `uv lock` (then `uv sync`);
- the **manual upgrade notes** for every release between yours and the
  newest — template-level steps a version bump does not do for you
  (e.g. the rc.29 CSP change for blob/media payloads).

Exit codes: `0` up to date · `1` upgrade available · `2` mismatched or
unresolvable — so `pylair upgrade --check && echo current` does the
right thing in scripts.

`--online` additionally checks the npm registry. If the registry has a
newer CLI than the one you are running, the bundled manifest itself is
behind: update the CLI first (`npm install -D @pylair/cli@<version>`,
or wherever you invoke it from), then re-run — its manifest knows the
newer set.

## A manual upgrade walkthrough

Starting from an app scaffolded at rc.28 (npm `0.1.0-rc.28`, crate
`0.1.0-rc.22`, `pylair 0.1.0rc14`):

1. `npx pylair upgrade --check` — read the output. It shows the app's
   set, the newest set, and the per-file edits.
2. `package.json`: bump `"@pylair/client"` and `"@pylair/cli"` to the
   printed npm version, then `npm install`.
3. `src-tauri/Cargo.toml`: bump the `tauri-plugin-pylair` pin (the `=`
   exact pin is deliberate — re-resolution surprises are what lockfiles
   are for), then `cargo update -p tauri-plugin-pylair` in `src-tauri`.
4. `pyproject.toml`: bump `pylair==…`, then `uv lock && uv sync`.
5. Apply the printed manual notes for the in-between releases. These are
   the steps automation cannot do: template CSP changes, config renames,
   anything with a judgment call. If you customized the file a note
   refers to, diff it against a fresh scaffold
   (`npx create-pylair reference-app`) instead of copying blindly.
6. `npx pylair doctor` — the Versions section should now read *a
   compatible set* for the newest release. Then `npx pylair dev` and
   click through the app: versions are a contract check, not a behavior
   proof.

## What the manifest is (for maintainers)

`packages/cli/src/compat.json` — one JSON file, vendored byte-identical
into create-pylair at build time (`scripts/sync-ui.mjs`; a parity test
fails on any drift). For each npm release tag: the crate and PyPI
versions of the compatible set, plus free-text upgrade notes. The
scaffold pins derive from its `current` entry, so a release bump edits
exactly this file (the template `Cargo.lock` regeneration is the train's
separate mechanical step, pinned by the same tripwire tests as before).
