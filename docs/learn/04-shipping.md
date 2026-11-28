# 4. Ship Inkwell as an installed app

**What you'll build:** a native Inkwell installer for your operating system,
with Python and the locked Python dependencies bundled inside. You will install
it, launch it, and understand what signing and auto-update would add.

**Time:** about 25 minutes, plus the first Rust compile.

Start from your guide 3 app, or fetch
[`examples/learn/04-shipping/`](../../examples/learn/04-shipping/) with the
[sparse-checkout command](README.md#jumping-in-without-the-repo) and run
`npm install` and `uv sync` inside it.

## The application state

Guide 4 does not change Inkwell's application code. Shipping should first
prove the app you already tested, so the complete guide 4 state is the same
backend, UI and dependency pins as the end of guide 3. The difference is what
you do with it.

## Build an installer

Run:

```sh
npx pylair build
```

The first build downloads a standalone CPython build and Tauri's installer
toolchain, then compiles the Rust shell. Later builds reuse both caches. A
trimmed real Windows run looked like this:

```text
building inkwell 0.1.0 for win32/x64 (x86_64-pc-windows-msvc)
note: pylair builds native bundles only; run this on each target OS
-> bundle python
fetching cpython-3.12.16+20261110-x86_64-pc-windows-msvc-install_only_stripped.tar.gz
[ok] bundle python (21.3s)
-> install deps
Resolved 25 packages in 231ms
Installed 25 packages in 597ms
[ok] install deps (1.5s)
-> codegen
[ok] codegen (2.1s): src/api.generated.ts
-> stage app
third-party notices: pylair, python interpreter, 25 wheel(s)
[ok] stage app (0.3s)
-> prune
pruned 6 paths (1.7 MiB) from staged resources
[ok] prune (0.2s)
-> compile bytecode
[ok] compile bytecode (3.3s)
-> smoke test
[ok] smoke test (1.0s): imports backend.app
-> build installer
   Finished `release` profile [optimized] target(s) in 2m 02s
    Finished 2 bundles at:
        bundle/msi/inkwell_0.1.0_x64_en-US.msi (56.09 MiB)
        bundle/nsis/inkwell_0.1.0_x64-setup.exe (36.02 MiB)
[ok] build installer (4m 57s)

built inkwell 0.1.0 in 5m 26s
   56.1 MiB  bundle/msi/inkwell_0.1.0_x64_en-US.msi
   36.0 MiB  bundle/nsis/inkwell_0.1.0_x64-setup.exe
UNSIGNED BUILD: Gatekeeper and SmartScreen will warn users (see docs/build.md#signing)
```

The important stages are:

1. **bundle python** fetches the pinned python-build-standalone interpreter
   for the host platform and verifies its committed hash.
2. **install deps** exports the frozen `uv.lock` set and installs it with the
   bundled interpreter. This is the same locked dependency set `pylair dev`
   uses, not whatever happens to be installed globally.
3. **codegen** regenerates the typed client against the staged backend.
4. **stage app** copies the backend source and writes third-party notices.
5. **prune** removes tests, caches and other files that should not ship.
6. **compile bytecode** precompiles the staged Python tree.
7. **smoke test** imports `backend.app` with the bundled interpreter, so an
   over-pruned or missing dependency fails on the build machine.
8. **build installer** runs the normal Tauri production build and bundler.

The installer lands under `src-tauri/target/release/bundle/`. On this Windows
run the installers were:

```text
src-tauri/target/release/bundle/msi/inkwell_0.1.0_x64_en-US.msi
src-tauri/target/release/bundle/nsis/inkwell_0.1.0_x64-setup.exe
```

On macOS look under `bundle/dmg/` or `bundle/macos/`; on Linux look under
`bundle/appimage/` or the enabled Linux bundle directory. Install or run the
artifact the way your OS normally handles it, then launch Inkwell from the
installed copy rather than with `npx pylair dev`.

## Where the model appears

The embedding model is not inside the installer. `models.model_snapshot` is a
runtime declaration: the first installed launch downloads the pinned revision
into the app data directory, verifies it, and only then marks the `embedder`
component ready.

That is deliberate. The installer carries application code and Python library
dependencies, while large runtime artifacts live in the per-user data dir.
They can be shared across reinstalls, replaced independently, and verified or
resumed without rebuilding the app. The trade-off is that first launch needs
network access unless the machine already has the snapshot. Disk layout and
upgrade behavior are in [docs/models.md](../models.md).

## Signing, at the first-timer level

You do not need signing credentials to finish this guide. An unsigned build
is still a real installer, but each OS treats it cautiously:

- **Windows:** SmartScreen may warn because the publisher is unknown. A test
  user can choose more information and run anyway; do not train users to
  ignore warnings as a release process.
- **macOS:** Gatekeeper warns for an unidentified developer. Pylair still
  ad-hoc signs the whole `.app` when no identity is configured so the bundle
  is self-consistent, but ad-hoc signing is not Developer ID trust.
- **Linux:** there is no equivalent platform publisher check for an AppImage;
  users decide whether to trust the download source.

`pylair build --sign` is about updater artifacts. It forces Tauri to create
signed update artifacts and requires `TAURI_SIGNING_PRIVATE_KEY` in the
environment. It does not magically create an operating-system signing
identity, and it does not remove SmartScreen or Gatekeeper warnings by itself.
OS trust uses the platform credentials described in
[docs/build.md](../build.md#signing): a Developer ID identity and notarization
on macOS, or a Windows certificate or Azure Trusted Signing setup.

## Optional: auto-update

Auto-update is opt-in and needs somewhere to host release artifacts. The
simplest path is to choose it when scaffolding a new app:

```sh
npx create-pylair my-app --with-updater
```

That flag is scaffold-only. For an existing app such as Inkwell, the current
path is a manual edit following [docs/updates.md](../updates.md):

- add `tauri-plugin-updater` and the `updater` feature on
  `tauri-plugin-pylair` in `src-tauri/Cargo.toml`;
- register `tauri_plugin_updater` and the hidden watchdog entry in
  `src-tauri/src/lib.rs`;
- add `bundle.createUpdaterArtifacts` and `plugins.updater` to
  `src-tauri/tauri.conf.json`;
- add the updater permissions to `src-tauri/capabilities/default.json`;
- keep private keys outside the repository.

Then generate update keys:

```sh
npx pylair keys init --key-path ~/keys/inkwell-updater.key
```

`keys init` writes the public key into `plugins.updater.pubkey` and creates a
minisign private key at the path you chose. Keep that private key in CI or a
password manager, never in the app repository. Build signed artifacts with:

```sh
TAURI_SIGNING_PRIVATE_KEY=... npx pylair build --sign
```

Then publish a manifest and artifacts:

```sh
npx pylair update manifest \
  --base-url https://github.com/ORG/REPO/releases/latest/download \
  --notes "what's new"
```

The manifest command scans the signed artifacts, checks versions and
signatures, and writes `latest.json`. Upload that file, the artifacts and
their `.sig` files to the hosting location named by the updater endpoints.
GitHub Releases is enough; no update server is required.

The rollback watchdog is the safety piece. Before an update installs, the app
caches the signed artifact for the version being replaced and starts a hidden
watchdog copy of itself. The new version must reach readiness within the
configured verification window. If it crashes, reports a failed component or
goes silent, the watchdog verifies the cached rollback artifact, marks the
bad version skipped, restores the previous install and relaunches it. That is
why rollback hosting matters: every release needs its own `latest.json` as
well as the moving latest manifest. The exact lifecycle is in
[docs/updates.md](../updates.md).

## What you learned

> - `pylair build` bundles CPython, the locked Python dependencies, app
>   source, generated types and the production UI into a native installer.
> - The smoke test catches missing or over-pruned Python dependencies before
>   the installer leaves the build machine.
> - Model snapshots download on first launch into the app data directory,
>   not at build time.
> - Unsigned builds are fine for local testing but trigger platform trust
>   warnings; OS signing and updater signing are separate concerns.
> - Auto-update is opt-in, needs artifact hosting, and includes a watchdog
>   that can roll back a release which never becomes ready.
>
> **Next:** [guide 5](05-going-further.md) adds a bundled document-export
> binary, cancellation and Blob results. The complete state for this guide is
> in [`examples/learn/04-shipping/`](../../examples/learn/04-shipping/).
