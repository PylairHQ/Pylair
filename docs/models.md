# Models (`pylair.models`)

A download manager for the large files a local-AI app needs on first
run — model weights, tokenizers, embeddings — on machines with no
system Python and no place for a multi-GB blob inside the install
folder. `pylair.models` downloads over https into the app's data dir,
verifies the sha256 as bytes land, resumes interrupted downloads, and
reports progress through the app-readiness components so the front end
can gate calls on `requires=`.

```python
from pylair import expose, models

models.model(
    "llm-7b-q4",
    url="https://example.com/models/llm-7b-q4.gguf",
    sha256="9f2c…",
    size=4_400_000_000,
)


@expose(requires=("llm-7b-q4", "worker:chat"))
def load_model():
    p = models.path("llm-7b-q4")  # verified Path or ModelNotReady
    ...
```

## The declarative form: `models.model()`

`models.model(name, *, url, sha256, size=None, headers=None,
sensitive_headers=None)` at module level is the recommended form:

- It **registers** the spec and **declares the readiness component
  managed** at import — before the end-of-startup sweep. That is what
  makes `requires=(name,)` safe: an unmanaged component would be
  swept to `failed` when startup ends, so a `fetch()` lazily gated
  behind its own `requires` can never run (the "first-call race",
  decision 0009 §5).
- After hello, beside `@on_startup` hooks and **never blocking
  dispatch**, it starts the download on a daemon thread. The component
  goes `loading` with byte progress, then `ready` on verification —
  `health().app.components[name]` and `onApp` observe the real
  transitions.

Import alone never downloads: the autostart runs only inside a serving
runtime, so `pylair codegen` and headless scans stay offline.

## The manual form: `models.fetch()`

`models.fetch(name, *, url, sha256, size=None, headers=None,
sensitive_headers=None, abort=None)` is the same download under manual
control — idempotent: a verified file returns in one stat; anything
else locks, resumes/downloads, verifies and reports `ready`. It
belongs in an `@on_startup` hook or another ungated path. An optional
`threading.Event` aborts it — cancellation leaves `.part` in place for
the next resume, and the component re-reports `loading` (cancelled,
`.part` kept) rather than `failed`.

```python
@on_startup
def fetch_models() -> None:
    models.fetch("embedder", url=EMBED_URL, sha256=EMBED_SHA)
```

## Resolving: `models.path()` and `models.dir()`

`models.path(name)` returns the verified `Path` or raises
`ModelNotReady`. It is **strict-check only** — one stat: the file plus
a sidecar whose sha256, size and `mtime_ns` all match. It never
re-hashes, never locks, never touches the network. A sidecar mismatch
(a changed mtime after a copy or a backup restore) raises
`ModelNotReady`; the next `fetch()` — including the autostart, which
runs on every launch — performs one verify-and-repair pass.

`models.dir(name)` returns the `models/` dir the file lives in, for
logging or `du`.

Both are safe in **isolated and warm workers**: workers inherit
`PYLAIR_APP_DATA_DIR` through the runtime environment and `path()` is
resolve-only there — `models.fetch()` in a worker raises `ModelError`.
A `requires=(name,)` gate is still how a call waits for the download;
`path()` only ever answers for a verified file.

```python
@expose(worker=warm(), requires=("llm-7b-q4",))
def generate(prompt: str) -> str:
    return engine.load(models.path("llm-7b-q4")).complete(prompt)
```

(`worker=warm()` is the typed helper from `pylair` — the dict form
`{"mode": "warm"}` it builds keeps working unchanged.)

To gate on a model *and* a warm worker together:

```python
@expose(requires=("llm-7b-q4", "worker:chat"))
def chat(prompt: str) -> str: ...
```

## Where files live

The host hands the app's per-user data dir to the runtime as
`PYLAIR_APP_DATA_DIR` (`docs/build.md`); models go under
`<dir>/models/`:

| OS | Location |
|---|---|
| Windows | `%APPDATA%/<bundle id>/models/` |
| macOS | `~/Library/Application Support/<bundle id>/models/` |
| Linux | `~/.local/share/<bundle id>/models/` |

It survives upgrades and reinstalls and needs no admin rights — a
model is downloaded once per machine, not per install. If the variable
is unset (a host that predates it), the component fails with a clear
detail instead of guessing a path.

The layout is flat:

```
<app data>/models/<name>            verified file — the model itself
<app data>/models/<name>.part       in-flight or cancelled download
<app data>/models/<name>.part.json  resume metadata (validator + hash state)
<app data>/models/<name>.json       verified sidecar (sha256, size, mtime_ns, url)
<app data>/models/<name>.lock       cross-process lock
```

The final file appears only via atomic `os.replace` after the hash
matches — a partial or unverified file is never presented as ready.

## Resume, verification, concurrency

- **Resume.** A `.part` with usable validators (ETag / Last-Modified
  from `<name>.part.json`) is resumed with `Range` + `If-Range`; the
  partial prefix is re-hashed first. A `200` (range ignored), `416`,
  or a bogus `Content-Range` restarts cleanly — never silent
  corruption from a server that ignores or mishandles ranges.
- **Verification.** sha256 is computed as bytes land; a mismatch
  triggers one clean restart, then the component fails with expected
  vs actual. `size` (optional) pre-flights disk space and bounds the
  transfer.
- **Concurrency.** An in-process name lock plus a kernel file lock on
  `<name>.lock` (`fcntl`/`msvcrt`) single-flights the download across
  threads and processes; a second caller waits and reports shared
  progress from the `.part` size. A dead holder's lock is released by
  the OS.
- **Retries.** Network errors, timeouts, `429` and `5xx` retry on a
  1s/3s/8s ±25% backoff; redirects are capped at 5 with the scheme
  re-validated per hop.

## Auth headers and redirects

`headers=` passes request headers (e.g. a gated-model bearer token).
On a redirect that changes origin (scheme+host+port), `Authorization`,
`Cookie` and any names listed in `sensitive_headers=` are **dropped** —
urllib's default handler would forward them to the CDN host.
Same-origin hops keep them. Header names and values never appear in
readiness detail, logs or exceptions.

## https only — and the test escape hatch

URLs must be `https://`. `PYLAIR_MODELS_ALLOW_INSECURE=1` allows `http://`
— intended for loopback test fixtures and trusted LANs, and scoped to
whatever environment sets it. The scheme is re-checked on every
redirect hop.

On minimal Linux images (containers, `deb`-only installs) the bundled
CPython may not find the system CA store — point `SSL_CERT_FILE` at
one (the runtime already honours OpenSSL's variables):

```python
os.environ.setdefault("SSL_CERT_FILE", "/etc/ssl/certs/ca-certificates.crt")
```

## Errors

`ModelError` (with `ModelNotReady` and `ModelCancelled` subclasses)
carries the precise reason — expected vs actual digests on a hash
failure, the failing status/URL on a server error — never headers.
Readiness failure details are sanitized the same way.

## Snapshots (multi-file models)

Most real models are directories, not files: a config, tokenizer
files, sharded weights with an index. `models.fetch_snapshot()`
downloads and verifies a whole pinned snapshot into an immutable
revision directory:

```python
from pylair import models

models.model_snapshot(
    "qwen",
    repo="Qwen/Qwen3-0.6B",
    revision="1f2e9c8b7a6d5f4e3c2b1a0987654321fedcba98",
    allow=["*.json", "*.safetensors", "tokenizer*"],
)
```

Hugging Face metadata is the verifier: the tree API at a pinned commit
carries sha256 for LFS files and git blob sha1 for small ones, so
every file verifies without the author publishing checksums. The
`revision` must be a **full 40-hex commit sha** - with a branch the
tree query and the file downloads would be reads of a moving ref, and
verification would race the author instead of checking the content.
Getting the sha is one command, an authoring-time dev helper that
never ships into the runtime:

```
$ pylair models resolve Qwen/Qwen3-0.6B@main
1f2e9c8b7a6d5f4e3c2b1a0987654321fedcba98
revision="1f2e9c8b7a6d5f4e3c2b1a0987654321fedcba98"  # Qwen/Qwen3-0.6B@main, resolved 2026-11-27
```

`models.path("qwen")` returns the revision directory, and the returned
`Path` never changes what it points at: revisions are immutable, so a
tokenizer read lazily an hour later comes from the same snapshot as
the weights already in memory. `models.path("qwen", revision=...)`
pins a specific revision explicitly - the escape hatch for riding out
an upgrade inside one process.

Upgrades never flicker readiness: fetching a new revision stages
side-by-side and `current` switches atomically, so `path()` resolves a
complete snapshot at every instant and the component stays `ready`
(progress surfaces in its detail). The old revision stays on disk
until every process using it restarts - reader locks keep it alive -
so **peak disk usage during an upgrade is old + new**. A long-running
app that has moved on to the new revision still pins the old one until
restart; that is the price of never corrupting a live load.

Non-Hugging-Face sources pin an explicit manifest instead:

```python
models.fetch_snapshot(
    "m",
    base_url="https://models.example.com/pack/",
    manifest=[
        {"path": "config.json", "sha256": "...", "size": 612},
        {"path": "model.safetensors", "sha256": "...", "size": 988021102},
    ],
)
```

Auth mirrors the single-file rules: explicit `headers` win,
`HF_TOKEN` is adopted as a Bearer token when no Authorization header
is given (never logged, stripped on cross-origin redirects), and
`HF_ENDPOINT` selects a mirror. Gated or private repos work with a
token that has access.

Out of scope for snapshots: the Xet storage protocol (dedup/chunked
transfers), datasets, delta sync, interop with the huggingface_hub
cache layout, and loading snapshots into inference frameworks - that
last part is your code. See `docs/decisions/0013-model-snapshots.md`
for the full design record.

## Out of scope

- **No bundled inference runtime.** `pylair.models` fetches and
  verifies files; running them is your code (`docs/ideas.md` keeps a
  bundled runtime as an idea).
- **No model registry, quantization, or format handling** — the file
  is opaque bytes plus a hash.
- **No P2P or multi-source downloads** — one URL, one file.
- **No background downloader service** — downloads live inside your
  runtime's lifetime; a cancelled or killed app leaves `.part` for the
  next launch.

See `docs/decisions/0009-model-manager.md` for the full design record.
