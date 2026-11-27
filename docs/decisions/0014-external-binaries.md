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
license_file = "licenses/ffmpeg.txt"    # copied into the notices tree (section 7)

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
  `url`, or with both, is a config error. This section covers
  single-file executables; binaries that ship with companion shared
  libraries are the units of section 2.
- **Staging.** `pylair build` knows the target triples it is building
  (v1: the host triple; cross-compilation is out of scope, section 9).
  For each declared binary it stages
  `src-tauri/binaries/<name>-<triple>[.exe]` exactly as Tauri's sidecar
  naming requires (the `.exe` suffix on Windows targets, nothing
  elsewhere), verifies `sha256` of the *staged executable* for
  bare-file sources and of the archive itself for archive sources
  (matching what upstream publishes; extraction of a verified archive
  is deterministic, so what is staged is what was verified), and
  refuses the build with a precise message when a target is missing:

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

## 2. Directory-shaped binaries: companion shared libraries

The single-file model above breaks on the headline use cases.
llama-server ships as an executable plus ggml/llama shared libraries
(DLLs or dylibs), plus CUDA runtime libraries in GPU builds;
whisper.cpp is the same shape; shared-build ffmpeg needs avcodec and
friends. externalBin stages exactly one file per name, so the design
must either refuse these tools or carry trees. Position: **carry
trees**. The alternative (static executables only, with the build
scanning for dynamic dependencies and failing) was seriously
considered and rejected: static ffmpeg exists, but static GPU builds
of the inference tools effectively do not (CUDA's runtime libraries
are distributed as shared libraries by NVIDIA itself), and even CPU
builds of llama.cpp and whisper.cpp ship as shared-library trees in
their upstream releases. A static-only rule would tell every app
developer to become a build engineer for upstream C++ projects, which
kills the feature for exactly the tools that motivate it.

v1 therefore supports two shapes per binary entry:

- **single-file**, as section 1: staged through externalBin.
- **unit**: a directory-shaped binary. The entry names an archive or a
  directory, plus `executable`, the entry point within it:

```toml
[tool.pylair.binaries.llama-server]
license = "MIT"
license_file = "licenses/llama.cpp.txt"

[tool.pylair.binaries.llama-server.x86_64-pc-windows-msvc]
url = "https://github.com/ggml-org/llama.cpp/releases/download/b7040/llama-b7040-bin-win-cuda-x64.zip"
sha256 = "..."                    # the archive's hash, as upstream publishes it
executable = "llama-server.exe"   # stage the whole tree; this is the entry point
```

  `executable` (versus `member`) is what declares the unit: `member`
  extracts one file, `executable` stages the whole tree preserving its
  internal layout and names the binary within it that
  `pylair.binary()` resolves. A directory `path` source works the same
  way when it points at a directory. The two keys are mutually
  exclusive.

**Extraction is a trust boundary of its own.** A verified archive
says the bytes are what upstream published; it says nothing about
whether the paths inside are safe to write. Extraction (unit trees and
single-file `member` alike) rejects any entry with an absolute path,
a `..` component, or a symlink or hard link whose target escapes the
unit directory, and fails the build naming the offending entry:

```
error: archive for binaries.llama-server contains unsafe entry "lib/../../evil.dll"
  archives may only carry relative paths inside the unit tree
```

  Upstream release archives are benign in practice, but the build runs
  wherever the developer runs it, often with write access to the whole
  project; a malicious or malformed archive must never write outside
  the staging tree. Symlinks *within* the unit are preserved (some
  dylib layouts need them); links pointing outside it are the failure.

**Where the unit lives per OS.** The unit always stays together in one
tree, so same-directory resolution does the work everywhere:

- **Windows**: the loader searches the exe's own directory first, so a
  tree with the DLLs beside the exe just works. The unit installs as a
  subtree `binaries/<name>/` in the install root, *not* flattened next
  to the main executable: two tools shipping different versions of
  `ggml.dll` would collide in a flat layout, and the install root
  stays reviewable.
- **macOS**: `Contents/Resources/binaries/<name>/` inside the bundle.
  This is exactly how the bundled interpreter already lives
  (`resources/python/` holds python plus libpython plus the openssl
  dylibs), and that tree already signs and notarizes in production
  builds, so the placement question is answered by precedent rather
  than theory. Dylibs in Frameworks is convention, not a notarization
  rule. The build rewrites install names rather than requiring
  upstream to build `@executable_path`-relative (they do not): for
  every Mach-O file in the unit, non-system install names become
  `@rpath/<basename>` and every such file gains rpath entries covering
  each directory in the unit that holds Mach-O files, expressed
  relative to `@loader_path` (for the exe, `@loader_path` is its own
  directory). Upstream trees are usually flat, where the rule reduces
  to one same-dir entry.
- **Linux / AppImage**: same tree, staged into the resources payload.
  The build runs `patchelf` to set `RUNPATH` to `$ORIGIN`-relative
  entries by the same mechanical rule (patchelf is already part of the
  Linux toolchain prerequisites). `LD_LIBRARY_PATH` set by a launcher
  wrapper is rejected: it leaks into grandchildren, breaks across
  re-exec (the updater restarts the app), and interposes on system
  library resolution in surprising ways.

**This is where resources staging is justified.** externalBin carries
one file per name per triple; a tree cannot be expressed in it. Units
therefore stage through the resources tree, the same mechanism that
already carries the interpreter, and single-file executables stay on
externalBin. The Rejected entry below is updated to match: resources
are wrong for single files (where externalBin is idiomatic and free),
right for trees (where externalBin is structurally impossible).

**Resolution.** The build writes `binaries/binaries.json` into the
staged tree recording every declared binary, its shape, and its
executable path relative to the payload root, for single-file and unit
entries alike. The plugin reads that manifest at spawn to build the
`PYLAIR_BINARIES` map, so resolution is one mechanism instead of two
and dev can emit the same manifest into `.pylair/binaries/`. Exact
placement mechanics belong to the implementation PRs; the decision is
that a build-generated manifest, not host-side guesswork, is the
source of truth.

**Hashing.** The declared `sha256` covers the archive as downloaded
when the source is a url, matching what upstream publishes, and is
verified before extraction; extraction of a verified archive is
deterministic, so what is staged is what was verified. A bare-file
source hashes the file. A directory source declares
`manifest = "path/to/manifest.json"`, a checked-in file mapping every
relative path in the tree to its sha256; the build verifies every
staged file against it, which is also what makes a vendored tree
reviewable in git. The shape of the source, not `member` versus
`executable`, decides what the hash covers: archive sources always
hash the archive (that is what upstream publishes), bare-file sources
hash the file.

**Signing the unit.** Every nested dylib and DLL is code in the
notarization sense: macOS scans them all, and an unsigned nested
dylib fails the ticket even when the executables are signed. The
innermost-first rule from section 6 extends over the tree: libraries
first, then the executables that load them, then the bundle. On
Windows the DLLs get Authenticode signatures alongside the exe, for
the same EDR reasons.

**GPU variants are out of scope as a variant system in v1.** CUDA,
CPU and Metal builds of the same tool are different binaries for the
same target triple, and a per-target `variant` key would need a
runtime selection story to mean anything. v1's answer: declare them as
separate binary names (`llama-server-cuda`, `llama-server`) and let
app code pick at runtime, accepting the installer size cost, or defer
to the lazy-download path, where GPU detection at runtime makes the
variant question natural. A first-class variant key belongs to the
v2 download surface, where it interacts correctly with per-machine
hardware.

**GPU units are the extreme case of the size trade-off.** A CUDA build
of llama-server pulls cuBLAS, cuBLASLt and the CUDA runtime libraries
into the unit: hundreds of megabytes inside the installer, inside
*every* 0011 update download, and again inside the rollback cache.
That is the section-8 bundle-versus-lazy rule pushed firmly toward
lazy: bundling a CUDA unit means every user, including those without
an NVIDIA GPU, pays the size cost on every update. There is also a
redistribution constraint unique to this case: NVIDIA's runtime
libraries are redistributable only as the CUDA EULA's listed
redistributable files, so a CUDA unit's `license` entry must cover
them (the EULA text travels as the unit's `license_file` like any
other), and an app shipping such a unit is making a EULA-compliant
redistribution, not just a technical choice. The notices step carries
it exactly like any other license file; the visibility argument of
section 7 applies double here.

## 3. Python access: `pylair.binary(name) -> Path`

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

## 4. Running them: a thin wrapper, not just a path

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
    sidecar supervision is explicitly out of scope (section 9): the
    existing supervisor's job is the Python runtime, and a second
    supervision domain with its own restart budgets is a design of its
    own, not a side effect of shipping ffmpeg.

## 5. Permissions: no new capability, and why that is honest

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

## 6. Signing, notarization, and updates

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
  the main input to section 8's rule of thumb.

## 7. Licensing: recorded at build, fed into the notices tree

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

## 8. Bundling versus lazy download

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
deserves its own surface rather than a documented hack. There is also
a signing constraint that matters for the v2 design: on Apple silicon,
arm64 code must carry a valid signature to execute at all (the kernel
refuses unsigned arm64 binaries; ad-hoc is sufficient), so
`pylair.binaries.fetch` will have to ad-hoc re-sign downloaded Mach-O
executables and dylibs after hash verification, because upstream
release binaries are frequently unsigned. A future
`pylair.binaries.fetch` that reuses the models machinery with chmod,
ad-hoc re-signing and resolution layered on is the natural v2; nothing
in the v1 design closes it off (the resolution map is just a map; a
downloaded binary can join it).

## 9. Explicitly out of scope for v1

- **Cross-compiled staging.** `pylair build` stages for the triples it
  actually builds, which in v1 means the host. Building macOS-arm64 and
  macOS-x64 binaries from one mac requires per-target sources already
  declared, but assembling multi-arch installers from one host is a
  build-system feature of its own.
- **Sidecar supervision.** No restart budgets, no crash-loop detection,
  no health probing of sidecars by the host or runtime. App-level
  watchdog pattern documented instead (section 4).
- **Running sidecars from the webview** (shell plugin). Deliberately
  closed, section 5.
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
- **Universal macOS binaries.** Tauri universal builds expect the
  triple name `universal-apple-darwin`, and the per-triple keying is
  agnostic: an entry may declare a source under that key and it stages
  like any other. What v1 does not do is *assemble* universal binaries
  with `lipo` from per-arch sources, and since the tools this feature
  exists for ship per-arch upstream, universal support is pass-through
  rather than a first-class path. Documented, not special-cased.

## Rejected

- **Stage as plain resources instead of externalBin.** Resources are
  real files in the install tree, so executables there would technically
  run. Rejected *for single-file executables* because externalBin exists
  precisely for them: it gives the triple-naming convention, places
  binaries next to the executable, and is what every Tauri reviewer
  recognizes. Directory-shaped units are the exception (section 2):
  externalBin is structurally one-file-per-name, so units stage through
  the resources tree exactly like the bundled interpreter already does,
  and the build-generated `binaries.json` manifest, not host-side
  guesswork, is how the plugin resolves either shape.
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
- **Per-binary Tauri capabilities.** Covered in section 5: a capability
  on a boundary that does not exist is worse than none.
- **PyInstaller-style freezing of binaries into the Python payload** or
  appending them to the interpreter. Distribution formats for Python
  code do not help with non-Python executables, and embedding them as
  data-to-extract re-adds an extraction step, a hash-verification
  question and a temp-dir execution problem that externalBin simply
  does not have.
- **Static executables only, with dependency scanning.** The build
  could parse Mach-O/PE/ELF dependencies (`otool -L`, the PE import
  table, `readelf -d`) and fail on any non-system library, pushing
  developers toward static builds. Rejected on section 2's terms:
  static GPU builds of the inference tools effectively do not exist
  and even CPU upstream releases are shared-library trees, so the rule
  would exclude the headline use case. The scanning idea survives as a
  diagnostic (a doctor warning is a fine ideas.md entry), not a gate.
- **Auto-installing package-manager dependencies** (brew/apt/winget
  shims at install time). Breaks the no-system-dependencies premise of
  0001 and requires elevation; the whole point is that the installer is
  self-contained.

## Implementation plan

1. **CLI PR: declaration and staging.** `[tool.pylair.binaries]` parsing
   in config.ts, download+extract+hash staging into `src-tauri/binaries/`
   for single files and into the resources tree for units (including
   the install-name/`patchelf` rewriting and the `binaries.json`
   manifest both shapes resolve through),
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
