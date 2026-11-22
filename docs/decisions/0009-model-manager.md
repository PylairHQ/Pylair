# 0009: Model download manager (`pylair.models`)

Status: proposed (spike: PR #TBD, throwaway proof in `spikes/m9-models/`,
CI run evidence in `.github/workflows/spike-tls.yml` on the spike
branch — the workflow file is not for merge)

## Question

A local-AI app needs multi-GB models on first run. The runtime must
fetch them itself — the host cannot bundle them into the installer and
there is no system Python to lean on. Propose `pylair.models` with a
minimal v1 API:

```python
from pylair import models

models.fetch(
    name,
    url="https://…/model.gguf",
    sha256="…",
    size=None,          # bytes, optional — enables the pre-flight
                        # disk-space check and exact progress
    headers=None,       # e.g. {"Authorization": "Bearer …"} — never logged
)
models.path(name)       # -> Path, only for a verified-ready model
```

Progress reports through app readiness (decision 0007): the component
`name` is `loading(name, progress, "x of y GB")` while downloading and
verifying, `ready(name)` on a verified file, `failed(name, detail)` with
a precise reason. `requires=(name,)` gates calls on it and
`health().app` shows it — and it composes with `worker:<fn>` from warm
workers (0008).

## 1. Storage layout and how the runtime learns the data dir

```
<app data dir>/models/
    <name>            # the verified model file
    <name>.part       # in-flight / resumable download
    <name>.json       # sidecar: {sha256, size, mtime_ns, etag, last_modified, url}
    <name>.lock       # cross-process lock (byte-range lock on this file)
```

`models.path(name)` returns `<data>/models/<name>` when the file exists
and its sidecar verifies cheaply (size + mtime unchanged ⇒ report
`ready` immediately, **no re-hash**; a changed mtime triggers one
re-hash before deciding, and a mismatch falls through to download).
Launch cost for an already-verified multi-GB model is one `stat`.

**The data dir.** The Python runtime does not know it today — only the
Rust side does (`app.path().app_data_dir()`, `docs/build.md:176`).
Proposal: the plugin passes it at spawn as `PYLAIR_APP_DATA_DIR` —
`SpawnSpec.env` already carries pairs to the child
(`supervisor/spawn.rs:67 cmd.envs(spec.env)`), and `app_spawn_spec`
(`spec.rs:135-157`) holds the `AppHandle`, so `app.path().app_data_dir()`
resolves in both dev and release. It is pushed into `spec.env` in both
branches of `app_spawn_spec`, and follows the existing override
precedent (spec.rs:140-142): in **debug builds only** an inherited
`PYLAIR_APP_DATA_DIR` wins (dev/test override); in a **release** spawn
the computed value is always set — a packaged app must not let the
launching environment relocate its data, same reason the interpreter
and module can't be redirected.

- **Installed app**: `app_data_dir()` per OS — `%APPDATA%/<bundle id>`,
  `~/Library/Application Support/<bundle id>`, `~/.local/share/<bundle
  id>`. Deliberately **outside the install folder** (`Program Files`,
  `.app`, `/usr/lib/<app>`): those are wiped/replaced by installers,
  often need admin rights, and are per-version. Models in the data dir
  survive upgrades.
- **`pylair dev`**: the dev app has the same Tauri identifier, so
  `app_data_dir()` returns the *same* directory the installed app will
  use on that machine — dev exercises the real path for free. A
  project-local alternative (`.pylair/models/` under the app dir) was
  considered and rejected: dev should mirror installed behavior, and
  two apps on one machine should not share a dev cache anyway.
  `PYLAIR_APP_DATA_DIR` stays overridable for tests/CI (an explicit env
  already set wins over the computed default — see precedence below).
- **Uninstall**: OS uninstallers remove the install dir; the data dir
  typically remains (Tauri has no universal uninstall hook). That is
  the desired property: models are user data, not program files — same
  reason a browser profile outlives a reinstall. `docs/models.md`
  (implementation PR) documents the path per OS so users can reclaim it.

Fallback when `PYLAIR_APP_DATA_DIR` is unset (spec-fn paths that never
got the env, unit tests): `models` errors loudly on `fetch`/`path`
unless the caller passes `dir=` — guessing a location silently is worse
than a clear error. (Option evaluated: default to
`platformdirs`-style per-OS dirs — rejected, the bundled runtime has no
such dependency and hand-rolled per-OS dirs re-introduce the problem
the env var solves.)

## 2. TLS from the bundled runtime — measured, all three OSes

This decided whether `pylair` stays dependency-free, so it was tested
on the exact shipped artifact: PBS `20261110` `3.12.16`
`install_only_stripped` per host triple, SHA-256 checked against the
committed `PBS_SHA256` values, positive probe (`pypi.org`) and negative
probe (`self-signed.badssl.com` must fail) — `spike-tls.yml` runs
`45063788355` (initial, linux leg fixed in `45063859707`).

| OS | Default verify paths | `urlopen https` | Verdict |
|---|---|---|---|
| windows-latest | `cafile=None` — **no CA file exists** | 200 OK; bad cert refused (`CERTIFICATE_VERIFY_FAILED`) | ✅ works — Windows system store, 410 CAs |
| macos-latest | `cafile=/private/etc/ssl/cert.pem` | 200 OK; bad cert refused | ✅ works — macOS system bundle, 126 CAs |
| ubuntu-22.04 | `capath=/etc/ssl/certs` (`cafile=None`) | 200 OK; bad cert refused | ✅ works — OpenSSL capath lazy-load |

Local repro of the packaged Windows interpreter
(`examples/reference-app/src-tauri/resources/python/python.exe`):
`urlopen("https://pypi.org")` 200, `enum_certificates("ROOT")` → 70
certs, `self-signed.badssl.com` refused. Same result.

**Mechanics.** On Windows, `load_default_certs` reads the ROOT system
store via `enum_certificates` (CPython). On macOS the PBS build's
OpenSSL defaults point at `/private/etc/ssl/cert.pem`, which macOS
maintains. On Linux `capath=/etc/ssl/certs` resolves the distro bundle
(`get_ca_certs()` reports 0 — capath loads lazily at verify time; the
probe proves the path actually works).

**Known gap (documented, not blocking):** a *minimal* Linux image with
no `ca-certificates` package fails — e.g. a bare container. The failure
is loud (`CERTIFICATE_VERIFY_FAILED`/`unable to get local issuer`), and
the documented remedy is `SSL_CERT_FILE=<pem>` at spawn (the host
already forwards env) or the app shipping certifi in requirements.
Windows' system store is a point-in-time view (the OS lazily fetches
missing roots on demand through CryptoAPI; OpenSSL's snapshot may lag a
brand-new root — Steve Dower's caveat, rare and self-healing).

**Decision: no new dependency.** `models` uses
`ssl.create_default_context()` and honors `SSL_CERT_FILE`/`SSL_CERT_DIR`
automatically. certifi (a real dep) and truststore (OS-store wrapper,
also a dep) are both unnecessary on all three supported OSes — proven,
not assumed. If a distro-less Linux deployment ever matters, the app
vendors a PEM via `SSL_CERT_FILE`; that is a deployment knob, not a
framework dependency.

## 3. Download semantics (mirrors `packages/cli/src/build/download.ts`)

The CLI's own PBS fetch already implements the semantics this needs;
`models` mirrors them in stdlib Python:

- `urllib.request` with a real `Request` (headers), `read(64 KiB)`
  chunks to `<name>.part`, hashing into `hashlib.sha256` as bytes land.
- Redirects: followed up to 5, cross-scheme or non-https targets
  refused (download.ts:171-182 pattern).
- Hash match on completion ⇒ `os.replace(.part, <name>)` (atomic on
  same filesystem — `.part` lives in the same dir) then sidecar write.
  Mismatch ⇒ delete `.part`, one clean restart; a second mismatch
  `failed(name, "sha256 mismatch: expected … got …")`.
- Retries: up to 4 attempts, exponential backoff `1s/3s/8s` ±25%
  jitter; retryable = socket errors, 5xx, 429, short body, idle stall
  (30 s no-data timeout, `PYLAIR_DOWNLOAD_TIMEOUT_MS`-style knob);
  non-retryable = 4xx (except 429/416 handling), redirect loop, refused
  scheme.
- Disk space: when `size` is known, `shutil.disk_usage` on the models
  dir before starting; `failed(name, "not enough disk space: need X,
  have Y")` without touching the network.

## 4. Resume and integrity — the edge cases

`hashlib` state cannot be persisted, so a resume **re-hashes `.part`
once at start** — measured 1.3 GiB/s on a mid-range Windows box
(`spikes/m9-models/rehash_bench.py`: 2 GiB in 1.56 s), so a 5 GiB model
costs ~4 s and a 20 GiB model ~15 s before the Range request goes out.
Negligible next to the download itself, and it doubles as a sanity
read of the partial file.

- **Range**: `Range: bytes=<len(.part)>-`. `206` ⇒ verify
  `Content-Range` start offset equals `.part` size before appending
  (append at the wrong offset = silent corruption). `200` ⇒ server
  ignored Range — reopen `.part` with `w` and take the whole body.
  `416` ⇒ `.part` is longer than the remote (or unsatisfiable) — wipe
  and restart.
- **Changed remote file**: sidecar stores `ETag`/`Last-Modified`;
  resume sends `If-Range` with the stored value — a changed resource
  answers `200` (new body) → restart, an unchanged one `206`. Servers
  without ETag fall back to `Last-Modified`; with neither, `If-Range`
  is omitted and the plain `Range` applies.
- **Partial-file corruption**: `.part` is never trusted silently. If
  the appended body yields a wrong final hash, the file is deleted and
  downloaded clean once; persistent mismatch ⇒ `failed` with expected
  vs actual hash (covers hostile content, never infinite-loops).
- **>4 GB files**: Python ints, `Content-Length`/`Content-Range`
  parsing and `os.path.getsize` are unbounded on 64-bit; NTFS/APFS/ext4
  all handle the size. Only a FAT32 volume (4 GiB limit) can fail —
  the write raises `OSError`, surfaced as a precise `failed` detail.
- **No partial-hash sidecar** in v1: prefix checkpoints would let a
  corrupted prefix restart at the corrupt point instead of the end;
  skipped as complexity — the final-hash guarantee is the contract,
  and the re-hash cost above is the only price paid per resume.

## 5. Concurrency, locking, lifecycle

- **Lock**: `<name>.lock` opened `os.O_CREAT|os.O_RDWR`, then a
  byte-range lock — `fcntl.flock(LOCK_EX|LOCK_NB)` on POSIX,
  `msvcrt.locking(LK_NBLCK)` on Windows. Both are kernel locks tied to
  the open handle: **a killed or crashed holder releases it** — proven
  on Windows (`spikes/m9-models/lock_proof.py`: a second process's
  `LK_NBLCK` fails with EACCES while held, acquires after release, and
  acquires immediately after the holder is terminated). No stale-lock
  cleanup is ever needed — unlike `O_EXCL` lockfiles, which leak on
  kill -9 and were rejected.
- **Second caller** (second window, dev restart, `fetch` called twice):
  spins on `LOCK_NB` with a poll (~200 ms), meanwhile reports
  `loading(name, <.part size>/<size>, "downloading (shared)")` — it
  *sees the real progress* by stat-ing `.part` against `size` (or the
  sidecar once it lands), then verifies the final file and returns
  `path()`. Two windows get one download and two accurate progress
  bars.
- **Download thread**: a daemon `threading.Thread` per fetch —
  `fetch()` is a sync function so `await asyncio.to_thread(...)` when
  called from async fns; it never touches the event loop except
  `_readiness.report` (which is thread-safe — the flusher thread
  already exists for `worker:<fn>`).
- **Cancellation**: a `threading.Event` checked between chunks —
  task cancel (the Python `cancel` path) sets it; the thread writes
  the sidecar only on success, so a cancelled/shutdown/dev-restart
  download just leaves `.part` for the next launch. On runtime exit
  the daemon thread dies with the process — same semantics.
- **Readiness lifecycle**: `declare(name, managed=True)` at fetch
  start so the end-of-startup sweep never fails a model still
  downloading after startup completes (0007/0008's managed flag —
  `worker:<fn>` precedent: a component whose lifecycle outlives
  startup). `requires=(name,)` waits through the sweep by design.
- **on_startup hooks**: an `@on_startup` hook can `fetch` directly —
  it runs beside dispatch, and progress flows through the same
  component. The example app does exactly this.

## 6. Security

- **Schemes**: `https` only in v1. `http` is rejected unless
  `PYLAIR_MODELS_ALLOW_INSECURE=1` is set explicitly (dev/LAN servers);
  `file:`/`ftp:`/`data:` refused outright — a model URL is remote by
  definition.
- **Secrets**: `headers` are forwarded verbatim to the request and
  **never** appear in logs, `failed` details, or readiness reports —
  error strings carry status codes and hostnames, not headers.
  (`url` is logged — it is the identity of the thing, and app code
  supplied it; tokens belong in `headers`.)
- **`name` traversal**: `^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$`, no `..`,
  no separators — `models/"../x"` is a `ValueError` before any fs op.
  The regex also forbids names that would collide with the reserved
  suffixes `.part`/`.lock`/`.json`.
- **Loop safety**: all network and hashing happen on the fetch
  thread; the only loop interaction is the thread-safe readiness
  report — a model download can never starve dispatch or pongs.
- **Redirect chain**: scheme re-validated per hop (no `https → http`
  downgrade); cap at 5 hops.

## 7. API surface, out of scope, example

**v1 surface** (in `pylair.models`):

| API | Behavior |
|---|---|
| `fetch(name, *, url, sha256, size=None, headers=None, timeout=None)` | idempotent: verified-ready returns immediately (one stat); else acquire lock → download/resume → verify → `ready`. Returns `Path`. |
| `path(name)` | `Path` if verified-ready, else `ModelNotReady` (a `RuntimeError` subclass). |
| `dir(name)` | the `models/` dir `Path` (mainly for logging/`du`). |

Everything else is **out of scope for v1**: model catalogues/registries,
mirror lists/fallbacks, delta/range-based updates, parallel multi-range
download, compression/decompression (`.gguf` etc. ship already packed),
GC/eviction policies (a `delete(name)` may come later but is trivial to
add), shared-machine/global caches (per-app dir only), bandwidth caps.
Each is a plausible `docs/ideas.md` entry — none gates the v1 design.

**Example story** — a chat app, first launch vs later launches:

```python
@on_startup
def load_llm():
    # first launch: downloads with progress in health().app;
    # later launches: one stat and straight to ready.
    models.fetch("llm-7b-q4",
                 url="https://…/model.gguf",
                 sha256="…",
                 size=4_100_000_000)

@expose(isolated=True,
        worker={"mode": "warm", "prewarm": True},
        requires=("llm-7b-q4", "worker:chat"))
def chat(prompt: str) -> str:
    ...
```

First launch: the `llm-7b-q4` component shows `loading` with `x of y
GB` progress (and `worker:chat` reports its own prewarm state); a
`chat()` call blocks inside `requires` until both are ready. Later
launches: sidecar match → `ready` in milliseconds, warm member loads
the model once, calls stay under the warm-path millisecond range from
0008 §8. Dev restart mid-download resumes `.part` where it left off.

## 8. Test plan

- **Unit**: a local `http.server` fixture with `Range` support
  (206/`Content-Range`, `If-Range`, `416`, redirect hops, drop-mid-body,
  wrong-hash server, no-`Content-Length`). Hash-mismatch detail, disk-
  space pre-flight (`shutil.disk_usage` patched), scheme allowlist,
  name validation, `.part` reuse across "restarts" (delete only the
  runtime, keep the dir), the second-caller-waits-and-shares-progress
  case, and lock contention (two `fetch` threads, one models dir).
- **Fault injection**: kill the fetch thread's process between chunks
  (POSIX flock release + Windows `msvcrt.locking` release — the
  mechanism proven in `lock_proof.py`), `If-Range` mismatch → 200
  restart, `416` → wipe, server that ignores `Range` entirely.
- **e2e**: `python -m pylair` + testapp fixture serving the same local
  server — `fetch` through the readiness path, `requires` gating,
  cancel mid-download → `.part` remains, next call resumes.
- **Self-test check** (`packages/client`): a `models` check in the
  installed-webview self-test on `--local` legs — the fixture fetches a
  *small* file from a loopback server started by the harness (real
  multi-GB downloads are covered by the unit/e2e layers).
- **CI**: the `spike-tls.yml` evidence above already proves bundled-
  interpreter HTTPS on all three OSes; the implementation PR keeps a
  smaller permanent TLS probe inside the Python suite (against the
  loopback fixture with a generated cert — no network dependence in CI).

## 9. PR split and release impact

1. **Crate PR** — `PYLAIR_APP_DATA_DIR` in `app_spawn_spec` (both dev
   and release branches), spawn-env plumbing tests, `docs/build.md` a
   line on the new env var. Crate bumps in the next train.
2. **pylair PR** — `pylair/models.py` + `models/__init__.py` export,
   `PYLAIR_MODELS_ALLOW_INSECURE`/`PYLAIR_APP_DATA_DIR` handling, the
   full unit suite, `docs/models.md`, README feature bullet, ideas.md
   entries for the deferred v2 items. PyPI bumps.
3. **Client/self-test PR** — the `models` selftest check + `--local`
   acceptance wiring (armed like `warm-workers`: skip fails `--local`
   legs, `PYLAIR_MODELS_EXPECTED` on pack/registry when the resolved
   versions carry it) + docs. npm bumps.

Release impact: `pylair` (the API lives there) and `tauri-plugin-pylair`
(the env var at spawn) — no wire change, no client API change, so the
npm trio only moves for the selftest check like rc.27 did.

## 10. Alternatives considered

- **Rust-side download**: the host downloads, Python only reads. Gains
  reqwest/rustls maturity; costs a new protocol surface (progress
  frames, cancel plumbing, hash verification all host-side) and puts
  the app-facing API in Tauri commands instead of the Python code that
  owns `requires=`. Rejected: readiness/reporting/cancellation all
  live natively in the Python side already; the download is a leaf
  feature, not a supervisor concern.
- **`requests`/`httpx` dependency**: unnecessary — the stdlib covers
  every requirement above (proven TLS), and the bundled runtime stays
  dependency-free as it is today.
- **Global shared cache** (`~/.cache/pylair/models`): dedupes across
  apps but breaks the `app_data_dir` story (uninstall, quotas,
  per-app trust). Per-app dir is the honest v1 default; a future
  `dir=` override already leaves the door open.
- **`O_EXCL` lockfile**: leaks on process death; rejected in favor of
  kernel byte-range locks (§5, proven).
