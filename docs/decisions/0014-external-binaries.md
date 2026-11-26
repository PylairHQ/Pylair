# 0014: External binaries (sidecars)

Status: proposed

## Question

Real Pylair apps need native executables next to Python: ffmpeg for
media, llama-server or whisper.cpp for local inference, ollama for model
serving, or a custom Rust/Go tool the app compiles separately. The
bundled CPython runtime (0001) cannot produce these, pip does not ship
most of them, and asking users to install them defeats the one-command
installer. Tauri already has a mechanism for exactly this: sidecars,
declared in `bundle.externalBin` and resolved per target triple
(`<name>-<target-triple>`). This decision designs the Pylair pattern on
top of it: how binaries are declared, staged and verified at build time,
how Python code finds and runs them in dev and release, what the
permission, signing, licensing and update stories are, and when to
prefer a runtime download over bundling.

Nothing here is product code yet; the design leans on documented Tauri
behavior plus the established precedents in 0001 (resource staging),
0003 (process-tree containment), 0004 (host-side permissions), 0007
(readiness), 0009 (`PYLAIR_APP_DATA_DIR` handoff and lazy downloads) and
0011 (update artifacts). The first implementation PR carries the small
validation spike that proves externalBin staging end to end in the
template app.

## 1. Declaration and build-time staging

Binaries are declared in `pyproject.toml` under `[tool.pylair.binaries]`,
one table per binary, with per-target sources keyed by Rust target
triple:

```toml
[tool.pylair.binaries.ffmpeg]
license = "LGPL-2.1-or-later"
license_file = "licenses/ffmpeg.txt"    # copied into the notices tree (section 6)

[tool.pylair.binaries.ffmpeg.x86_64-pc-windows-msvc]
url = "https://example.com/ffmpeg-7.1-win-x64.zip"
sha256 = "…"
member = "bin/ffmpeg.exe"               # one file out of an archive (zip/tar.gz)

[tool.pylair.binaries.ffmpeg.aarch64-apple-darwin]
path = "vendor/ffmpeg-macos-arm64"      # or a local path checked into the repo
sha256 = "…"                            # verified for paths too: reviewable pinning
```

Rules:

- **Per-target source, keyed by triple.** Each target the build
  produces must have exactly one source: `path` (a file in the repo or
  produced by an earlier build step) or `url` + `sha256` (downloaded at
  build time into the build cache, then staged). `member` names the
  file to extract when the source is an archive; without it the source
  is the executable itself. A target entry with neither `path` nor
  `url`, or with both, is a config error.
- **Staging.** `pylair build` knows the target triples it is building
  (v1: the host triple; cross-compilation is out of scope, section 8).
  For each declared binary it stages
  `src-tauri/binaries/<name>-<triple>[.exe]` exactly as Tauri's sidecar
  naming requires (the `.exe` suffix on Windows targets, nothing
  elsewhere), verifies `sha256` of the *staged executable* (after
  extraction, not of the archive), and refuses the build with a precise
  message when a target is missing:

  ```
  error: binaries.ffmpeg has no source for x86_64-pc-windows-msvc
    add [tool.pylair.binaries.ffmpeg.x86_64-pc-windows-msvc] with path or url + sha256,
    or drop the binary from this target
  ```

  A build that stages nothing still fails the same way; a half-staged
  installer that runs on one OS and crashes on another is the failure
  mode this exists to prevent. Hashes are mandatory even for `path`
  sources: the hash in pyproject is the reviewable record of what
  ships, and a vendor file swapped by accident fails the build instead
  of shipping.
- **`tauri.conf.json` merging.** `bundle.externalBin` must list
  `binaries/<name>` for Tauri to pick the files up. `pylair build`
  already edits `tauri.conf.json` during build (the updater version
  sync), so it merges the externalBin entries itself, idempotently:
  entries for declared binaries are added if absent, entries the build
  added previously but which are no longer declared are removed, and
  entries a developer wrote by hand for non-pylair sidecars are left
  untouched (recognized by not colliding with a declared name). The
  diff is part of the build tree, reviewable in git like the version
  sync.
- **`pylair doctor`** validates the table without building: parses
  every entry, checks the host triple has a source, verifies `path`
  files exist and match their hash, and warns (not fails) when a url is
  plain http.

## 2. Python access: `pylair.binary(name) -> Path`

Python code should never compute platform paths itself. The resolution
story mirrors the `PYLAIR_APP_DATA_DIR` handoff (0009 section 1):

- **Release.** The plugin resolves each externalBin binary through
  Tauri's path resolution (sidecars land next to the main executable on
  Windows and Linux, inside `Contents/MacOS` in a macOS bundle) and
  passes the result to the runtime in the spawn environment as
  `PYLAIR_BINARIES`, a JSON object mapping name to absolute path:
  `{"ffmpeg": "/opt/myapp/ffmpeg-x86_64-unknown-linux-gnu"}`. Only
  binaries the plugin actually found are in the map.
- **Dev.** `pylair dev` keeps dev binaries in `.pylair/binaries/` under
  the app directory. Where a dev binary comes from, in order:
  1. the declared `path` for the host triple, if the entry uses one;
  2. otherwise a **dev download**: the declared `url` + `sha256` for
     the host triple, downloaded once by `pylair dev` into
     `.pylair/binaries/` and reused afterwards (hash-verified, so the
     cache is self-validating).

  The CLI passes the directory to the host as `PYLAIR_BINARIES_DIR`
  (inherited by the plugin process, same channel as
  `PYLAIR_RESTART_TRIGGER`); in debug builds the plugin resolves each
  name as `<dir>/<name>-<triple>[.exe]` and the JSON map it pushes is
  built from there.
- **The release-cannot-be-redirected rule, same as 0009.** In debug
  builds an inherited `PYLAIR_BINARIES` wins (tests and harnesses need
  to inject fixture binaries). In a release spawn the computed map is
  always set: a packaged app must not let the launching environment
  redirect which executables it runs, the same reason it cannot
  redirect the interpreter, the module, or its data dir. An inherited
  `PYLAIR_BINARIES` in a release launch is ignored (and logged once),
  never merged.
- **The API.** `pylair.binary("ffmpeg") -> Path` reads the map. An
  unknown name raises `KeyError` listing the available names; a name
  that is declared but missing from the map (binary failed to stage, or
  running outside a Pylair spawn) raises a clear `RuntimeError` saying
  the binary was not resolved by the host. There is no silent PATH
  fallback anywhere in the design (see Rejected). Worker processes
  inherit the runtime's environment, so warm/isolated workers resolve
  the same paths for free, exactly like `models.path` (0009 section 5).

## 3. Running them: a thin wrapper, not just a path

Position: v1 ships both the path API above **and** a minimal wrapper,
because three of the four integration points are impossible to retrofit
from outside:

```python
import pylair

proc = await pylair.proc.spawn("ffmpeg", "-i", "in.mp4", "out.webm")
# or pylair.proc.spawn(path, ...) with any executable
async for line in proc.stdout:          # text lines, or stderr=...
    ...
code = await proc.wait()                # return code; proc.kill(), proc.pid
```

- **Cancellation tie-in.** The wrapper registers each child in a
  per-task registry. When the owning task is cancelled (the existing
  `cancel` path), the runtime kills the child's process tree before
  reporting `TASK_CANCELLED`. A bare `asyncio.create_subprocess_exec`
  cannot do this: the child keeps running after the caller is gone,
  which for ffmpeg means minutes of silent CPU burn. This is the
  strongest reason to wrap, and it is the same ownership rule the
  bridge already applies to everything else a task holds.
- **Kill on runtime exit is already free.** The supervisor's
  containment reaps the runtime's whole process tree (0003 evidence:
  app quit reaps the server tree, zero survivors, asserted on all three
  OSes by the dogfood script). Children of the Python runtime are
  inside that tree, so no wrapper work is needed for the exit case;
  the wrapper exists for the *cancel* case.
- **stderr into the inspector.** `stderr` defaults to inherit, and
  because the dev-mode inspector already captures the runtime's own
  stderr stream into the timeline, sidecar stderr lands there with zero
  additional plumbing, generation-tagged and interleaved with the calls
  that produced it. `stderr=PIPE` captures instead for machine
  consumption (ffmpeg's progress). stdout defaults to `PIPE` since
  pipes are the common case for tool output.
- **Long-running servers.** llama-server and friends are not
  batch tools: they bind a port, need readiness, and sometimes die.
  v1's answers, in order:
  - **Ports**: the app picks, either a fixed port or port 0 with the
    chosen port parsed from the server's output or `/proc`-style
    discovery. No port allocation service in v1; loopback port
    collisions are the app's problem exactly as they are in wrap mode
    (0003), where the launcher already demonstrates the fixed-port
    pattern.
  - **Readiness**: through decision 0007, unchanged. The app declares a
    component (`pylair.loading("llama-server")`, probes the server's
    health endpoint from Python, then `pylair.ready("llama-server")`),
    and `requires=("llama-server",)` gates the calls that need it. The
    readiness surface deliberately does not care whether the component
    is a model download, a warm worker, or a sidecar server.
  - **Restart**: none in v1. If the server dies the component stays
    ready and calls fail honestly; the documented pattern is a
    supervisor task in app code that respawns and re-readies. Automatic
    sidecar supervision is explicitly out of scope (section 8): the
    existing supervisor's job is the Python runtime, and a second
    supervision domain with its own restart budgets is a design of its
    own, not a side effect of shipping ffmpeg.

## 4. Permissions: no new capability, and why that is honest

Position: **running a bundled sidecar from Python needs no capability,
and adding one would be security theater.**

Decision 0004's model scopes *which window may call which Python
function*; the decision is enforced host-side before anything reaches
Python. What an exposed function then does inside the runtime (write
files, open sockets, ctypes into system libraries) has never been
capability-gated, because the Python runtime is inside the app's trust
boundary: it is the app's own code, shipped by the app developer,
reviewed with the app. A subprocess spawn is not a privilege escalation
for a process that can already `ctypes.CDLL` anything; gate it and the
honest reviewer is *less* informed, because the capability file would
claim a boundary that does not exist.

Two consequences that keep the story clean:

- **The shell-plugin path stays closed.** Tauri's documented way to run
  sidecars from JavaScript (`Command.sidecar` via `shell:allow-execute`
  or `shell:allow-spawn` in the capability) is a *webview-facing*
  surface, and the webview is exactly what the trust boundary separates
  from the runtime. The templates do not grant it, `pylair doctor`
  warns if a capability grants `shell:allow-execute`/`allow-spawn` with
  `sidecar: true`, and the docs state the rule: sidecars are run from
  Python, never from the webview. One execution story, one place to
  audit.
- **What the capability file does say** stays true: per-function scopes
  decide which window can reach the functions that *use* the binaries.
  An app that ships ffmpeg behind `convert_video` scopes
  `convert_video` exactly as today; the binary itself is an
  implementation detail of trusted code, no different from a wheel with
  native extensions (which already executes arbitrary machine code).

## 5. Signing, notarization, and updates

- **macOS.** Nested executables are the sharp edge: Apple's hardened
  runtime and notarization require every executable inside the `.app`
  to be signed with the same identity, and an unsigned or adhoc-signed
  sidecar fails notarization even when the main binary is fine (the
  notary service scans nested code). `pylair build --sign` must sign
  each staged externalBin binary with the app's identity *before* the
  enclosing bundle is signed, innermost-first, with the hardened
  runtime enabled. The existing ad-hoc sign path (unsigned dev builds)
  signs each sidecar ad-hoc too, because an unsigned nested executable
  can fail to execute at all on newer macOS versions even without
  notarization in the loop. Entitlements: the sidecar inherits none
  automatically; apps whose sidecars need entitlements (fork/exec of
  further children, JIT) declare them in the signing config, documented
  with the warning that every entitlement on a sidecar is part of the
  notarization review.
- **Windows.** `pylair build --sign` signs each staged binary with
  Authenticode before the installer is built, then signs the installer
  as today. Strictly, nested unsigned executables do not trigger
  SmartScreen when spawned programmatically (no mark-of-the-web on
  installer-written files), but unsigned binaries are what EDR products
  and enterprise policies flag, and signing is cheap once the
  certificate is wired.
- **Linux / AppImage.** No signing story, same as the rest of the
  AppImage today; the binaries are inside the image and inherit
  whatever trust the image itself carries. Nothing to do.
- **Auto-update (0011).** Sidecars ride inside the bundle, so update
  artifacts carry them with zero new mechanics: full-bundle replacement
  is full-bundle replacement. The cost is *size*: every update
  re-downloads every bundled binary even when only Python code changed,
  and the rollback cache doubles that. A 70 MB ffmpeg adds 70 MB to
  every update download and 70 MB to the watchdog's cache. This cost is
  the main input to section 7's rule of thumb.

## 6. Licensing: recorded at build, fed into the notices tree

Position: yes, the build records licenses, because ffmpeg alone makes
this non-optional. An ffmpeg built with `--enable-gpl` (x264, x265) is
GPL-licensed code; shipping it inside an installer is distribution, and
the obligations land on the distributor. LGPL builds carry their own
notice requirements. The build cannot judge compliance, but it can make
the license impossible to not think about:

- `license` (an SPDX identifier) is **required** on every
  `[tool.pylair.binaries]` entry; the build refuses a binary without
  one. `license_file` names a text file copied into
  `THIRD-PARTY-NOTICES/binaries/<name>/` next to the existing pylair,
  interpreter and wheel notices (`notices.ts` already implements the
  collection shape; this adds a fourth source). For `url` sources
  without a `license_file`, the build requires `license_url` instead
  and downloads the text at build time, cached beside the binary.
- An index entry per binary joins the existing index:
  `binaries/ffmpeg/ - ffmpeg 7.1 (LGPL-2.1-or-later), <source host>`.
- **GPL-family warnings.** `GPL-2.0*`, `GPL-3.0*` and `AGPL-*`
  identifiers produce a loud build warning naming the obligation class
  ("distributing a GPL binary imposes source-offer obligations on you;
  LGPL requires the notice text you are shipping"). The build does not
  block: plenty of apps are GPL-compatible or have licenses, and the
  framework's job is visibility, not legal advice.
- The SBOM angle: the notices tree plus a machine-readable
  `THIRD-PARTY-NOTICES/index.json` (name, version if declared, SPDX id,
  source url, sha256) covers what an SBOM generation step needs later.
  The json file is the v1 deliverable; a CycloneDX/SPDX-document export
  is an ideas.md entry, not v1.

## 7. Bundling versus lazy download

`pylair.models` (0009) already downloads, verifies, resumes and
readiness-reports multi-GB files at runtime; a binary is just a file
plus an executable bit. The two paths fit different shapes:

| | bundle (this decision) | lazy download (models-style) |
|---|---|---|
| cost shape | installer size + **every update** re-downloads it | first-run download only, survives updates in the data dir |
| availability | present at first run, offline-safe | needs network on first run |
| per-target | each installer carries only its own target anyway | downloads only the host target |
| upstream churn | new binary means a new app release | app can pin/update independently |
| fits | small tools (a few MB), required at startup, offline-first apps, anything licensing-sensitive enough that you want your own exact build | large tools (ffmpeg-full is 70-100 MB), optional features, fast-moving upstreams |

Rule of thumb: bundle what every user needs at first launch and what is
small enough that update cost is noise; lazy-download what is large or
optional. llama-server for an inference app is borderline either way;
ffmpeg-full for a feature most users never open is clearly lazy;
a 2 MB custom Rust helper is clearly bundle.

v1 implements **bundling only**. Lazy-downloading an executable is
possible today in principle (`models.fetch` plus `chmod`), but the
executable bit on Windows is meaningless, the PATH resolution is not
the models dir's job, and an unverified-permission executable download
deserves its own surface rather than a documented hack. A future
`pylair.binaries.fetch` that reuses the models machinery with
chmod + resolution layered on is the natural v2; nothing in the v1
design closes it off (the resolution map is just a map; a downloaded
binary can join it).

## 8. Explicitly out of scope for v1

- **Cross-compiled staging.** `pylair build` stages for the triples it
  actually builds, which in v1 means the host. Building macOS-arm64 and
  macOS-x64 binaries from one mac requires per-target sources already
  declared, but assembling multi-arch installers from one host is a
  build-system feature of its own.
- **Sidecar supervision.** No restart budgets, no crash-loop detection,
  no health probing of sidecars by the host or runtime. App-level
  watchdog pattern documented instead (section 3).
- **Running sidecars from the webview** (shell plugin). Deliberately
  closed, section 4.
- **Binaries as services/daemons** (systemd units, launchd, Windows
  services). Sidecars die with the app; machine-wide service
  installation is an installer concern, not a runtime one.
- **Interactive stdin protocols** beyond writing bytes. `proc.stdin`
  accepts writes, but framed/typed RPC over a sidecar's stdio is what
  the Python runtime itself already is; apps needing that should wrap
  the tool in Python instead.
- **Delta/partial updates of bundled binaries** inside the 0011
  update flow.
- **A binary catalogue/registry** (named well-known tools with
  curated urls and hashes). Useful, small, and not v1.

## Rejected

- **Stage as plain resources instead of externalBin.** Resources are
  real files in the install tree, so executables there would technically
  run. Rejected because externalBin exists precisely for this: it gives
  the triple-naming convention, places binaries next to the executable
  where macOS nested-code signing and notarization expect them, and is
  what every Tauri reviewer recognizes. A resources-based layout would
  reinvent the resolution logic the plugin gets for free.
- **PATH fallback in dev.** Convenient (`brew install ffmpeg` and go)
  and wrong: dev would resolve a different binary, often a different
  major version, than the one the build pins and ships. The dev
  download plus declared-path story keeps dev and release on the same
  bits, which is the entire point of the pinned-hash design.
- **Host-side execution proxy** (a Tauri command that spawns the
  sidecar and pipes back to Python over the bridge). Rejected for the
  same reason 0009 rejected host-side downloads: subprocess management
  is natively complete in Python, the cancellation and readiness
  integration live in the runtime already, and a proxy would add
  protocol surface (spawn frames, exit frames, signal forwarding, pipe
  backpressure) to re-implement `asyncio.subprocess` badly.
- **Per-binary Tauri capabilities.** Covered in section 4: a capability
  on a boundary that does not exist is worse than none.
- **PyInstaller-style freezing of binaries into the Python payload** or
  appending them to the interpreter. Distribution formats for Python
  code do not help with non-Python executables, and embedding them as
  data-to-extract re-adds an extraction step, a hash-verification
  question and a temp-dir execution problem that externalBin simply
  does not have.
- **Auto-installing package-manager dependencies** (brew/apt/winget
  shims at install time). Breaks the no-system-dependencies premise of
  0001 and requires elevation; the whole point is that the installer is
  self-contained.

## Implementation plan

1. **CLI PR: declaration and staging.** `[tool.pylair.binaries]` parsing
   in config.ts, download+extract+hash staging into `src-tauri/binaries/`,
   the missing-target and hash-mismatch errors, the idempotent
   externalBin merge in `tauri.conf.json`, the dev-download cache in
   `.pylair/binaries/` + `PYLAIR_BINARIES_DIR`, license recording into
   THIRD-PARTY-NOTICES with the GPL-family warning and `index.json`,
   doctor validation, and the validation spike folded in as tests: the
   template app stages a small real binary (the repo's own test helper)
   and the acceptance suite runs it in the installed app. Carries the
   `notices.ts` wiring. `pylair build`/`doctor` surface, npm bumps.
2. **Crate PR: resolution and the env handoff.** `PYLAIR_BINARIES`
   resolution in the spawn spec (release: beside the executable via
   Tauri path resolution; debug: `PYLAIR_BINARIES_DIR` then inherited
   override), the release-cannot-be-redirected rule, spawn-env tests in
   the release_spec style. Crate bumps in the next train.
3. **pylair PR: `pylair.binary` + `pylair.proc`.** The path API, the
   spawn wrapper with the cancel-tied process registry, stderr-inherit
   into the inspector for free, unit tests with fixture binaries on all
   three OSes, worker inheritance tests, `docs/binaries.md`, README
   bullet. PyPI bumps.
4. **Docs/acceptance PR:** template gains an optional commented example
   entry, docs index update, selftest check that the installed app can
   run a staged fixture binary end to end (armed like the other
   selftest checks). npm bumps with the acceptance wiring.

Order note: 1 and 2 are independent in code but the acceptance test in
1 needs 2's resolution to mean anything in the installed app, so the
sensible merge order is 2 then 1 then 3, or one combined PR if the
batch is small. No protocol change anywhere: the handoff is
environmental, the wire is untouched.
