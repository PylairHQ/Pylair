"""Multi-file snapshot fetch core (decision 0013, PR 1) — internal.

Fetches one pinned revision of a directory-shaped model into
``<data>/models/<name>/<revision>/`` and returns that directory.
Nothing here is public API yet: ``models.fetch_snapshot``/``path()``
with the ``current`` pointer, reader locks and GC are PR 2.

Sources:

- Hugging Face: ``repo`` + ``revision`` (a full 40-hex commit sha,
  §1) — the tree API at that revision is the immutable manifest: LFS
  files verify by raw sha256 (``lfs.oid``), small files by git blob
  sha1 over the ``blob <size>\\0<payload>`` framing (``oid``).
- Anything else: ``base_url`` + an explicit ``manifest`` of
  ``{path, sha256, size}`` entries, stored under a synthetic revision
  key (sha256 of the canonical manifest, truncated to 40 hex) so the
  revision-addressed layout works uniformly.

Layout per fetch (§3): ``<name>/<revision>.partial/`` staging with
``.parts/<path>.part`` in-flight files, ``.manifest.json`` the plan,
``.verify.json`` the per-file verification record, ``.ready`` written
last, then one rename to ``<revision>/`` — a directory nobody has
open yet, so the rename is safe on Windows. Per-file transfer is the
0009 machinery (resume, retries, redirect header stripping), fanned
out 4 across files (§4).
"""

from __future__ import annotations

import concurrent.futures
import fnmatch
import hashlib
import json
import os
import re
import shutil
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
from collections.abc import Callable, Iterable
from dataclasses import dataclass
from pathlib import Path

from pylair import models
from pylair.models import ModelCancelled, ModelError

FANOUT = 4
DEFAULT_ENDPOINT = "https://huggingface.co"

_SHA_RE = re.compile(r"^[0-9a-f]{40}$")
_LFS_RE = re.compile(r"^[0-9a-fA-F]{64}$")
_BLOB_RE = re.compile(r"^[0-9a-fA-F]{40}$")
# Top-level names the layout itself owns — a repo file colliding with
# one would corrupt the bookkeeping, so it is rejected at plan time.
_RESERVED = {
    ".ready",
    ".manifest.json",
    ".verify.json",
    ".use",
    ".parts",
    ".lock",
    "current",
}
# Windows-hostile characters (plus control chars): a snapshot must be
# representable on every supported OS.
_BAD_PATH_CHARS = re.compile(r'[<>:"|?*\x00-\x1f\\]')
# Windows reserved device names: matching is case-insensitive and
# ignores any extension, so "aux", "AUX" and "aux.json" all alias the
# device. Paths arrive from a remote tree — an entry the filesystem
# silently aliases is a silent overwrite.
_DEVICES = frozenset(
    {"con", "prn", "aux", "nul"}
    | {f"com{i}" for i in range(1, 10)}
    | {f"lpt{i}" for i in range(1, 10)}
)

_name_locks: dict[str, threading.Lock] = {}
_registry_lock = threading.Lock()


def _log(msg: str) -> None:
    models._log(msg)


# ---- validation ------------------------------------------------------------


def _check_revision(revision: object) -> str:
    """§1: a full lowercase commit sha, never a branch or tag — the
    tree query and the file downloads must be one immutable system,
    and a moving ref makes verification a race instead of a check."""
    if not isinstance(revision, str) or not _SHA_RE.match(revision):
        raise ValueError(
            f"revision must be a full 40-character lowercase commit sha, "
            f"got {revision!r} — resolve a branch or tag at authoring "
            f"time (pylair models resolve org/model@main) and pin the sha"
        )
    return revision


def _check_repo_path(path: object) -> str:
    """A repo-relative posix path that must land inside the staging
    dir on every OS: no traversal, no absolute or backslash forms, no
    Windows-hostile chars, no collision with the layout's own files."""
    if not isinstance(path, str) or not path:
        raise ValueError(f"manifest path must be a non-empty string, got {path!r}")
    segments = path.split("/")
    if (
        path.startswith("/")
        or any(seg in ("", ".", "..") for seg in segments)
        or _BAD_PATH_CHARS.search(path)
    ):
        raise ValueError(f"unsafe manifest path {path!r}")
    for seg in segments:
        # Windows strips trailing dots and spaces, so "a." aliases
        # "a" — and a device name with any extension ("aux.json",
        # "nul/x") aliases the device. Both are silent-overwrite
        # collisions waiting for a case-insensitive filesystem.
        if seg.endswith((".", " ")):
            raise ValueError(
                f"unsafe manifest path {path!r}: segment {seg!r} ends in "
                f"a dot or space, which Windows strips"
            )
        if seg.split(".")[0].rstrip(" .").casefold() in _DEVICES:
            raise ValueError(
                f"unsafe manifest path {path!r}: segment {seg!r} is a "
                f"Windows reserved device name"
            )
    if path.split("/")[0] in _RESERVED or path in _RESERVED:
        raise ValueError(f"manifest path {path!r} collides with the snapshot layout")
    return path


def _check_plan_collisions(entries: list) -> None:
    """Two entries equal under casefold collide on Windows and macOS —
    one silently overwrites the other on disk. Reject the whole plan
    up front, naming the pair; the tree is remote input, so this is a
    security check, not a convenience."""
    seen: dict[str, str] = {}
    for e in entries:
        key = e.path.casefold()
        if key in seen:
            raise ValueError(
                f"entries {seen[key]!r} and {e.path!r} collide on "
                f"case-insensitive filesystems — the snapshot cannot be "
                f"represented safely"
            )
        seen[key] = e.path


def _check_globs(globs: object, what: str) -> tuple[str, ...]:
    if globs is None:
        return ()
    if isinstance(globs, str) or not isinstance(globs, Iterable):
        raise TypeError(f"{what} must be a list of glob strings, got {globs!r}")
    out = tuple(globs)
    for g in out:
        if not isinstance(g, str):
            raise TypeError(f"{what} entries must be strings, got {g!r}")
    return out


def _selected(path: str, allow: tuple[str, ...], ignore: tuple[str, ...]) -> bool:
    """fnmatch semantics on repo-relative posix paths, case-sensitive
    on every OS. Note fnmatch's ``*`` crosses ``/`` (it is not glob's
    segment-limited star) — documented in docs/models.md. ``allow``
    empty means everything; ``ignore`` applies after."""
    if allow and not any(fnmatch.fnmatchcase(path, g) for g in allow):
        return False
    return not any(fnmatch.fnmatchcase(path, g) for g in ignore)


# ---- the plan --------------------------------------------------------------


@dataclass(frozen=True)
class _Entry:
    path: str  # repo-relative posix
    size: int
    digest: str  # expected hex digest of the file content
    kind: str  # "sha256" (raw content / LFS) | "blob-sha1" (git framing)


def _git_blob_oid(data: bytes) -> str:
    """git's object id for a blob: sha1 over the framed payload. The
    empty-blob vector e69de29b… is the well-known proof."""
    return hashlib.sha1(b"blob %d\0" % len(data) + data).hexdigest()


def _blob_hasher(size: int) -> Callable:
    def make():
        return hashlib.sha1(b"blob %d\0" % size)

    return make


def _canonical_manifest(entries: list[_Entry]) -> str:
    rec = [
        {"path": e.path, "size": e.size, "digest": e.digest, "kind": e.kind}
        for e in sorted(entries, key=lambda x: x.path)
    ]
    return json.dumps(rec, sort_keys=True, separators=(",", ":"))


def _synthetic_revision(entries: list[_Entry]) -> str:
    """Non-HF sources have no commit sha — the manifest IS the pin, so
    its own hash keys the storage: a changed manifest is a changed
    snapshot, exactly the upgrade semantic of §7."""
    return hashlib.sha256(_canonical_manifest(entries).encode()).hexdigest()[:40]


def _check_manifest(manifest: object) -> list[_Entry]:
    """The explicit non-HF manifest: every entry carries sha256 + size
    — unverifiable bytes must never enter the snapshot."""
    if not isinstance(manifest, (list, tuple)) or not manifest:
        raise ValueError("manifest must be a non-empty list of {path, sha256, size}")
    entries = []
    for item in manifest:
        if not isinstance(item, dict):
            raise TypeError(f"manifest entries must be dicts, got {item!r}")
        path = _check_repo_path(item.get("path"))
        sha = item.get("sha256")
        if not isinstance(sha, str) or not _LFS_RE.match(sha):
            raise ValueError(
                f"manifest entry {path!r} must carry a sha256 of 64 hex "
                f"chars, got {sha!r} — unverifiable files are never fetched"
            )
        size = item.get("size")
        if not isinstance(size, int) or isinstance(size, bool) or size < 0:
            raise ValueError(f"manifest entry {path!r} has invalid size {size!r}")
        entries.append(_Entry(path, size, sha.lower(), "sha256"))
    return entries


# ---- tree resolution -------------------------------------------------------


def _next_link(header: str | None) -> str | None:
    """RFC 5988 Link header: the tree API paginates large repos with a
    cursor'd rel="next" URL. Minimal parse — comma-separated links,
    url in angle brackets."""
    if not header:
        return None
    for part in header.split(","):
        segs = part.split(";")
        if len(segs) < 2:
            continue
        url = segs[0].strip()
        for attr in segs[1:]:
            if attr.strip() == 'rel="next"':
                return url.strip("<>")
    return None


def _fetch_json(url: str, headers: dict, sensitive: frozenset) -> tuple:
    req = urllib.request.Request(url, headers=dict(headers))
    try:
        resp = models._opener(sensitive).open(req, timeout=models._IDLE_TIMEOUT_S)
    except urllib.error.HTTPError as e:
        raise ModelError(f"tree: {e.code} for {url}") from e
    except Exception as e:
        raise ModelError(f"tree: {models._err_text(e)}") from e
    with resp:
        try:
            return json.loads(resp.read()), resp.headers
        except ValueError as e:
            raise ModelError(f"tree: invalid JSON from {url}") from e


def _resolve_tree(
    repo: str,
    revision: str,
    endpoint: str,
    headers: dict,
    sensitive: frozenset,
) -> list[_Entry]:
    """The tree API at an immutable commit IS the manifest — walk
    every page before writing it. LFS entries verify by lfs.oid (raw
    sha256 of content); plain entries by git blob sha1 of the framed
    payload. A file entry with neither is a loud error: unverifiable
    bytes never enter the snapshot."""
    entries = []
    url = f"{endpoint}/api/models/{repo}/tree/{revision}?recursive=true"
    seen_paths = set()
    while url:
        data, hdrs = _fetch_json(url, headers, sensitive)
        if not isinstance(data, list):
            raise ModelError(f"tree: unexpected response shape for {repo}@{revision}")
        for item in data:
            if not isinstance(item, dict):
                continue
            if item.get("type") != "file":
                continue  # directories carry no content; anything else is not fetchable
            path = item.get("path")
            if not isinstance(path, str):
                continue
            path = _check_repo_path(path)
            size = item.get("size")
            if not isinstance(size, int) or size < 0:
                raise ModelError(f"tree: {path}: missing size")
            lfs = item.get("lfs")
            if isinstance(lfs, dict) and lfs.get("oid"):
                oid = lfs["oid"]
                if not isinstance(oid, str) or not _LFS_RE.match(oid):
                    raise ModelError(f"tree: {path}: lfs.oid is not a sha256")
                entries.append(_Entry(path, size, oid.lower(), "sha256"))
            else:
                oid = item.get("oid")
                if not isinstance(oid, str) or not _BLOB_RE.match(oid):
                    raise ModelError(
                        f"tree: {path}: no usable hash — neither an LFS "
                        f"sha256 nor a git blob oid; the file cannot be verified"
                    )
                entries.append(_Entry(path, size, oid.lower(), "blob-sha1"))
            if path in seen_paths:
                raise ModelError(f"tree: duplicate path {path!r}")
            seen_paths.add(path)
        url = _next_link(hdrs.get("Link"))
    if not entries:
        raise ModelError(f"tree: no files for {repo}@{revision}")
    return entries


# ---- progress --------------------------------------------------------------


class _Progress:
    """Exact byte accounting across the fan-out (§4): verified files
    count in full plus the current length of each .part. The callback
    shape is what the readiness wiring consumes:
    (bytes_done, bytes_total, current_file, files_done, files_total)."""

    def __init__(
        self, total: int, baseline: dict[str, int], callback, sizes: dict
    ) -> None:
        self.total = total
        self.done_by_file = dict(baseline)
        self.sizes = sizes
        self.current_file: str | None = None
        self.callback = callback
        self.lock = threading.Lock()

    def file_progress(self, path: str, done: int) -> None:
        with self.lock:
            self.done_by_file[path] = done
            self.current_file = path
            self._emit()

    def _emit(self) -> None:
        if self.callback is None:
            return
        files_done = sum(
            1 for p, sz in self.sizes.items() if self.done_by_file.get(p, 0) >= sz
        )
        try:
            self.callback(
                sum(self.done_by_file.values()),
                self.total,
                self.current_file,
                files_done,
                len(self.sizes),
            )
        except Exception as e:  # noqa: BLE001 - a progress callback must never kill a download
            _log(f"snapshots: progress callback failed: {e}")


# ---- fetch -----------------------------------------------------------------


@dataclass(frozen=True)
class _Plan:
    name: str
    revision: str
    entries: list[_Entry]
    url_for: Callable[[str], str]
    headers: dict | None
    sensitive: frozenset


def _name_lock(name: str) -> threading.Lock:
    with _registry_lock:
        return _name_locks.setdefault(name, threading.Lock())


def _write_json_atomic(path: Path, obj) -> None:
    tmp = path.with_name(path.name + ".tmp")
    tmp.write_text(json.dumps(obj))
    os.replace(tmp, path)


def _quote_path(path: str) -> str:
    return "/".join(urllib.parse.quote(seg, safe="") for seg in path.split("/"))


def _scan_staging(
    staging: Path, entries: list[_Entry]
) -> tuple[dict[str, int], set[str]]:
    """What a previous interrupted fetch already banked: verified files
    (present at their final path with the planned size) and in-flight
    .part lengths. Verified files are never re-downloaded; .parts
    resume through the 0009 machinery."""
    verified = set()
    baseline = {}
    for e in entries:
        final = staging / Path(e.path)
        part = staging / ".parts" / (e.path + ".part")
        try:
            if final.is_file() and final.stat().st_size == e.size:
                verified.add(e.path)
                baseline[e.path] = e.size
                continue
        except OSError:
            pass
        try:
            if part.is_file() and part.stat().st_size > 0:
                baseline[e.path] = part.stat().st_size
        except OSError:
            pass
    return baseline, verified


def _preflight(
    staging: Path, entries: list[_Entry], verified: set[str], baseline
) -> None:
    """§6: exact need before the first byte — summed sizes minus what
    is already verified or partial for this revision (resume must not
    double-count). The old+new peak of an upgrade falls out naturally:
    the old revision's bytes are already on disk and are never credited
    as reclaimable."""
    need = 0
    for e in entries:
        if e.path in verified:
            continue
        need += max(e.size - baseline.get(e.path, 0), 0)
    try:
        free = shutil.disk_usage(staging).free
    except OSError as e:
        raise ModelError(f"could not check disk space: {e}")
    if free < need:
        raise ModelError(
            f"not enough disk space: need {models._fmt_bytes(need)}, "
            f"have {models._fmt_bytes(free)}"
        )


class _FileError(Exception):
    def __init__(self, path: str, original: BaseException) -> None:
        self.path = path
        self.original = original
        super().__init__(f"{path}: {original}")


def _fetch_one(
    plan: _Plan, entry: _Entry, staging: Path, abort, progress: _Progress
) -> None:
    """One file through the 0009 core: resume/retries/verify inside
    .parts, then os.replace into the staging tree — a file only ever
    enters the snapshot verified."""
    part = staging / ".parts" / (entry.path + ".part")
    meta = Path(str(part) + ".json")
    final = staging / Path(entry.path)
    part.parent.mkdir(parents=True, exist_ok=True)
    final.parent.mkdir(parents=True, exist_ok=True)
    spec = models._Spec(
        entry.path,
        plan.url_for(entry.path),
        entry.digest,
        entry.size,
        plan.headers,
        plan.sensitive,
    )
    hasher = _blob_hasher(entry.size) if entry.kind == "blob-sha1" else hashlib.sha256
    algo = "git blob sha1" if entry.kind == "blob-sha1" else "sha256"
    models._download_verified(
        spec,
        part,
        meta,
        abort,
        report=lambda done, total: progress.file_progress(entry.path, done),
        new_hasher=hasher,
        algo=algo,
    )
    os.replace(part, final)
    meta.unlink(missing_ok=True)
    st = final.stat()
    return {
        "size": st.st_size,
        "mtime_ns": st.st_mtime_ns,
        "digest": entry.digest,
        "kind": entry.kind,
    }


def _fast_path(rev_dir: Path) -> Path:
    """A repeat fetch of a completed revision: .ready plus one stat
    pass over the manifest sizes — no network, no hashing. A
    <revision>/ that exists at all must be complete: the rename is
    atomic and carries .ready with it, so anything short is tampering
    or filesystem damage — loud, never silently repaired or re-fetched
    into a collision."""
    ready = rev_dir / ".ready"
    manifest = rev_dir / ".manifest.json"
    if not ready.is_file() or not manifest.is_file():
        raise ModelError(
            f"snapshot revision at {rev_dir} exists but is incomplete "
            f"(.ready or .manifest.json missing) — revision directories "
            f"are immutable and atomic; delete the revision and refetch"
        )
    try:
        plan = json.loads(manifest.read_text())
        for e in plan["entries"]:
            f = rev_dir / Path(e["path"])
            if not f.is_file() or f.stat().st_size != e["size"]:
                raise ModelError(
                    f"snapshot revision at {rev_dir} is corrupt: "
                    f"{e['path']} does not match its verified record — "
                    f"revision directories are immutable; delete the "
                    f"revision and refetch"
                )
    except (OSError, ValueError, KeyError) as e:
        raise ModelError(f"snapshot revision at {rev_dir} is unreadable: {e}")
    return rev_dir


def _finish_staging(
    staging: Path, rev_dir: Path, revision: str, entries: list[_Entry]
) -> None:
    """.ready lands last inside staging, then one rename — a crash at
    any earlier point leaves only .partial, never a <revision>/
    without .ready."""
    # .parts is pure in-flight scaffolding: every verified file has
    # been os.replace'd out of it, so the completed revision tree
    # carries no transfer residue.
    shutil.rmtree(staging / ".parts", ignore_errors=True)
    _write_json_atomic(
        staging / ".ready",
        {
            "revision": revision,
            "file_count": len(entries),
            "total_bytes": sum(e.size for e in entries),
        },
    )
    # Note: the revision is not yet reader-protected at this instant —
    # on Windows a directory cannot be renamed while any file inside it
    # is open (at any share mode), so a pre-rename .use lock is
    # impossible. The newborn-revision race is closed instead by the
    # pair of: on_verified writing current inside the <name>/.lock
    # hold, and GC skipping a name entirely while that lock is
    # contended.
    os.rename(staging, rev_dir)


def _stage_and_fetch(
    plan: _Plan, name_dir: Path, abort: threading.Event, progress_cb
) -> Path:
    revision = plan.revision
    rev_dir = name_dir / revision
    staging = name_dir / f"{revision}.partial"
    canonical = _canonical_manifest(plan.entries)

    # Crash-at-the-last-instant resume: staging already holds .ready,
    # so every file verified and only the rename never happened.
    if (staging / ".ready").is_file():
        try:
            if (
                json.loads((staging / ".manifest.json").read_text())["canonical"]
                == canonical
            ):
                os.rename(staging, rev_dir)
                return rev_dir
        except (OSError, ValueError, KeyError):
            pass

    if staging.exists():
        try:
            prior = json.loads((staging / ".manifest.json").read_text())
            same_plan = prior.get("canonical") == canonical
        except (OSError, ValueError):
            same_plan = False
        if not same_plan:
            # A different allow/ignore set (or manifest) is a different
            # snapshot: never mix plans in one staging tree.
            shutil.rmtree(staging, ignore_errors=True)
    staging.mkdir(parents=True, exist_ok=True)
    (staging / ".parts").mkdir(exist_ok=True)
    manifest_rec = {
        "revision": revision,
        "canonical": canonical,
        "entries": [
            {"path": e.path, "size": e.size, "digest": e.digest, "kind": e.kind}
            for e in sorted(plan.entries, key=lambda x: x.path)
        ],
    }
    if not (staging / ".manifest.json").is_file():
        _write_json_atomic(staging / ".manifest.json", manifest_rec)

    baseline, verified = _scan_staging(staging, plan.entries)
    _preflight(staging, plan.entries, verified, baseline)
    progress = _Progress(
        sum(e.size for e in plan.entries),
        baseline,
        progress_cb,
        {e.path: e.size for e in plan.entries},
    )
    progress._emit()

    verify_path = staging / ".verify.json"
    try:
        verify_rec = json.loads(verify_path.read_text())
    except (OSError, ValueError):
        verify_rec = {"files": {}}
    verify_lock = threading.Lock()
    # Repair records for files a crashed run placed but never recorded
    # (os.replace landed, the process died before the record write).
    # Their bytes were verified before placement, so re-recording is
    # sound — and cheap verification is what .verify.json exists for.
    repaired = False
    for e in plan.entries:
        if e.path in verified and e.path not in verify_rec["files"]:
            st = (staging / Path(e.path)).stat()
            verify_rec["files"][e.path] = {
                "size": st.st_size,
                "mtime_ns": st.st_mtime_ns,
                "digest": e.digest,
                "kind": e.kind,
            }
            repaired = True
    if repaired:
        _write_json_atomic(verify_path, verify_rec)

    todo = [e for e in plan.entries if e.path not in verified]
    # Workers only ever watch inner_abort: the supervisor loop relays
    # the caller's abort into it within one wait tick, and a sibling
    # failure sets it immediately — one event, two sources, no relay
    # threads.
    inner_abort = threading.Event()

    def work(entry: _Entry):
        if inner_abort.is_set() or abort.is_set():
            raise ModelCancelled(f"download of {plan.name!r} cancelled")
        try:
            rec = _fetch_one(plan, entry, staging, inner_abort, progress)
        except ModelCancelled:
            raise
        except Exception as e:
            if inner_abort.is_set():
                raise ModelCancelled(f"download of {plan.name!r} cancelled") from e
            raise _FileError(entry.path, e) from e
        with verify_lock:
            verify_rec["files"][entry.path] = rec
            _write_json_atomic(verify_path, verify_rec)
        progress.file_progress(entry.path, entry.size)

    with concurrent.futures.ThreadPoolExecutor(max_workers=FANOUT) as pool:
        futures = {pool.submit(work, e): e for e in todo}
        pending = set(futures)
        failure: BaseException | None = None
        while pending:
            done, pending = concurrent.futures.wait(
                pending,
                timeout=0.2,
                return_when=concurrent.futures.FIRST_EXCEPTION,
            )
            for f in done:
                exc = f.exception()
                if exc is None or failure is not None:
                    continue
                if isinstance(exc, ModelCancelled) and not abort.is_set():
                    continue  # sibling-cancel noise; the real cause is elsewhere
                failure = exc
            if failure is not None or abort.is_set():
                inner_abort.set()
            if abort.is_set() and failure is None:
                failure = ModelCancelled(f"download of {plan.name!r} cancelled")
        if failure is not None:
            if isinstance(failure, _FileError):
                orig = failure.original
                detail = f"{failure.path}: {orig}"
                raise ModelError(detail) from orig
            raise failure

    _finish_staging(staging, rev_dir, revision, plan.entries)
    return rev_dir


def fetch_snapshot(
    name: str,
    *,
    repo: str | None = None,
    revision: str | None = None,
    endpoint: str = DEFAULT_ENDPOINT,
    base_url: str | None = None,
    manifest: list[dict] | None = None,
    allow: Iterable[str] | None = None,
    ignore: Iterable[str] | None = None,
    headers: dict[str, str] | None = None,
    sensitive_headers: Iterable[str] | None = None,
    abort: threading.Event | None = None,
    progress: Callable[[int, int, str | None, int, int], None] | None = None,
    on_verified: Callable[[Path], None] | None = None,
) -> Path:
    """Fetch one pinned revision into ``<data>/models/<name>/<revision>/``
    and return that directory. ``on_verified`` fires after a fresh
    completion while the cross-process ``<name>/.lock`` is still held —
    the lifecycle layer uses it to write ``current`` inside the window
    GC cannot enter."""
    name = models._check_name(name)
    allow_g = _check_globs(allow, "allow")
    ignore_g = _check_globs(ignore, "ignore")
    if headers is not None:
        headers = {str(k): str(v) for k, v in dict(headers).items()}
    sensitive = models._ALWAYS_SENSITIVE | frozenset(
        str(h).lower() for h in (sensitive_headers or ())
    )
    abort = abort or threading.Event()

    hf = repo is not None
    if hf and (base_url is not None or manifest is not None):
        raise ValueError("repo/revision are mutually exclusive with base_url/manifest")
    if not hf and not (base_url and manifest is not None):
        raise ValueError("either repo+revision or base_url+manifest is required")

    if hf:
        if not isinstance(repo, str) or not re.match(
            r"^[A-Za-z0-9][\w.-]*/[A-Za-z0-9][\w.-]*$", repo
        ):
            raise ValueError(f"repo must look like 'org/model', got {repo!r}")
        revision = _check_revision(revision)
        models._check_scheme(endpoint)
        endpoint = endpoint.rstrip("/")

        def url_for(path: str) -> str:
            return f"{endpoint}/{repo}/resolve/{revision}/{_quote_path(path)}"

    else:
        base_url = base_url if base_url.endswith("/") else base_url + "/"
        models._check_scheme(base_url)
        entries0 = _check_manifest(manifest)
        revision = _synthetic_revision(entries0)

        def url_for(path: str) -> str:
            return base_url + _quote_path(path)

    model_root = models._data_dir()
    name_dir = model_root / name
    name_dir.mkdir(parents=True, exist_ok=True)
    rev_dir = name_dir / revision
    lockpath = name_dir / ".lock"

    # Fast path before any network: a completed revision costs one
    # stat pass (§7). A <revision>/ without .ready is structurally
    # impossible (the rename carries .ready with it) — it means
    # tampering, and _fast_path is loud about it rather than guessing.
    if rev_dir.is_dir():
        return _fast_path(rev_dir)

    name_lock = _name_lock(name)
    while True:
        if name_lock.acquire(timeout=0.2):
            break
        if abort.is_set():
            raise ModelCancelled(f"download of {name!r} cancelled")
        if rev_dir.is_dir():
            # A sibling in this process finished it while we spun.
            return _fast_path(rev_dir)
    try:
        fd = None
        while fd is None:
            if rev_dir.is_dir():
                return _fast_path(rev_dir)  # outer finally releases name_lock
            if abort.is_set():
                raise ModelCancelled(f"download of {name!r} cancelled")
            fd = models._try_file_lock(lockpath)
            if fd is None:
                time.sleep(0.2)
        try:
            if rev_dir.is_dir():
                # A fetcher in another process finished while we spun.
                return _fast_path(rev_dir)

            # The plan resolves under the lock: tree queries are cheap
            # metadata, and resolving inside keeps a second fetcher
            # from ever downloading against a different tree snapshot.
            if hf:
                entries = _resolve_tree(
                    repo, revision, endpoint, headers or {}, sensitive
                )
            else:
                entries = entries0
            entries = [e for e in entries if _selected(e.path, allow_g, ignore_g)]
            if not entries:
                raise ModelError(
                    f"nothing to fetch: allow/ignore globs exclude every "
                    f"file of {name!r}"
                )
            _check_plan_collisions(entries)
            plan = _Plan(name, revision, entries, url_for, headers, sensitive)
            out = _stage_and_fetch(plan, name_dir, abort, progress)
            if on_verified is not None:
                on_verified(out)
            return out
        finally:
            models._release_file_lock(fd)
    finally:
        name_lock.release()


# ---- lifecycle: current, reader locks, GC (PR 2) ---------------------------
#
# The locking primitives need true shared semantics: any number of
# reader processes may hold a revision's .use at once, while GC's
# exclusive probe must fail if even one does. POSIX flock has LOCK_SH
# natively; msvcrt.locking is exclusive-only, so Windows goes through
# LockFileEx (ctypes, still stdlib) which has real shared locks.

if os.name == "nt":
    import ctypes
    import msvcrt

    _LOCKFILE_EXCLUSIVE_LOCK = 0x2
    _LOCKFILE_FAIL_IMMEDIATELY = 0x1

    _GENERIC_RW = 0xC0000000
    _FILE_SHARE_ALL = 0x7
    _OPEN_ALWAYS = 4
    ctypes.windll.kernel32.CreateFileW.restype = ctypes.c_void_p

    def _open_use(path: Path):
        """Windows opens .use with FILE_SHARE_DELETE — the CRT default
        (share read/write only) would let this very handle block the
        staging rename and GC deletes, the exact sharing violations the
        layout exists to avoid."""
        handle = ctypes.windll.kernel32.CreateFileW(
            str(path), _GENERIC_RW, _FILE_SHARE_ALL, None, _OPEN_ALWAYS, 0x80, None
        )
        if not handle or handle == ctypes.c_void_p(-1).value:
            raise OSError(f"could not open {path}")
        return msvcrt.open_osfhandle(handle, os.O_RDWR)

    def _open_read_shared(path: Path):
        """Non-creating share-delete open for small control files."""
        handle = ctypes.windll.kernel32.CreateFileW(
            str(path), 0x80000000, _FILE_SHARE_ALL, None, 3, 0x80, None
        )
        if not handle or handle == ctypes.c_void_p(-1).value:
            raise OSError(f"could not open {path}")
        return msvcrt.open_osfhandle(handle, os.O_RDONLY)

    class _OVERLAPPED(ctypes.Structure):
        _fields_ = [
            ("Internal", ctypes.c_void_p),
            ("InternalHigh", ctypes.c_void_p),
            ("Offset", ctypes.c_uint32),
            ("OffsetHigh", ctypes.c_uint32),
            ("hEvent", ctypes.c_void_p),
        ]

    def _lock_use(fd, shared: bool) -> bool:
        flags = _LOCKFILE_FAIL_IMMEDIATELY
        if not shared:
            flags |= _LOCKFILE_EXCLUSIVE_LOCK
        ov = _OVERLAPPED()
        return bool(
            ctypes.windll.kernel32.LockFileEx(
                ctypes.c_void_p(msvcrt.get_osfhandle(fd)),
                flags,
                0,
                1,
                0,
                ctypes.byref(ov),
            )
        )

    def _unlock_use(fd) -> None:
        ov = _OVERLAPPED()
        ctypes.windll.kernel32.UnlockFileEx(
            ctypes.c_void_p(msvcrt.get_osfhandle(fd)), 0, 1, 0, ctypes.byref(ov)
        )

else:
    import fcntl

    def _open_use(path: Path):
        return os.open(path, os.O_CREAT | os.O_RDWR)

    def _open_read_shared(path: Path):
        return os.open(path, os.O_RDONLY)

    def _lock_use(fd, shared: bool) -> bool:
        op = fcntl.LOCK_SH if shared else fcntl.LOCK_EX
        try:
            fcntl.flock(fd, op | fcntl.LOCK_NB)
            return True
        except OSError:
            return False

    def _unlock_use(fd) -> None:
        fcntl.flock(fd, fcntl.LOCK_UN)


# One fd per revision per process, held for the process lifetime and
# never closed: the kernel releases on death, exactly like the 0009
# fetch lock. The registry makes repeat path() calls idempotent.
_readers: dict[str, int] = {}
_readers_lock = threading.Lock()


def read_current(name_dir: Path) -> str | None:
    # Windows: read through the share-delete open — a CRT open would
    # momentarily deny the os.replace in write_current, and under
    # concurrent readers those windows overlap into a hard failure.
    try:
        fd = _open_read_shared(name_dir / "current")
    except OSError:
        return None
    try:
        with os.fdopen(fd, "rb") as f:
            data = f.read().decode().strip()
    except (OSError, UnicodeDecodeError):
        return None
    return data or None


def write_current(name_dir: Path, revision: str) -> None:
    """The pointer switch: a small file plus os.replace, atomic on
    every OS including Windows — no directory rename ever touches a
    revision a reader might hold. The bounded retry is standard
    Windows practice for atomic replace: virus scanners and indexers
    transiently deny the destination, which is noise, not a signal."""
    tmp = name_dir / "current.tmp"
    tmp.write_text(revision)
    for _ in range(100):
        try:
            os.replace(tmp, name_dir / "current")
            return
        except PermissionError:
            time.sleep(0.01)
    os.replace(tmp, name_dir / "current")


def _adopt_reader_fd(rev_dir: Path, fd: int) -> None:
    """Register a shared-locked .use fd for the process lifetime; if
    another thread beat us to it theirs wins and ours is released."""
    key = str(rev_dir)
    with _readers_lock:
        if key not in _readers:
            _readers[key] = fd
            return
    _unlock_use(fd)
    os.close(fd)


def _hash_kind(f: Path, kind: str, size: int) -> str:
    h = hashlib.sha1(b"blob %d\0" % size) if kind == "blob-sha1" else hashlib.sha256()
    with open(f, "rb") as fh:
        while b := fh.read(models._REHASH):
            h.update(b)
    return h.hexdigest()


def _cheap_check(rev_dir: Path) -> None:
    """The §3 cheap check: .ready and .manifest.json present, then one
    stat per verified file against the .verify.json record — size and
    mtime. A changed mtime re-hashes just that file before deciding,
    mirroring 0009 §1. Any failure is ModelNotReady, never a wrong Path."""
    if not (rev_dir / ".ready").is_file():
        raise models.ModelNotReady(f"snapshot at {rev_dir} has no .ready marker")
    if not (rev_dir / ".manifest.json").is_file():
        raise models.ModelNotReady(f"snapshot at {rev_dir} has no .manifest.json")
    try:
        verify = json.loads((rev_dir / ".verify.json").read_text())
        files = verify["files"]
    except (OSError, ValueError, KeyError) as e:
        raise models.ModelNotReady(
            f"snapshot at {rev_dir} has an unreadable verify record: {e}"
        )
    for path, rec in files.items():
        f = rev_dir / Path(path)
        try:
            st = f.stat()
        except OSError:
            raise models.ModelNotReady(f"snapshot file {path} is missing")
        if st.st_size != rec["size"]:
            raise models.ModelNotReady(
                f"snapshot file {path} changed size — the revision is corrupt"
            )
        if st.st_mtime_ns != rec["mtime_ns"] and (
            _hash_kind(f, rec["kind"], rec["size"]) != rec["digest"]
        ):
            raise models.ModelNotReady(
                f"snapshot file {path} failed re-verification — the revision is corrupt"
            )


def snapshot_path(name_dir: Path, revision: str | None = None) -> Path:
    """The §7 reader protocol: read current, open and shared-lock
    <rev>/.use, recheck .ready under the held lock, then cheap-check.
    The ordering closes the race with a switching upgrade plus another
    process's GC: a present .ready under a held shared lock proves the
    directory is not being deleted, because GC deletes .ready first and
    can only win the exclusive lock when no shared lock exists."""
    pinned = revision is not None
    last_current = None
    for _attempt in range(1 if pinned else 3):
        rev = revision or read_current(name_dir)
        last_current = rev
        if rev is None:
            raise models.ModelNotReady(
                f"snapshot in {name_dir} has no current revision — nothing has been fetched"
            )
        rev_dir = name_dir / rev
        key = str(rev_dir)
        with _readers_lock:
            registered = key in _readers
        if not registered:
            try:
                fd = _open_use(rev_dir / ".use")
            except OSError:
                if pinned:
                    raise models.ModelNotReady(
                        f"snapshot revision {rev} is not present"
                    )
                continue  # being deleted under us — re-read current
            if not _lock_use(fd, shared=True):
                os.close(fd)
                if pinned:
                    raise models.ModelNotReady(
                        f"snapshot revision {rev} is being deleted"
                    )
                continue
            if not (rev_dir / ".ready").is_file():
                # GC won the race before we locked: .ready-first deletion
                # means this revision is never handed out.
                _unlock_use(fd)
                os.close(fd)
                if pinned:
                    raise models.ModelNotReady(f"snapshot revision {rev} is not ready")
                continue
            _adopt_reader_fd(rev_dir, fd)
        _cheap_check(rev_dir)
        return rev_dir
    raise models.ModelNotReady(
        f"revision switched during lookup; current is {last_current} — "
        f"retry, or pin revision= to ride out an upgrade"
    )


def gc_name(name_dir: Path) -> None:
    """One GC pass over a snapshot name (§7). Never touches the
    revision current names. A revision whose exclusive .use probe fails
    has a live reader — skip silently. When the probe wins, delete
    .ready FIRST, then the rest of the tree, holding the lock
    throughout: a reader slipping in afterwards fails the recheck and
    retries onto current, never landing in a partial directory. Windows
    sharing violations mid-delete are 'still in use', never an error —
    residue is retried at the next pass. .partial residue is collected
    only when <name>/.lock proves there is no live writer."""
    if not name_dir.is_dir():
        return
    current = read_current(name_dir)
    lockpath = name_dir / ".lock"
    name_lock_fd = models._try_file_lock(lockpath)
    if name_lock_fd is None:
        # A live fetch holds <name>/.lock: skip the whole name. This is
        # what makes the newborn-revision window airtight — between the
        # rename and the current write, no GC anywhere can probe the
        # just-born revision.
        return
    try:
        children = sorted(name_dir.iterdir())
    except OSError:
        models._release_file_lock(name_lock_fd)
        return
    for child in children:
        if child.name in ("current", ".lock"):
            continue
        if child.is_dir() and child.name.endswith(".partial"):
            # .lock is already ours — proof there is no live writer.
            shutil.rmtree(child, ignore_errors=True)
            continue
        if not child.is_dir() or child.name == current or not _SHA_RE.match(child.name):
            continue
        try:
            fd = _open_use(child / ".use")
        except OSError:
            continue
        if not _lock_use(fd, shared=False):
            os.close(fd)
            continue  # a live reader — skip silently
        try:
            try:
                (child / ".ready").unlink()
            except OSError:
                pass
            shutil.rmtree(child, ignore_errors=True)
        finally:
            _unlock_use(fd)
            os.close(fd)
        # .use itself and the dir go once our fd is closed (Windows
        # cannot delete an open file); leftover residue — a mmap'd
        # shard in a dying process — is next pass's problem, never an
        # error.
        shutil.rmtree(child, ignore_errors=True)
    models._release_file_lock(name_lock_fd)


def gc_all(model_root: Path) -> None:
    """Startup sweep: every directory in the models root is a snapshot
    name (single-file models are files and never match). Never raises —
    GC is bookkeeping, not correctness."""
    try:
        children = sorted(model_root.iterdir())
    except OSError:
        return
    for d in children:
        if d.is_dir():
            try:
                gc_name(d)
            except Exception as e:  # noqa: BLE001 - GC must never crash a runtime
                _log(f"snapshots: GC pass over {d} failed: {e}")


def _reset() -> None:
    """Test isolation: clear in-process lock state. Reader fds stay
    registered — they model process-lifetime holds, and tests use fresh
    tmp dirs so stale keys never collide."""
    with _registry_lock:
        _name_locks.clear()
