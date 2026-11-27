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
with a mandatory sha256 and license. **Single-file binaries only for
now** - directory units (an executable plus sibling shared libraries)
land in a later release; declaring one fails the build saying so.

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
