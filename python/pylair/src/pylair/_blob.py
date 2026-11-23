"""Blob values — large binary payloads returned as `pylair-blob` URLs.

docs/decisions/0010-blob-urls.md. A `Blob` in a result or stream chunk
serializes as `{"$blob": {"id","mime","size"}}`; its bytes live in the
generation's `PYLAIR_BLOB_DIR` and the host's registry learns the file
through a `blob.register` notification flushed ahead of the carrying
frame (registration-before-use — the host never serves an unregistered
id).

A `Blob` object is an IMMUTABLE DESCRIPTION of its source — bytes or a
path plus a mime — deliberately sendable more than once (a module-level
thumbnail cached for the app's life). Every serialization pass that
carries it mints a fresh `_BlobHandle`: a new id and a new file,
hard-linked from the source path or written from the bytes. Per-send
state lives in the collecting pass's `regs` map keyed by object
identity, so the same object twice in one frame is one handle while two
calls returning it get independent handles — a later `release()` of one
can never orphan the other.

The env var is the capability bit (§3): with `PYLAIR_BLOB_DIR` unset —
an older host, or the host could not create the dir — a Blob degrades to
`{"$b64": ...}` when that fits the frame cap, else the call fails
LIMIT_EXCEEDED naming the feature. Never a silent drop.

Python side is passive for lifetime (§4): once `blob.register` has been
flushed the HOST owns the file's unlink (release, window close,
generation end) — Python code never deletes a registered file. Files
materialized but never carried by a frame (a serialization that failed
mid-pass, or a frame refused for lacking a task) are unlinked on the
error path; everything past that is the generation-end sweep.
"""

from __future__ import annotations

import base64
import os
import secrets
import shutil
import tempfile
from pathlib import Path
from typing import Any

_ENV_DIR = "PYLAIR_BLOB_DIR"
_B64_TAG = "$b64"
_BLOB_TAG = "$blob"
_OCTET = "application/octet-stream"


def _blob_dir() -> Path | None:
    d = os.environ.get(_ENV_DIR)
    return Path(d) if d else None


def _new_id() -> str:
    """128-bit unguessable id on the crate's accepted alphabet —
    token_urlsafe chars are exactly `[A-Za-z0-9_-]`.
    """
    return secrets.token_urlsafe(16)


def _check_mime(mime: str) -> str:
    """Reject a mime the host could never put in a Content-Type header —
    the crate refuses anything but printable ASCII/space/tab, and a
    rejected registration would 404 the blob at fetch time. Catch it at
    construction instead.
    """
    if not isinstance(mime, str) or not mime:
        raise ValueError("mime must be a non-empty string")
    if not all(c.isascii() and (c.isprintable() or c in " \t") for c in mime):
        raise ValueError("mime must be printable ASCII")
    return mime


class BlobFallbackExceeded(Exception):
    """A Blob needs the `$b64` fallback (no `PYLAIR_BLOB_DIR` — an older
    host or a dir that failed to resolve) but the encoded value cannot fit
    the host's frame cap. Raised by check_wire so the send boundary maps
    it to LIMIT_EXCEEDED naming the feature (decision 0010 §3).
    """


class Blob:
    """A binary payload the host serves back to the owning webview as a
    `pylair-blob:` URL — no +33% b64, no 16 MiB frame cap on the data.

    `Blob(data, mime=...)` keeps the bytes until encode time, when they
    are written atomically (tmp + rename) under `PYLAIR_BLOB_DIR`.
    `Blob.from_path(path, mime=...)` hardlinks the file into the dir
    (zero-copy) with a plain-copy fallback across filesystems; the host
    re-stats size+mtime per serve, so an in-place rewrite of the source
    answers 410 Gone instead of stale bytes. `copy=True` forces a copy
    for sources that will be rewritten in place.

    The object itself is only a description — `id` and a materialized
    `path` are per-SEND state and do not exist here; each serialization
    pass mints a fresh handle so the same Blob can be returned from any
    number of calls.
    """

    def __init__(self, data: bytes | bytearray | memoryview, mime: str = _OCTET):
        self._mime = _check_mime(mime)
        try:
            self._data: bytes | None = bytes(data)
        except TypeError:
            raise TypeError(
                f"Blob data must be bytes-like, got {type(data).__name__}"
            ) from None
        self._src: Path | None = None
        self._copy = False

    @classmethod
    def from_bytes(
        cls, data: bytes | bytearray | memoryview, mime: str = _OCTET
    ) -> Blob:
        """Named-constructor spelling of `Blob(data, mime)` (the doc's
        constructor reads `pylair.Blob(data, mime=...)`; both exist).
        """
        return cls(data, mime)

    @classmethod
    def from_path(
        cls, path: str | os.PathLike, mime: str = _OCTET, copy: bool = False
    ) -> Blob:
        """Reference an existing file — hardlink into the blob dir, so the
        bytes are served without a second copy. `copy=True` detaches the
        content (the doc's escape hatch for sources rewritten in place).
        """
        p = Path(path)
        if not p.is_file():
            raise FileNotFoundError(f"Blob.from_path: no file at {p}")
        b = cls(b"", mime)
        b._data = None
        b._src = p
        b._copy = copy
        return b

    @property
    def mime(self) -> str:
        return self._mime

    @property
    def size(self) -> int:
        """The source's current size — for `from_path` this is a live
        stat, so an in-place rewrite shows here until the next send
        mints a handle (the host answers 410 for a stale registration).
        """
        if self._data is not None:
            return len(self._data)
        assert self._src is not None
        return self._src.stat().st_size

    def _bytes(self) -> bytes:
        if self._data is not None:
            return self._data
        assert self._src is not None
        return self._src.read_bytes()

    def _to_wire(self, regs: dict | None) -> dict:
        """The value `_default` emits. With `regs` — a collecting pass —
        mints (or reuses within the pass) this object's handle; with
        None the tag still materializes a file (a plain dumps produces a
        blob the host will serve once its register lands — or 404).
        """
        if _blob_dir() is None:
            # Capability off — degrade to b64 (check_wire has already
            # proven it fits the frame cap).
            return {_B64_TAG: base64.b64encode(self._bytes()).decode("ascii")}
        if regs is not None and self in regs:
            return regs[self]._tag
        h = _BlobHandle(self)
        if regs is not None:
            regs[self] = h
        return h._tag

    def _fallback_check(self) -> None:
        """check_wire's leaf visit: with the capability off the fallback
        is `$b64` — prove it fits before any file is read or written.
        """
        if _blob_dir() is not None:
            return
        # Import here — _transport doesn't import _json/_blob, but the
        # top of this file must stay light for _json's own imports.
        from pylair._transport import MAX_FRAME

        n = self.size
        # b64 inflates 4/3 and the tag/envelope adds a little — a value
        # that can't fit MAX_FRAME never reaches the wire.
        if (n + 2) // 3 * 4 + 64 > MAX_FRAME:
            raise BlobFallbackExceeded(
                f"result contains a {n}-byte Blob but {_ENV_DIR} is unset "
                "(older host or the blob dir failed to resolve) and its "
                f"$b64 fallback exceeds the {MAX_FRAME}-byte frame cap — "
                f"upgrade the host or return smaller values"
            )


class _BlobHandle:
    """One send of a Blob — a fresh id, its own file in the blob dir,
    and the wire artifacts (`$blob` tag + `blob.register` params). Held
    in the collecting pass's `regs` map, never on the Blob.
    """

    __slots__ = ("_blob", "_id", "_path", "_registered", "_size", "_tag")

    def __init__(self, blob: Blob):
        self._id = _new_id()
        self._blob = blob
        self._path = self._materialize()
        self._size = self._path.stat().st_size
        self._tag = {
            _BLOB_TAG: {"id": self._id, "mime": blob._mime, "size": self._size}
        }
        self._registered = False  # a register frame for it reached the queue

    def _materialize(self) -> Path:
        """Write/link the file into `PYLAIR_BLOB_DIR`, fully written and
        closed before it returns (the host re-stats at serve time, so
        nothing may touch the file after registration).
        """
        d = _blob_dir()
        assert d is not None, "_materialize requires PYLAIR_BLOB_DIR"
        dest = d / self._id
        src = self._blob._src
        if src is not None and not self._blob._copy:
            try:
                # Zero-copy when the source shares the volume (the common
                # case — both live under the app's dirs).
                os.link(src, dest)
            except OSError:
                # Cross-filesystem or a fs without hardlinks — plain copy.
                shutil.copyfile(src, dest)
        else:
            # Atomic write: a tmp name + rename, so a reader can never
            # observe a partial file.
            fd, tmp = tempfile.mkstemp(dir=d, prefix=".tmp-", suffix=self._id)
            try:
                with os.fdopen(fd, "wb") as f:
                    f.write(self._blob._bytes())
                os.replace(tmp, dest)
            except BaseException:
                try:
                    os.unlink(tmp)
                except OSError:
                    pass
                raise
        return dest

    def _register_params(self) -> dict[str, Any]:
        """The `blob.register` params — host-internal metadata (the path
        never reaches a webview). The runtime adds task_id.
        """
        return {
            "id": self._id,
            "path": str(self._path),
            "mime": self._blob._mime,
            "size": self._size,
        }

    def _unlink(self) -> None:
        """Error path: a file written but never registered is ours to
        remove. Once a register frame has been queued the host owns the
        unlink — this becomes a no-op (§4).
        """
        if self._registered:
            return
        try:
            os.unlink(self._path)
        except OSError:
            pass
