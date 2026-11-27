"""Model download manager (decision 0009).

Local-AI apps fetch multi-GB models on first run. `models.model()`
declares a model at module import — the component is managed and the
download auto-starts after hello beside `@on_startup` hooks;
`models.fetch()` is the idempotent manual form for `@on_startup` hooks;
`models.path(name)` resolves a verified file or raises `ModelNotReady`.

Storage lives under ``PYLAIR_APP_DATA_DIR`` (the host hands it down at
spawn, PR 1/3): ``<dir>/models/<name>`` is the verified file,
``<name>.part`` an in-flight download, ``<name>.json`` the verified
sidecar and ``<name>.lock`` the cross-process lock. stdlib only —
urllib streaming, sha256 as bytes land, atomic ``os.replace`` on a
hash match.
"""

from __future__ import annotations

import hashlib
import http.client
import json
import os
import random
import re
import shutil
import ssl
import sys
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
from collections.abc import Iterable
from dataclasses import dataclass
from pathlib import Path

from pylair import _readiness

_NAME_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$")
_SUFFIXES = (".part", ".lock", ".json")
_CHUNK = 1 << 16  # 64 KiB stream/read units
_REHASH = 1 << 22  # 4 MiB re-hash units
_MAX_REDIRECTS = 5
_IDLE_TIMEOUT_S = 30.0
_BACKOFF_S = (1.0, 3.0, 8.0)
_POLL_S = 0.2
_ALWAYS_SENSITIVE = frozenset({"authorization", "cookie"})
_ENV_DIR = "PYLAIR_APP_DATA_DIR"
_ENV_INSECURE = "PYLAIR_MODELS_ALLOW_INSECURE"
_ENV_WORKER = "PYLAIR_WORKER_PARENT"

_registry: dict[str, _Spec] = {}
_registry_lock = threading.Lock()
_name_locks: dict[str, threading.Lock] = {}
_started: set[str] = set()
_abort = threading.Event()
_autostart_hooked = False


def _log(msg: str) -> None:
    print(f"[pylair] {msg}", file=sys.stderr, flush=True)


class ModelError(RuntimeError):
    """A model could not be fetched or verified."""


class ModelNotReady(ModelError):
    """`path()` was asked for a model that is not verified-ready."""


class ModelCancelled(ModelError):
    """The fetch's abort event fired between chunks."""


@dataclass(frozen=True)
class _Spec:
    name: str
    url: str
    sha256: str
    size: int | None
    headers: dict[str, str] | None
    sensitive: frozenset[str]


class _Cancelled(Exception):
    pass


class _Fresh(Exception):
    """Wipe and restart from byte 0 (416 / bogus Content-Range)."""


# ---- validation ------------------------------------------------------------


def _check_name(name: object) -> str:
    """The model name is a path component: strict allowlist, no dot
    segments, no reserved suffix collisions — before any fs op."""
    if (
        not isinstance(name, str)
        or not _NAME_RE.match(name)
        or ".." in name
        or name.endswith(_SUFFIXES)
    ):
        raise ValueError(f"invalid model name {name!r}")
    return name


def _check_url(url: object) -> str:
    if not isinstance(url, str) or not url:
        raise ValueError(f"model url must be a non-empty string, got {url!r}")
    _check_scheme(url)
    return url


def _check_scheme(url: str) -> None:
    scheme = urllib.parse.urlsplit(url).scheme.lower()
    if scheme == "https":
        return
    if scheme == "http" and _insecure_ok():
        return
    raise ModelError(
        f"model URL must be https (got {scheme!r}) — set "
        f"{_ENV_INSECURE}=1 to allow http"
    )


def _insecure_ok() -> bool:
    return os.environ.get(_ENV_INSECURE) == "1"


def _in_worker() -> bool:
    return os.environ.get(_ENV_WORKER) is not None


def _check_sha(sha256: object) -> str:
    if (
        not isinstance(sha256, str)
        or len(sha256) != 64
        or any(c not in "0123456789abcdefABCDEF" for c in sha256)
    ):
        raise ValueError(f"sha256 must be 64 hex chars, got {sha256!r}")
    return sha256.lower()


def _make_spec(
    name: str,
    url: str,
    sha256: str,
    size: int | None,
    headers: dict[str, str] | None,
    sensitive_headers: Iterable[str] | None,
) -> _Spec:
    name = _check_name(name)
    url = _check_url(url)
    sha256 = _check_sha(sha256)
    if size is not None and (
        not isinstance(size, int) or isinstance(size, bool) or size < 0
    ):
        raise ValueError(f"size must be a non-negative int, got {size!r}")
    if headers is not None:
        headers = {str(k): str(v) for k, v in dict(headers).items()}
    sens = _ALWAYS_SENSITIVE | frozenset(
        str(h).lower() for h in (sensitive_headers or ())
    )
    return _Spec(name, url, sha256, size, headers, sens)


# ---- storage ---------------------------------------------------------------


def _data_dir() -> Path:
    """<app data dir>/models — the host hands the dir down at spawn;
    when it did not (headless self-test, older host), fail clearly
    rather than guess a location."""
    base = os.environ.get(_ENV_DIR)
    if not base:
        raise ModelNotReady(
            f"{_ENV_DIR} is not set — the host did not provide an app "
            "data dir, so there is no models location"
        )
    return Path(base) / "models"


def _sidecar(model_dir: Path, name: str) -> dict | None:
    try:
        return json.loads((model_dir / f"{name}.json").read_text())
    except (OSError, ValueError):
        return None


def _write_sidecar(model_dir: Path, spec: _Spec, final: Path, headers) -> None:
    rec = {
        "sha256": spec.sha256,
        "size": final.stat().st_size,
        "mtime_ns": final.stat().st_mtime_ns,
        "etag": (headers or {}).get("etag"),
        "last_modified": (headers or {}).get("last_modified"),
        "url": spec.url,
    }
    try:
        # tmp + os.replace — a concurrent path() must never read a
        # half-written record (the model file gets the same treatment).
        tmp = model_dir / f"{spec.name}.json.tmp"
        tmp.write_text(json.dumps(rec))
        os.replace(tmp, model_dir / f"{spec.name}.json")
    except OSError as e:
        # The file itself is verified this run — a missing sidecar only
        # costs one re-hash next launch. Log, never fatal.
        _log(f"models: could not write verified record for {spec.name!r}: {e}")


def _sidecar_match(model_dir: Path, spec: _Spec | None, name: str) -> Path | None:
    """The strict ready check: file plus a sidecar whose sha256 (the
    spec's when registered, else the record's own), size and mtime_ns
    all match the live stat — one stat, zero file reads. This is all
    path() does: a multi-GB re-hash belongs in the download path, never
    inside an arbitrary caller's (or warm worker's) call."""
    final = model_dir / name
    if not final.is_file():
        return None
    st = final.stat()
    rec = _sidecar(model_dir, name)
    want_sha = spec.sha256 if spec else (rec or {}).get("sha256")
    if (
        rec
        and want_sha
        and rec.get("sha256") == want_sha
        and rec.get("size") == st.st_size
        and rec.get("mtime_ns") == st.st_mtime_ns
    ):
        return final
    return None


def _verified(model_dir: Path, spec: _Spec | None, name: str) -> Path | None:
    """The download-path ready check: strict sidecar match, else one
    re-hash against the expected sha and a sidecar repair. Fetch and
    the autostart run this on every launch, so a stale mtime (copy,
    backup restore) self-heals there rather than inside path()."""
    p = _sidecar_match(model_dir, spec, name)
    if p is not None:
        return p
    final = model_dir / name
    if not final.is_file():
        return None
    rec = _sidecar(model_dir, name)
    want_sha = spec.sha256 if spec else (rec or {}).get("sha256")
    if not want_sha or _hash_file(final) != want_sha:
        return None
    if spec is not None:
        _write_sidecar(model_dir, spec, final, rec)
    return final


def _hash_file(p: Path) -> str:
    h = hashlib.sha256()
    with open(p, "rb") as f:
        while b := f.read(_REHASH):
            h.update(b)
    return h.hexdigest()


def _fmt_bytes(n: int) -> str:
    if n >= 1 << 30:
        return f"{n / 2**30:.1f} GB"
    if n >= 1 << 20:
        return f"{n / 2**20:.1f} MB"
    if n >= 1 << 10:
        return f"{n / 2**10:.1f} KB"
    return f"{n} B"


# ---- redirects -------------------------------------------------------------

_ORIGIN_DEFAULT_PORTS = {"https": 443, "http": 80}


def _origin(url: str) -> tuple:
    """scheme+host+port with default ports normalized — the boundary at
    which sensitive headers must be dropped."""
    s = urllib.parse.urlsplit(url)
    scheme = s.scheme.lower()
    return (
        scheme,
        (s.hostname or "").lower(),
        s.port or _ORIGIN_DEFAULT_PORTS.get(scheme),
    )


class _SensitiveRedirect(urllib.request.HTTPRedirectHandler):
    """Follows redirects but re-validates the scheme per hop and drops
    the sensitive set whenever the origin changes. urllib's default
    handler copies `add_header` headers onto the redirected request
    *including across hosts* — a gated-model 302 to a CDN would leak
    Authorization otherwise."""

    def __init__(self, sensitive: frozenset) -> None:
        self._sensitive = sensitive
        self.max_redirections = _MAX_REDIRECTS
        self.max_repeats = _MAX_REDIRECTS
        super().__init__()

    def redirect_request(self, req, fp, code, msg, headers, newurl):
        new = super().redirect_request(req, fp, code, msg, headers, newurl)
        if new is None:
            return None
        _check_scheme(new.full_url)
        if _origin(req.full_url) != _origin(new.full_url):
            for store in (new.headers, new.unredirected_hdrs):
                for k in [k for k in store if k.lower() in self._sensitive]:
                    del store[k]
        return new


def _opener(sensitive: frozenset) -> urllib.request.OpenerDirector:
    return urllib.request.build_opener(
        _SensitiveRedirect(sensitive),
        urllib.request.HTTPSHandler(context=ssl.create_default_context()),
    )


# ---- locking ---------------------------------------------------------------


def _name_lock(name: str) -> threading.Lock:
    with _registry_lock:
        return _name_locks.setdefault(name, threading.Lock())


def _try_file_lock(lockpath: Path):
    """Acquire the cross-process byte lock or return None. The lock is
    tied to the open handle — a killed or crashed holder releases it
    with no stale-lock cleanup."""
    fd = os.open(lockpath, os.O_CREAT | os.O_RDWR)
    try:
        if os.name == "nt":
            import msvcrt

            os.lseek(fd, 0, os.SEEK_SET)
            msvcrt.locking(fd, msvcrt.LK_NBLCK, 1)
        else:
            import fcntl

            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        return fd
    except OSError:
        os.close(fd)
        return None


def _release_file_lock(fd) -> None:
    try:
        if os.name == "nt":
            import msvcrt

            os.lseek(fd, 0, os.SEEK_SET)
            msvcrt.locking(fd, msvcrt.LK_UNLCK, 1)
        else:
            import fcntl

            fcntl.flock(fd, fcntl.LOCK_UN)
    finally:
        os.close(fd)


# ---- transfer --------------------------------------------------------------


def _read_meta(meta: Path) -> dict:
    try:
        return json.loads(meta.read_text())
    except (OSError, ValueError):
        return {}


def _write_meta(meta: Path, headers) -> None:
    rec = {
        "etag": headers.get("ETag"),
        "last_modified": headers.get("Last-Modified"),
    }
    try:
        meta.write_text(json.dumps(rec))
    except OSError:
        pass


def _wipe(*paths: Path) -> None:
    for p in paths:
        try:
            p.unlink(missing_ok=True)
        except OSError:
            pass


def _cr_start(content_range: str) -> int | None:
    """ "bytes START-END/TOTAL" -> START."""
    try:
        return int(content_range.split()[1].split("-")[0])
    except (IndexError, ValueError):
        return None


def _report_progress(name: str, done: int, total: int | None) -> None:
    if total:
        _readiness.loading(
            name, min(done / total, 1.0), f"{_fmt_bytes(done)} of {_fmt_bytes(total)}"
        )
    else:
        _readiness.loading(name, None, f"{_fmt_bytes(done)} downloaded")


def _transfer(
    spec: _Spec,
    part: Path,
    meta: Path,
    abort: threading.Event,
    report,
    new_hasher,
) -> str:
    """One transfer attempt: resume from .part if present (re-hash the
    prefix into the digest, Range + If-Range), stream 64 KiB chunks to
    disk hashing as they land, and return the whole-file hex digest.

    ``report(done, total)`` receives byte progress; ``new_hasher()``
    builds the digest object (sha256 for 0009 files, a pre-framed
    blob-sha1 for 0013 small files — framing is a constant prefix, so
    the resume re-hash stays correct either way)."""
    resume_from = 0
    if part.exists() and (sz := part.stat().st_size) > 0:
        resume_from = sz
    h = new_hasher()
    headers = dict(spec.headers or {})
    if resume_from:
        # hashlib state cannot persist — re-hash the existing prefix
        # once so the resumed digest covers the whole file.
        with open(part, "rb") as f:
            while b := f.read(_REHASH):
                h.update(b)
        headers["Range"] = f"bytes={resume_from}-"
        m = _read_meta(meta)
        validator = m.get("etag") or m.get("last_modified")
        if validator:
            headers["If-Range"] = validator
    req = urllib.request.Request(spec.url, headers=headers)
    try:
        resp = _opener(spec.sensitive).open(req, timeout=_IDLE_TIMEOUT_S)
    except urllib.error.HTTPError as e:
        if e.code == 416 and resume_from:
            _wipe(part, meta)
            raise _Fresh() from e
        raise
    with resp:
        status = getattr(resp, "status", 200)
        if resume_from and status == 206:
            # Verify the offset before appending — the wrong start
            # offset is silent corruption.
            if _cr_start(resp.headers.get("Content-Range", "")) != resume_from:
                _wipe(part, meta)
                raise _Fresh()
            mode, digest, done = "ab", h, resume_from
        elif status == 200:
            # Whole body — a fresh download, or the server ignored/
            # rejected Range and restarts us from byte 0.
            mode, digest, done = "wb", new_hasher(), 0
        else:
            raise ModelError(f"unexpected status {status}")
        _write_meta(meta, resp.headers)
        cl = resp.headers.get("Content-Length")
        total = spec.size or (
            (resume_from + int(cl)) if cl and status == 206 else int(cl) if cl else None
        )
        last = 0.0
        with open(part, mode) as f:
            while True:
                if abort.is_set() or _abort.is_set():
                    raise _Cancelled()
                try:
                    chunk = resp.read(_CHUNK)
                except (http.client.IncompleteRead, ConnectionError, TimeoutError) as e:
                    raise _Retryable(e)
                if not chunk:
                    break
                f.write(chunk)
                digest.update(chunk)
                done += len(chunk)
                now = time.monotonic()
                if now - last >= _POLL_S:
                    last = now
                    report(done, total)
        report(done, total)
        if cl is not None:
            expected = resume_from + int(cl) if status == 206 else int(cl)
            if done < expected:
                raise _Retryable(
                    f"connection dropped mid-body ({done} of {expected} bytes)"
                )
    return digest.hexdigest()


class _Retryable(Exception):
    pass


def _retryable(exc: BaseException) -> bool:
    if isinstance(exc, _Retryable):
        return True
    if isinstance(exc, urllib.error.HTTPError):
        return exc.code == 429 or 500 <= exc.code < 600
    return isinstance(
        exc,
        (
            urllib.error.URLError,
            http.client.HTTPException,
            TimeoutError,
            ConnectionError,
        ),
    )


def _err_text(exc: BaseException) -> str:
    """Failure detail for readiness — status codes and reasons, never
    request headers."""
    return f"{type(exc).__name__}: {exc}"


def _download_verified(
    spec: _Spec,
    part: Path,
    meta: Path,
    abort: threading.Event,
    *,
    report,
    note=None,
    new_hasher=hashlib.sha256,
    algo: str = "sha256",
) -> str:
    """The download+verify core shared by single-file models (0009) and
    snapshot files (0013): disk preflight, resume, retries with
    backoff, one hash-mismatch restart. Raises ModelError /
    ModelCancelled with NO readiness side effects — reporting is the
    caller's, so snapshot files never pollute the readiness surface.
    On success the verified bytes are in ``part`` (returned digest in
    hand); atomic placement is the caller's job.

    ``report(done, total)`` is byte progress, ``note(msg)`` textual
    states like retry backoff, ``new_hasher()`` the digest factory and
    ``algo`` its name for the mismatch detail."""
    if spec.size is not None:
        present = part.stat().st_size if part.exists() else 0
        need = max(spec.size - present, 0)
        try:
            free = shutil.disk_usage(part.parent).free
        except OSError as e:
            raise ModelError(f"could not check disk space: {e}")
        if free < need:
            raise ModelError(
                f"not enough disk space: need {_fmt_bytes(need)}, "
                f"have {_fmt_bytes(free)}"
            )
    restarts = 0
    attempt = 0
    while True:
        try:
            digest = _transfer(spec, part, meta, abort, report, new_hasher)
        except _Cancelled:
            raise ModelCancelled(f"download of {spec.name!r} cancelled")
        except _Fresh:
            continue
        except ModelError:
            raise
        except Exception as e:
            if _retryable(e) and attempt < len(_BACKOFF_S):
                delay = _BACKOFF_S[attempt] * (0.75 + random.random() * 0.5)
                if note is not None:
                    note(f"retry {attempt + 1} in {delay:.0f}s")
                attempt += 1
                if abort.wait(delay) or _abort.wait(0):
                    raise ModelCancelled(f"download of {spec.name!r} cancelled")
                continue
            raise ModelError(
                f"network error after {attempt + 1} attempt(s): {_err_text(e)}"
            ) from e
        if digest == spec.sha256:
            return digest
        # Hash mismatch: never trust .part silently — one clean restart,
        # then a precise failure carrying both digests.
        restarts += 1
        _wipe(part, meta)
        if restarts > 1:
            raise ModelError(
                f"{algo} mismatch: expected {spec.sha256}, got {digest}",
            )


def _download_and_verify(spec: _Spec, model_dir: Path, abort: threading.Event) -> Path:
    name = spec.name
    part = model_dir / f"{name}.part"
    meta = model_dir / f"{name}.part.json"
    try:
        _download_verified(
            spec,
            part,
            meta,
            abort,
            report=lambda done, total: _report_progress(name, done, total),
            note=lambda msg: _readiness.loading(name, None, msg),
        )
    except ModelCancelled:
        raise
    except ModelError as e:
        _readiness.failed(name, str(e))
        raise
    final = model_dir / name
    os.replace(part, final)
    _write_sidecar(model_dir, spec, final, _read_meta(meta))
    meta.unlink(missing_ok=True)
    return final


# ---- fetch / path / dir ----------------------------------------------------


def _fetch_sync(spec: _Spec, abort: threading.Event) -> Path:
    """The fetch lifecycle: verified-fast-path -> in-process gate ->
    cross-process lock with shared progress -> download+verify ->
    atomic rename + sidecar -> ready."""
    name = spec.name
    model_dir = _data_dir()
    model_dir.mkdir(parents=True, exist_ok=True)
    lockpath = model_dir / f"{name}.lock"
    name_lock = _name_lock(name)
    while True:
        p = _verified(model_dir, spec, name)
        if p is not None:
            _readiness.ready(name)
            return p
        if abort.is_set() or _abort.is_set():
            raise ModelCancelled(f"download of {name!r} cancelled")
        if name_lock.acquire(blocking=False):
            break
        _shared_progress(spec, model_dir)
        time.sleep(_POLL_S)
    try:
        while True:
            fd = _try_file_lock(lockpath)
            if fd is not None:
                break
            p = _verified(model_dir, spec, name)
            if p is not None:
                _readiness.ready(name)
                return p
            if abort.is_set() or _abort.is_set():
                raise ModelCancelled(f"download of {name!r} cancelled")
            _shared_progress(spec, model_dir)
            time.sleep(_POLL_S)
        try:
            p = _verified(model_dir, spec, name)
            if p is None:
                p = _download_and_verify(spec, model_dir, abort)
            _readiness.ready(name)
            return p
        finally:
            _release_file_lock(fd)
    finally:
        name_lock.release()


def _shared_progress(spec: _Spec, model_dir: Path) -> None:
    """A second caller waits on the holder and reports the real
    progress by stat-ing .part against the expected size."""
    part = model_dir / f"{spec.name}.part"
    done = part.stat().st_size if part.exists() else 0
    if spec.size:
        _readiness.loading(
            spec.name,
            min(done / spec.size, 1.0),
            f"{_fmt_bytes(done)} of {_fmt_bytes(spec.size)} (shared)",
        )
    else:
        _readiness.loading(spec.name, None, f"{_fmt_bytes(done)} downloaded (shared)")


def fetch(
    name: str,
    *,
    url: str,
    sha256: str,
    size: int | None = None,
    headers: dict[str, str] | None = None,
    sensitive_headers: Iterable[str] | None = None,
    abort: threading.Event | None = None,
) -> Path:
    """Idempotent manual fetch: a verified file returns immediately
    (one stat); otherwise lock -> download/resume -> verify -> `ready`.
    Belongs in an `@on_startup` hook or another ungated path — behind
    its own `requires` gate it can never run (the gate fails at the
    end-of-startup sweep)."""
    if _in_worker():
        raise ModelError(
            "models can only be fetched by the main runtime — in a "
            "worker use models.path() (read-only) with requires=(name,)"
        )
    spec = _make_spec(name, url, sha256, size, headers, sensitive_headers)
    _check_kind(spec.name, "single")
    _readiness.declare(spec.name, managed=True)
    try:
        return _fetch_sync(spec, abort or threading.Event())
    except ModelCancelled:
        # Not a failure — the .part stays for the next fetch/launch.
        _readiness.loading(spec.name, None, "cancelled — .part kept for resume")
        raise
    except ModelError as e:
        _readiness.failed(spec.name, str(e))
        raise


def path(name: str, *, revision: str | None = None) -> Path:
    """The verified model file (single-file models) or snapshot
    directory, else `ModelNotReady`. Resolve-and-check only — never
    downloads or touches the network (safe inside isolated and warm
    workers, which inherit the same data dir). For snapshots the §7
    reader protocol applies: a shared `.use` lock taken and held for
    the process lifetime, `.ready` rechecked under it, and the cheap
    per-file stat check of §3. `revision=` pins a specific revision —
    the escape hatch for riding out an upgrade in one process."""
    name = _check_name(name)
    model_dir = _data_dir()
    if _is_snapshot(name, model_dir):
        p = _snapshots().snapshot_path(model_dir / name, revision)
        _readiness.ready(name)
        return p
    if revision is not None:
        raise ValueError(
            f"{name!r} is a single-file model — revision pins apply to snapshots"
        )
    spec = _registry.get(name)
    p = _sidecar_match(model_dir, spec, name)
    if p is None:
        raise ModelNotReady(f"model {name!r} is not downloaded and verified")
    _readiness.ready(name)
    return p


def dir(name: str) -> Path:
    """The `models/` dir the named model lives in (or will, once
    fetched) — mainly for logging and `du`."""
    _check_name(name)
    return _data_dir()


# ---- declarative form --------------------------------------------------------


def model(
    name: str,
    *,
    url: str,
    sha256: str,
    size: int | None = None,
    headers: dict[str, str] | None = None,
    sensitive_headers: Iterable[str] | None = None,
) -> None:
    """Declare a model at module level: registers the spec, declares the
    readiness component `managed` at import, and auto-starts the
    download after hello beside `@on_startup` hooks — never blocking
    dispatch. The answer to the first-call race (0009 §5): `requires=`
    alone is unmanaged, so a model must be declared here or fetched
    during startup, never lazily behind its own gate."""
    spec = _make_spec(name, url, sha256, size, headers, sensitive_headers)
    _check_kind(spec.name, "single")
    _readiness.declare(spec.name, managed=True)
    with _registry_lock:
        _registry[spec.name] = spec
    if _in_worker():
        # Workers only resolve files — the main runtime owns downloads.
        return
    _register_autostart()
    if _readiness.startup_begun():
        # Registered during a hook or after startup: the autostart
        # hook's generation already ran — start directly. Before
        # startup (import time, codegen scans) the hook waits for a
        # serving runtime, so nothing downloads on a mere import.
        _start_download(spec)


# ---- snapshots (decision 0013) ----------------------------------------------
#
# The storage mechanics live in pylair._snapshots; this layer owns the
# public surface: readiness wiring, the declarative form, worker rules,
# auth env adoption and the single-file/snapshot kind split.

_snapshot_registry: dict[str, dict] = {}


def _snapshots():
    # Deferred: _snapshots imports models for the transfer machinery,
    # so a top-level import here would be circular.
    from pylair import _snapshots as mod

    return mod


def _check_kind(name: str, want: str) -> None:
    """A name is one kind or the other, never both: a single-file model
    is the FILE <data>/models/<name>, a snapshot the DIRECTORY
    <data>/models/<name>/. Mixing them is a clear error at the call
    site, not undefined behaviour."""
    other = _snapshot_registry if want == "single" else _registry
    kind = "single-file model" if want == "snapshot" else "snapshot"
    if name in other:
        raise ValueError(
            f"{name!r} is already registered as a {kind} — a name is "
            f"one kind or the other, never both"
        )
    try:
        shape = _data_dir() / name
    except ModelNotReady:
        return  # no data dir yet — the registration check still fired
    if want == "snapshot" and shape.is_file():
        raise ValueError(
            f"{name!r} exists on disk as a single-file model — a name "
            f"is one kind or the other, never both"
        )
    if want == "single" and shape.is_dir():
        raise ValueError(
            f"{name!r} exists on disk as a snapshot — a name is one "
            f"kind or the other, never both"
        )


def _is_snapshot(name: str, model_dir: Path) -> bool:
    if name in _snapshot_registry:
        return True
    if name in _registry:
        return False
    return (model_dir / name).is_dir()


def fetch_snapshot(
    name: str,
    *,
    repo: str | None = None,
    revision: str | None = None,
    endpoint: str | None = None,
    base_url: str | None = None,
    manifest: list[dict] | None = None,
    allow: Iterable[str] | None = None,
    ignore: Iterable[str] | None = None,
    headers: dict[str, str] | None = None,
    sensitive_headers: Iterable[str] | None = None,
    abort: threading.Event | None = None,
) -> Path:
    """Idempotent snapshot fetch (decision 0013): download and verify
    one pinned revision of a directory-shaped model, switch ``current``
    to it and return the revision directory. Hugging Face sources pin a
    full commit sha; anything else pins an explicit
    ``{path, sha256, size}`` manifest.

    Readiness (§4): one component per snapshot name — exact-byte
    ``loading`` on first install, ``ready`` on a verified current,
    ``failed`` naming the file. Upgrades never flicker: while a
    verified current exists the component stays ``ready`` (progress
    surfaces in its detail), because ``path()`` keeps resolving the old
    revision until the pointer's atomic move.

    Auth: explicit ``headers`` win; otherwise ``HF_TOKEN`` is adopted
    as a Bearer token when set, and ``HF_ENDPOINT`` overrides the
    default endpoint. Tokens are stripped on cross-origin redirects
    and never logged. Workers never fetch — they resolve with
    ``path()``."""
    if _in_worker():
        raise ModelError(
            "models can only be fetched by the main runtime — in a "
            "worker use models.path() (read-only) with requires=(name,)"
        )
    name = _check_name(name)
    _check_kind(name, "snapshot")
    snap = _snapshots()
    if endpoint is None:
        endpoint = os.environ.get("HF_ENDPOINT") or snap.DEFAULT_ENDPOINT
    if headers is not None:
        headers = {str(k): str(v) for k, v in dict(headers).items()}
    if not any(k.lower() == "authorization" for k in (headers or {})):
        token = os.environ.get("HF_TOKEN")
        if token:
            headers = dict(headers or {})
            headers["Authorization"] = f"Bearer {token}"
    _readiness.declare(name, managed=True)
    model_root = _data_dir()
    name_dir = model_root / name

    # An upgrade keeps serving the old revision throughout: the
    # component never leaves ready while one verified current exists.
    cur = snap.read_current(name_dir)
    serving = bool(cur) and (name_dir / cur / ".ready").is_file()

    def progress(
        done: int, total: int, current_file, files_done: int, files_total: int
    ) -> None:
        detail = f"{_fmt_bytes(done)} of {_fmt_bytes(total)}"
        if files_total:
            detail += f" ({files_done} of {files_total} files)"
        fraction = min(done / total, 1.0) if total else None
        if serving:
            _readiness.report(name, "ready", fraction, f"upgrading: {detail}")
        else:
            _readiness.loading(name, fraction, detail)

    def _write_current_once(rd: Path) -> None:
        if snap.read_current(name_dir) != rd.name:
            snap.write_current(name_dir, rd.name)

    try:
        snap.gc_name(name_dir)
    except Exception as e:  # noqa: BLE001 - GC is bookkeeping, never fatal
        _log(f"models: GC before fetch failed: {e}")
    try:
        rev_dir = snap.fetch_snapshot(
            name,
            repo=repo,
            revision=revision,
            endpoint=endpoint,
            base_url=base_url,
            manifest=manifest,
            allow=allow,
            ignore=ignore,
            headers=headers,
            sensitive_headers=sensitive_headers,
            abort=abort,
            progress=progress,
            # current is written inside the cross-process fetch lock
            # hold: GC skips this name entirely while that lock is
            # contended, so the newborn revision is never deletable.
            # Fast-path repeats fire it too — the write is skipped when
            # the pointer already names the revision.
            on_verified=_write_current_once,
        )
        _readiness.ready(name)
        try:
            snap.gc_name(name_dir)
        except Exception as e:  # noqa: BLE001
            _log(f"models: GC after fetch failed: {e}")
        return rev_dir
    except ModelCancelled:
        if serving:
            _readiness.report(
                name,
                "ready",
                None,
                "upgrade cancelled — still serving the previous revision",
            )
        else:
            _readiness.loading(name, None, "cancelled — .partial kept for resume")
        raise
    except ModelError as e:
        if serving:
            _readiness.report(
                name,
                "ready",
                None,
                f"upgrade failed: {e} — still serving the previous revision",
            )
        else:
            _readiness.failed(name, str(e))
        raise


def model_snapshot(
    name: str,
    *,
    repo: str | None = None,
    revision: str | None = None,
    endpoint: str | None = None,
    base_url: str | None = None,
    manifest: list[dict] | None = None,
    allow: Iterable[str] | None = None,
    ignore: Iterable[str] | None = None,
    headers: dict[str, str] | None = None,
    sensitive_headers: Iterable[str] | None = None,
) -> None:
    """Declarative snapshot form, mirroring model(): registers the spec,
    declares the readiness component managed at import, and auto-starts
    the fetch after hello beside @on_startup hooks — never blocking
    dispatch, never downloading on a mere codegen-scan import."""
    name = _check_name(name)
    _check_kind(name, "snapshot")
    kwargs = {
        "repo": repo,
        "revision": revision,
        "endpoint": endpoint,
        "base_url": base_url,
        "manifest": manifest,
        "allow": allow,
        "ignore": ignore,
        "headers": headers,
        "sensitive_headers": sensitive_headers,
    }
    _readiness.declare(name, managed=True)
    with _registry_lock:
        _snapshot_registry[name] = kwargs
    if _in_worker():
        return
    _register_autostart()
    if _readiness.startup_begun():
        _start_snapshot_download(name)


def _start_snapshot_download(name: str) -> None:
    key = f"snapshot:{name}"
    with _registry_lock:
        if key in _started:
            return
        _started.add(key)
    t = threading.Thread(
        target=_run_snapshot_download,
        args=(name,),
        name=f"snapshot-fetch:{name}",
        daemon=True,
    )
    t.start()


def _run_snapshot_download(name: str) -> None:
    try:
        fetch_snapshot(name, **_snapshot_registry[name], abort=_abort)
    except ModelError as e:
        _readiness.failed(name, str(e))
    except Exception as e:  # noqa: BLE001 - the fetch thread must never die quietly
        _readiness.failed(name, f"{type(e).__name__}: {e}")


def _gc_sweep() -> None:
    """Runtime-startup GC in a daemon thread: never blocks startup or
    readiness, never crashes the runtime — bookkeeping only."""
    try:
        _snapshots().gc_all(_data_dir())
    except Exception as e:  # noqa: BLE001
        _log(f"models: startup GC failed: {e}")


def _register_autostart() -> None:
    global _autostart_hooked
    with _registry_lock:
        if _autostart_hooked:
            return
        _autostart_hooked = True
    _readiness.on_startup(_autostart)


def _autostart() -> None:
    """Post-hello start, beside user hooks: spawn one daemon thread per
    registered model and return at once — dispatch is never blocked.
    The startup GC sweep rides along in its own thread: it never
    blocks startup or readiness."""
    with _registry_lock:
        specs = list(_registry.values())
        snap_names = list(_snapshot_registry)
    threading.Thread(target=_gc_sweep, name="models-gc", daemon=True).start()
    for spec in specs:
        _start_download(spec)
    for name in snap_names:
        _start_snapshot_download(name)


def _start_download(spec: _Spec) -> None:
    with _registry_lock:
        if spec.name in _started:
            return
        _started.add(spec.name)
    t = threading.Thread(
        target=_run_download, args=(spec,), name=f"model-fetch:{spec.name}", daemon=True
    )
    t.start()


def _run_download(spec: _Spec) -> None:
    try:
        _fetch_sync(spec, _abort)
    except ModelError as e:
        _readiness.failed(spec.name, str(e))
    except Exception as e:  # noqa: BLE001 - the fetch thread must never die quietly
        _readiness.failed(spec.name, f"{type(e).__name__}: {e}")


def _reset() -> None:
    """Test isolation: clear registry/threads state."""
    global _autostart_hooked
    with _registry_lock:
        _registry.clear()
        _snapshot_registry.clear()
        _name_locks.clear()
        _started.clear()
        _autostart_hooked = False
    _abort.clear()


def _model_hint(name: str) -> str | None:
    """Sweep detail for an unmanaged name that is a registered model —
    it only reaches the sweep when the registration landed after it
    (a `models.model()` inside a hook or a lazily-imported module)."""
    if name in _registry:
        return (
            f"model {name!r} is registered via models.model() — declare "
            "it at module import so its managed declaration lands "
            "before the end-of-startup sweep"
        )
    return None


_readiness.register_detail_hint(_model_hint)
