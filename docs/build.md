# pylair build

`pylair build` turns a pylair app into a signed or unsigned native installer
with Python bundled inside. It is the M0 packaging recipe made generic —
see `docs/decisions/0001-python-packaging.md` for why
python-build-standalone + `uv pip install --target` was chosen.

## Pipeline

```
pyproject.toml [tool.pylair] + src-tauri/tauri.conf.json
        │
        ▼
1. fetch python-build-standalone CPython for the HOST triple
   (SHA256-verified against hashes committed in the CLI — a checksum
   fetched from the same release only catches corruption; tarball
   cached in .pylair/cache/ between builds)
2. uv export --frozen from uv.lock → uv pip install --target
   src-tauri/resources/site-packages using the bundled interpreter —
   the SAME locked, hashed set `pylair dev` syncs (requirements.txt is
   the no-lockfile fallback, with a drift warning)
3. copy the app source dir into src-tauri/resources/
4. prune the staged tree (default deny-list + app additions)
5. precompile bytecode for site-packages and the app source with the
   bundled interpreter (checked-hash pycs — see below)
6. import-check the app module with the staged tree
7. npx tauri build
8. warn (one line, linking the Signing section) if no signing
   credentials are configured
```

## What the output looks like

On an interactive terminal `pylair build` prints a short header (app, version,
platform), one line per stage with its time (`bundle python`, `install deps`,
`stage app`, `prune`, `compile bytecode`, `smoke test`, `build installer`), the
uv and tauri output folded into one live progress line, and a summary listing
each installer with its size. If a tool fails its whole captured log is printed
before the error.

- `--verbose` streams the uv and tauri output as it comes instead of folding it
  (it is still passed on to `tauri build`).
- `--plain` (any command) prints plain text: no colour, symbols, spinner or
  folding. That is also what you get whenever stdout is not a terminal, in CI
  (`CI` set), with `NO_COLOR` set, or with `TERM=dumb`. `FORCE_COLOR=1` turns the
  styled output on anyway; `FORCE_COLOR=0` turns it off.
- Symbols are Unicode where the console can show them and ASCII (`[ok]`, `[x]`,
  `[!]`) otherwise. On Windows that means Unicode in Windows Terminal or after
  `chcp 65001`, ASCII in the classic PowerShell/cmd console.
- Plain output keeps its wording where something reads it: the `pylair
  <command>: message` error lines, and the `pylair doctor` check lines
  (` ok `/`FAIL`, name, detail), which now carry a `> file` and `> fix` line
  under each failure.

## Wrap mode

`[tool.pylair] mode = "wrap"` builds the same installer for an existing
Python web app instead of a typed bridge (`docs/wrap-mode-brief.md`). The
pipeline is the one above with these differences:

- **Config.** `[tool.pylair.wrap]` (`framework`, `entry`, `cmd`,
  `ready-path`, `ready-timeout`, `downloads`) is validated before anything is
  downloaded; every error names the key and what is allowed. `app-module` is
  not used. `source` (default `app`) is the directory the server runs in.
- **Step 3** stages the source dir as `resources/app/` whatever it is called
  (`"resources/app": "app/"` in `tauri.conf.json` never changes), and writes
  `resources/pylair-wrap.json`: the launcher command line
  (`python -m pylair.wrap --framework F [--entry E] [--ready-path P] [--cmd ...]`;
  `--ready-path` only when `ready-path` is set, so the adapters keep their
  own health paths: gradio `/config`, streamlit `/_stcore/health`)
  and the window options the plugin reads at startup. A source dir that is
  the project root (`source = "."`) leaves out `src-tauri`, `node_modules`,
  `ui`, `dist` and `target`.
- **Step 6** imports `pylair.wrap` (the launcher the supervisor starts, so
  `pylair` must be one of the app's dependencies) and the framework with the
  bundled interpreter after the prune: `gradio`, `streamlit`, or the module
  of a `python -m <module>` command. An over-pruned bundle fails here, not on a user's machine.
- **`--stage-only`** runs steps 1 to 6 and stops before `tauri build`: no
  installer, no signing. CI uses it to prove a project builds.
- **`pylair dev`** needs no codegen: it writes `.pylair/pylair-wrap.json`
  with the absolute source dir and runs `tauri dev`; a change to a `.py` file
  restarts the server (a new port, and the window follows).
- **`pylair doctor`** checks the config, the dependency manifest, that the
  framework imports in the project venv (and, after a build, in the bundled
  interpreter), the window shell files, `"create": false` on the main window,
  and that no capability grants a remote url (the wrapped page must get no IPC).

Dependencies must come from a manifest (`pyproject.toml`, ideally with
`uv.lock`; `requirements.txt` is the warned fallback). `pylair wrap` on a
bare `app.py` stops with that explanation and offers `uv init`; imports are
never guessed.

## Network access on first build

A first build needs GitHub access twice: we download
python-build-standalone (step 1, then cached in `.pylair/cache/`), and
Tauri's bundler downloads its installer toolchain — WiX and NSIS on
Windows, the AppImage runtime on Linux — on first use. Later builds
reuse the caches; an offline rebuild works once they've been warmed.

## Slow or flaky networks

The python-build-standalone download (the part `pylair build` does
itself) is built for a bad connection:

- **Retry.** Up to 4 attempts, waiting about 1s, 3s and 8s between them
  (with jitter). It retries a stall (no data for the timeout below), a
  socket error such as `ECONNRESET`, a 5xx or 429 response, and a body
  that ends short of its `Content-Length`. It does not retry a 404 or
  other 4xx, a redirect to a non-https URL, or a redirect loop, since
  those fail the same way every time. Each retry prints one line, for
  example `retrying (2 of 4) in 3s: <reason>`.
- **Resume.** The partial file is kept as `<asset>.part` in
  `.pylair/cache/` and the next attempt (or the next run) continues it
  with a `Range` request, including after GitHub's redirect to its CDN.
  If the server ignores `Range`, or the partial file cannot be
  continued, the download restarts from zero. Resuming never skips
  verification: the finished file is still checked against the SHA-256
  committed in the CLI, and a mismatch is handled as before (one
  re-download, then the build refuses).
- **Progress.** A slow download prints bytes received over total, with a
  percentage when the size is known, to stderr at most every 10 seconds.
  A fast download prints nothing extra.
- **Timeout.** The no-data timeout is 30 seconds. Set
  `PYLAIR_DOWNLOAD_TIMEOUT_MS` (milliseconds, clamped to 5000..600000) to
  change it. A value that is not a whole number is an error.

### Downloading by hand

If every attempt fails, the error names the URL, how many bytes arrived
of how many, the exact path to use, the expected size and the SHA-256.
Fetch the file on a better connection (a browser or `curl -L -O` works),
and put it at the path shown, which is
`<your app>/.pylair/cache/<asset name>`. Keep the original file name. The next `pylair build` finds it in the cache,
verifies it against the committed SHA-256, and re-downloads it only if
the hash does not match.

## Configuration — one file

Everything pylair-specific lives in `[tool.pylair]` in `pyproject.toml`.
App identity (`productName`, `identifier`, `version`) stays in
`src-tauri/tauri.conf.json` — Tauri owns it, we derive from it. No third
config file.

```toml
[tool.pylair]
app-module = "app.backend"        # required: module holding @expose functions
source = "backend"                # dir staged into resources/ (default "backend")
requirements = "requirements.txt" # locked deps (default "requirements.txt")
python-version = "3.12"           # bundled CPython class: "3.12" | "3.13"
prune = ["my/extra/dir"]          # additions to the default deny-list
bundles = "dmg,app"               # optional override for tauri --bundles
include = [".env.example"]        # stage a path even though it matches
                                  # the secret-looking exclusions
```

## What ships from your source folder

The build copies `source` (default `backend/`) into the installer
verbatim — minus anything that looks like a secret. Skipped by default:

- any path segment starting with `.` — `.env`, `.env.*`, `.git`,
  `.venv`, `.DS_Store`, `.hidden/…`;
- basenames matching `*.pem`, `*.key`, `*.p12`, `*.pfx`, `id_rsa*`,
  `id_ed25519*`, `credentials*.json`, `service-account*.json`.

Skips are a warning, not an error: the build prints how many paths were
refused and the first ten names. To ship one deliberately, list it in
`include = [...]` — globs relative to `source`, same shape as `prune`
entries; an included path is staged even inside a refused directory.

Do not put secrets in the backend folder. The exclusion list is a
seatbelt for accidents, not a secret store — a committed `.env` is
already in your git history even if the installer drops it. Ship
configuration through the user's data dir at runtime
(`app.path().app_data_dir()` / `path_provider`), written on first run
or entered in a settings window.

## Pruning

The standalone CPython distribution ships things an installed app never
needs; site-packages does too. `pylair build` deletes them before bundling.
Default deny-list (globs relative to `src-tauri/resources/`):

| Pattern | Why it's dead weight |
|---|---|
| `python/include`, `*.a`, `lib/pkgconfig` | C headers / static libs — build-time only |
| `python/lib/python*/test`, `idlelib`, `tkinter`, `turtledemo` | stdlib test suite, IDLE, tk |
| `pydoc_data`, `ensurepip`, `venv`, `share` | docs topics, pip bootstrap, man pages |
| `site-packages/pip*`, `_distutils_hack` | pip is dead weight post-install |
| `site-packages/*/tests`, `*/test` | per-package test suites (numpy's is ~25 MB) |
| `**/*.pyc`, `**/__pycache__` | staging leftovers — the bundle ships its own pycs (next section) |

Deliberately **not** pruned:
- `*/testing` — `numpy.testing`, `pandas.testing`, `torch.testing` are
  imported at runtime by downstream code.
- `pkg_resources` / `setuptools` — still imported at runtime by many libs.
- `LICENSE.txt` and other license files — the PSF license *requires* them
  to ship with redistributed interpreters.

After pruning, the build imports the app module with the staged tree
(`PYTHONNOUSERSITE=1`) and fails if it breaks — over-pruning shows up at
build time, not on the user's machine.

Apps extend it via `prune = [...]`. If an app genuinely ships a package
named `tests` or needs pip at runtime (e.g. a plugin installer), that entry
belongs in a future `prune-exclude` — for now, deny-list is deny-list.

## Precompiled bytecode

After pruning — so denied dirs never get compiled — the build runs
`python -m compileall` on `site-packages/` and the staged app source
using the **bundled** interpreter, guaranteeing the pyc magic number
matches the runtime that will load them. This matters because the
install dir is not necessarily writable (per-machine Program Files
installs, signed `.app` bundles, mounted AppImages): without shipped
pycs the app would compile its whole Python tree in memory on *every*
launch, paying the cold-start cost each time. Packaged spawns also set
`PYTHONDONTWRITEBYTECODE=1` (see `docs/security.md` §8), so nothing ever
writes `__pycache__` into the install dir.

Invalidation mode is `checked-hash`, chosen deliberately:

- **timestamp (compileall default) — rejected.** Installers rewrite
  mtimes (NSIS/WiX normalize them); a timestamp pyc whose recorded
  mtime no longer matches the source file is silently ignored, which
  recreates the every-launch recompile this step exists to remove.
- **unchecked-hash** is the fastest — the loader trusts the pyc and
  never stats or hashes the source. Correct for immutable bundles, but
  a post-install source edit would silently run stale bytecode.
- **checked-hash** reads and hashes the source once per import — the
  measured cost on a numpy-sized tree is within noise of unchecked
  (~0–10ms), and an edited source recompiles correctly instead of
  running stale code. The safety costs nothing measurable.

Compilation is deterministic: `compileall` runs with cwd =
`src-tauri/resources/` and relative dir arguments, so `co_filename` is
bundle-relative (`site-packages/numpy/...`). The build path is never
embedded, identical inputs produce identical pycs on any machine, and
tracebacks in installed apps show the paths the user actually has.

Files that fail to compile are a **warning, not a build failure** — a
dependency may ship a py2-only file, template, or invalid fixture that
nothing imports; those files are left uncompiled and will compile (or
fail to import) as normal at runtime, while the interpreter failing to
run at all still fails the build.

## App data at runtime

The spawn spec sets `PYLAIR_APP_DATA_DIR` to the app's per-user data dir
(`app.path().app_data_dir()` — the bundle identifier's folder):
`%APPDATA%/<id>` on Windows, `~/Library/Application Support/<id>` on
macOS, `~/.local/share/<id>` on Linux. The plugin creates it if missing;
if it cannot be resolved the failure is logged once and the variable is
left unset — never a startup failure.

The dir is deliberately outside the install folder: it survives upgrades
and reinstalls, needs no admin rights, and is where large runtime-fetched
artifacts belong — downloaded models under `<dir>/models/` (decision
0009), caches, user files. Uninstall removes the app, not the data dir.

`pylair dev` hands down the same dir the installed app will use — the dev
app has the same bundle identifier — and in debug builds an inherited
`PYLAIR_APP_DATA_DIR` overrides it, like every other dev override. A
packaged app always uses the resolved dir: the launching environment
cannot relocate its data, the same reason it cannot redirect the
interpreter.

Isolated and warm workers are spawned by the runtime and inherit its
environment, so they see the same variable with no extra plumbing.

## No cross-compiling

Pylair bundles *native* wheels. A Linux host cannot produce a Windows
`.pyd`, and Tauri itself does not cross-compile. `pylair build` therefore:

- always fetches the interpreter for the host triple and runs `tauri build`
  without `--target`;
- errors early if the host `platform:arch` has no standalone CPython build
  (`wasm32`, 32-bit, etc.);
- should be invoked once per target OS — in CI, that's a matrix job.

Cross-OS installers are out of scope by design, matching Tauri's own model.

## Signing

**Unsigned by default.** `tauri build` produces an unsigned bundle and
pylair prints a one-line warning: unsigned apps trigger Gatekeeper on macOS
and SmartScreen on Windows, which is correct and intended: users should
see that warning until real signing is configured.

On macOS, `pylair build` still re-signs the bundle **ad-hoc** when no
identity is configured — every Mach-O binary inside the `.app` (the
vendored CPython tree included), verified with
`codesign --verify --deep --strict` before the build returns. Tauri's
own output leaves only a linker stub signature on the main binary,
which is the worse state for Gatekeeper. Ad-hoc signing does **not**
remove the warning — a downloaded `.app` still needs
right-click → **Open**, or `xattr -dr com.apple.quarantine <app>` — it
makes the signature self-consistent so the verdict is a plain warning,
not an "is damaged" refusal.

Signing credentials are read from environment variables, never config
files (secrets don't belong in VCS):

| Env var | Platform | Purpose |
|---|---|---|
| `APPLE_SIGNING_IDENTITY` | macOS | Developer ID Application cert |
| `APPLE_CERTIFICATE` + `APPLE_CERTIFICATE_PASSWORD` | macOS | base64 p12 for CI |
| `APPLE_ID`/`APPLE_PASSWORD`/`APPLE_TEAM_ID` or `APPLE_API_KEY`/`APPLE_API_ISSUER`/`APPLE_API_KEY_PATH` | macOS | notarization |
| `TAURI_SIGNING_PRIVATE_KEY` + `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | all | **updater** signatures only — does NOT sign the app and does not silence the unsigned warning |
| `WINDOWS_CERTIFICATE_THUMBPRINT` | Windows | cert in the machine store |
| `AZURE_TENANT_ID`/`AZURE_CLIENT_ID`/`AZURE_CLIENT_SECRET` + `AZURE_CODE_SIGNING_*` | Windows | Azure Trusted Signing (CI-friendly, no dongle) |

Open item: macOS **notarization of an embedded Python** — every `.so` and
`.dylib` inside `resources/` must be individually signed under the hardened
runtime before the outer bundle is notarized. This is where projects like
this usually get stuck, so it is tracked as its own milestone (M5/M6
risk item), not folded into the first build.
