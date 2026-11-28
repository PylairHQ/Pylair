# External binaries (`pylair.binary`, `pylair.proc`)

A local-AI app often needs real executables beside its Python runtime:
ffmpeg for media, llama-server for inference, a compiler, a shell tool.
Pylair ships them as declared, hash-verified build artifacts, resolves
them per platform, and runs them under the same ownership rules as
everything else a task holds. The full design is
[decision 0014](decisions/0014-external-binaries.md); this guide is the
using-it half.

## Declaring binaries

Binaries are declared in the app's `pyproject.toml`, per target triple,
with a license and cryptographic pinning. There are two shapes: a
single-file binary (`member` or neither key) and a directory unit
(`executable`), an executable shipped with its companion shared
libraries.

```toml
[tool.pylair.binaries.ffmpeg]
license = "LGPL-2.1-or-later"
license_file = "licenses/ffmpeg.txt"

[tool.pylair.binaries.ffmpeg.x86_64-pc-windows-msvc]
url = "https://example.com/ffmpeg-7.1-windows.zip"
sha256 = "9f2c…"   # 64 lowercase hex chars, of the archive or file
member = "bin/ffmpeg.exe"   # optional: extract one file from the archive

[tool.pylair.binaries.ffmpeg.x86_64-unknown-linux-gnu]
path = "vendor/ffmpeg-linux"   # or a vendored file in the repo
sha256 = "a41f…"
```

Per target you give either `path` (a vendored file, still hash-verified -
the hash is the reviewable record of what ships) or `url` + `sha256`,
plus optional `member` to extract one file out of a zip or tar.gz.
`pylair build` verifies the hash before anything is staged, downloads
once per sha256 (rebuilds never refetch), merges the binary into Tauri's
`bundle.externalBin`, and writes the resolution manifest the plugin
reads. `pylair dev` resolves the same entries into `.pylair/binaries/`
and hands the directory to the plugin, so dev and release resolve
through one mechanism. There is no PATH fallback anywhere: a machine
that happens to have ffmpeg installed is irrelevant, by design.

## Directory units

A unit ships the whole executable-plus-libraries tree. `executable`
marks the shape and is mutually exclusive with `member`: `member`
extracts one file, `executable` stages the whole tree and names its
entry point. This is the llama-server shape (CPU build shown):

```toml
[tool.pylair.binaries.llama-server]
license = "MIT"
license_file = "licenses/llama.cpp.txt"

[tool.pylair.binaries.llama-server.x86_64-unknown-linux-gnu]
url = "https://github.com/ggml-org/llama.cpp/releases/download/b7040/llama-b7040-bin-linux-x64.zip"
sha256 = "…"                  # the archive's sha256, as upstream publishes it
executable = "llama-server"   # entry point inside the extracted tree
```

The archive may be zip or tar.gz. Extraction preserves the tree's
layout, in-unit symlinks and executable bits, and rejects absolute
paths, `..` components and links escaping the unit before anything is
written. Units install as `binaries/<name>/` inside the resources
payload; `pylair.binary("llama-server")` still resolves directly to
the executable. In dev the same tree is staged at
`.pylair/binaries/<name>/`.

A vendored directory can be the source instead of an archive. Because
a directory has no single upstream hash, it needs a checked-in manifest
mapping every relative file path to its sha256:

```toml
[tool.pylair.binaries.llama-server.x86_64-unknown-linux-gnu]
path = "vendor/llama-linux"
manifest = "vendor/llama-linux.manifest.json"
executable = "llama-server"
```

Every staged file is verified against the manifest. Missing files,
extra files and hash mismatches are build errors naming the paths.

Loader rewriting is part of staging, and only ever happens when
building for the OS on that OS (there is no cross-OS staging):

- **macOS:** every Mach-O file, detected by magic bytes rather than
  extension, has non-system install names rewritten to
  `@rpath/<basename>` and rpaths added relative to `@loader_path` for
  each unit directory containing Mach-O files. Rewriting invalidates
  upstream signatures, so Pylair signs the staged code again: with the
  app identity and hardened runtime when signing is configured, ad-hoc
  otherwise. An unsigned nested dylib can fail to load even in an
  unsigned development build.
- **Linux:** every ELF file gets `$ORIGIN`-relative rpaths with
  `patchelf`. `patchelf` is a required Linux prerequisite; if it is
  missing the build fails with an actionable error. `LD_LIBRARY_PATH`
  launcher wrappers are deliberately not used.
- **Windows:** nothing is rewritten. The Windows loader searches the
  executable's own directory first, which is exactly the unit layout.
  When Windows signing is configured, every `.exe` and `.dll` in the
  unit is Authenticode-signed.

Watch the size. A unit is inside the installer, inside every updater
download, and again in the rollback cache. `pylair doctor` reports the
unit's file count and total size and warns above 200 MB: past that
point, decision 0014 section 8's bundle-versus-lazy rule says to think
hard about lazy download instead. GPU units are the extreme case. CUDA,
CPU and Metal builds are declared as separate names
(`llama-server-cuda`, `llama-server`) and app code chooses at runtime;
there is no first-class variant key in v1. A CUDA unit can add hundreds
of megabytes for every user, including machines without NVIDIA GPUs.
NVIDIA runtime libraries are redistributable only as the CUDA EULA's
listed redistributable files, so the unit's `license` must cover them
and the EULA text must travel as its `license_file`.

## `pylair.binary(name) -> Path`

The host-resolved absolute path of a declared binary:

```python
import pylair

ffmpeg = pylair.binary("ffmpeg")  # Path, already staged and verified
```

An unknown name raises `KeyError` listing the available names. A missing
or malformed resolution map raises `RuntimeError` saying the binary was
not resolved by the host - which is what running outside a Pylair spawn
(or an app that declares no binaries) looks like. Warm and isolated
workers inherit the runtime's environment, so resolution works there
unchanged, exactly like `models.path`.

## `pylair.proc`: running them

You can pass the `Path` to any process API, but `pylair.proc.spawn`
exists because one integration point is impossible to retrofit from
outside: **cancellation**.

```python
import pylair

proc = await pylair.proc.spawn("ffmpeg", "-i", "in.mp4", "out.webm")
# or pylair.proc.spawn(path, ...) with any executable path
async for line in proc.stdout:    # text lines, utf-8, errors="replace"
    ...
code = await proc.wait()          # return code; proc.kill(), proc.pid
```

- `exe` is a declared binary name (resolved through `pylair.binary`) or
  anything path-shaped (a `Path`, or a string with a separator).
- `stdout` defaults to `PIPE`; `stderr` defaults to **inherit**, so
  sidecar stderr lands in the dev inspector timeline interleaved with
  the calls that produced it. `stderr=pylair.proc.PIPE` captures instead
  (ffmpeg's progress, say). Both are async line iterators when piped;
  `.reader` on each is the raw stream, and `proc.process` is the asyncio
  `Process` for anything else.
- A pipe is a finite OS buffer (about 64 KB), not a sink. `stdout=PIPE`
  is right for batch tools whose output you consume, but if the child
  outlives the call that started it and nobody keeps reading, the buffer
  fills, the child blocks on write, and it hangs silently. For a
  long-running server either pass `stdout=pylair.proc.DEVNULL` or keep a
  reader task draining the iterator for as long as the server runs. The
  runtime logs one warning when a task finishes while a child it spawned
  still runs with a stdout PIPE nothing has ever read.
- `stdin` defaults to **devnull**, not inherit: the runtime's own stdin
  is the host protocol channel, so an inheriting child would eat frames.
  Pass `pylair.proc.PIPE` to feed input through `proc.process.stdin`.

### The two ownership rules

1. **Cancel kills the tree.** When the owning task is cancelled through
   the normal cancel path, every child it spawned is tree-killed -
   grandchildren included - *before* `TASK_CANCELLED` is reported. A
   cancelled ffmpeg does not burn CPU for minutes after its caller is
   gone. POSIX uses a process-group SIGTERM, a short grace, then
   SIGKILL; Windows uses `taskkill /T /F` with the same fallback chain
   as the worker kill path. One Windows limit is known: `taskkill /T`
   walks *live* parent links at the moment it runs, so a grandchild
   whose own parent already exited is no longer linked into the tree
   and survives the cancel. POSIX `killpg` does not have the gap - the
   grandchild stays in the victim's process group however its parent
   died. Closing the Windows gap needs a Job Object with
   `KILL_ON_JOB_CLOSE`; it is tracked in [ideas.md](ideas.md).
2. **Normal completion does not kill.** A child of a task that finishes
   (or even fails) keeps running: servers outlive the call that starts
   them. When the app exits, the supervisor's containment reaps the
   runtime's whole process tree either way, so nothing escapes the
   process lifetime.

A spawn outside any task belongs to the runtime itself and is covered
by rule 2's containment only.

### Long-running servers

llama-server and friends are not batch tools: they bind a port and need
readiness. The pattern is decision 0007 unchanged - the readiness
surface does not care whether the component is a model download or a
sidecar server:

```python
import pylair
from pylair import expose, loading, ready

ServerState = {"proc": None, "port": 8741}


@expose
async def start_llama() -> str:
    loading("llama-server", 0.1)
    # stdout=DEVNULL: the server outlives this call, and an unread PIPE
    # would fill up and block it on write. If you want the logs, keep a
    # reader task draining proc.stdout for the server's lifetime instead.
    proc = await pylair.proc.spawn(
        "llama-server",
        "--port",
        str(ServerState["port"]),
        stdout=pylair.proc.DEVNULL,
    )
    ServerState["proc"] = proc
    # probe the health endpoint until it answers, then:
    ready("llama-server")
    return "up"


@expose(requires=("llama-server",))
async def chat(prompt: str) -> str:
    ...  # never runs before the server is ready
```

There is no automatic restart in v1: if the server dies the component
stays ready and calls fail honestly; a supervisor task in app code that
respawns and re-readies is the documented pattern. See decision 0014
section 4 for the reasoning and section 9 for what is deliberately out
of scope (Xet-protocol models, automatic sidecar supervision).

## Licensing

Every declared binary carries its license into the app's
`THIRD-PARTY-NOTICES` tree at build time, GPL-family identifiers print a
loud build warning, and `THIRD-PARTY-NOTICES/index.json` is the
machine-readable seed for an SBOM. A missing license file fails the
build: shipping a binary you cannot account for is not a gray area.
