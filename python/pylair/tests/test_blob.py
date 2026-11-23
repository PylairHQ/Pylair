"""Blob values — decision 0010, Python side.

A `Blob` in a result or stream chunk serializes as
`{"$blob": {"id","mime","size"}}`; its file lives in the generation's
`PYLAIR_BLOB_DIR` and the runtime flushes `blob.register` (id, path,
mime, size, task_id) ahead of the carrying frame. With the env unset the
value degrades to `$b64` — or fails LIMIT_EXCEEDED naming the feature
when the fallback can't fit the frame cap. Workers register through the
parent; the host owns the unlink.
"""

from __future__ import annotations

import base64
import json
import os
import re
from pathlib import Path

import pytest

import pylair
from conftest import Runtime
from pylair import _json
from pylair import _protocol as proto

_ID_RE = re.compile(r"^[A-Za-z0-9_-]{20,}$")


def call(rid: int, fn: str, task: int, input: dict | None = None) -> dict:
    return proto.request(rid, "call", {"fn": fn, "task": task, "input": input or {}})


def grant(task: int, n: int = proto.INITIAL_STREAM_CREDIT) -> dict:
    """The host's stream window — required before a generator emits."""
    return proto.notification("stream.credit", {"task": task, "n": n})


# ---- unit: encoding, ids, from_path -------------------------------------------


def test_blob_serializes_to_tag(tmp_path, monkeypatch):
    monkeypatch.setenv("PYLAIR_BLOB_DIR", str(tmp_path))
    b = pylair.Blob(b"hello-blob", mime="text/plain")
    out = json.loads(_json.dumps({"x": b}))
    tag = out["x"]["$blob"]
    assert tag["mime"] == "text/plain"
    assert tag["size"] == 10
    assert _ID_RE.match(tag["id"])
    # File materialized fully + closed before encode returned.
    files = list(tmp_path.iterdir())
    assert len(files) == 1 and files[0].read_bytes() == b"hello-blob"


def test_blob_id_alphabet():
    ids = {pylair.Blob(b"").id for _ in range(50)}
    assert len(ids) == 50
    assert all(_ID_RE.match(i) for i in ids)


def test_blob_is_check_wire_leaf(tmp_path, monkeypatch):
    monkeypatch.setenv("PYLAIR_BLOB_DIR", str(tmp_path))
    # A Blob inside a nested container is a leaf — no recursion into its
    # guts, no depth penalty.
    _json.check_wire({"deep": [[pylair.Blob(b"x")]]})


def test_from_path_hardlinks(tmp_path, monkeypatch):
    monkeypatch.setenv("PYLAIR_BLOB_DIR", str(tmp_path))
    src = tmp_path / "src.bin"
    src.write_bytes(b"from-path-bytes")
    b = pylair.Blob.from_path(src, mime="text/plain")
    regs: dict = {}
    _json.dumps_collect(b, regs)
    assert b.path is not None
    assert os.path.samefile(src, b.path), "from_path should hardlink, not copy"
    assert b.path.parent == tmp_path
    assert regs[b.id] is b


def test_from_path_copy_true(tmp_path, monkeypatch):
    monkeypatch.setenv("PYLAIR_BLOB_DIR", str(tmp_path))
    src = tmp_path / "src.txt"
    src.write_bytes(b"copy-me")
    b = pylair.Blob.from_path(src, mime="text/plain", copy=True)
    _json.dumps_collect(b, {})
    assert not os.path.samefile(src, b.path)
    assert b.path.read_bytes() == b"copy-me"


def test_failed_serialize_unlinks_unregistered_files(tmp_path, monkeypatch):
    """A blob written but never carried (a sibling fails the dump) is
    ours to remove — the doc's error-path unlink. Dict order puts the
    Blob's materialize before the TypeError."""
    monkeypatch.setenv("PYLAIR_BLOB_DIR", str(tmp_path))
    b = pylair.Blob(b"orphan")
    with pytest.raises(TypeError):
        _json.dumps_collect({"blob": b, "bad": object()}, {})
    assert list(tmp_path.iterdir()) == []


def test_mime_rejected_at_construction():
    with pytest.raises(ValueError):
        pylair.Blob(b"x", mime="text/plain\r\nX-Evil: 1")


# ---- wire: register-before-use ordering -------------------------------------


def test_register_precedes_response(tmp_path):
    rt = Runtime(env={"PYLAIR_BLOB_DIR": str(tmp_path)})
    try:
        rt.recv()  # hello
        rt.send(call(1, "make_blob", task=7))
        first = rt.recv()
        assert first.get("method") == "blob.register", (
            f"expected register before response, got {first}"
        )
        p = first["params"]
        assert p["task_id"] == 7
        assert _ID_RE.match(p["id"])
        assert p["mime"] == "image/png"
        # The file is complete on disk when the register lands.
        assert Path(p["path"]).read_bytes() == b"tiny-png-ish-bytes"
        second = rt.recv()
        assert second["id"] == 1
        tag = second["result"]["$blob"]
        assert tag["id"] == p["id"] and tag["size"] == 18
        # Python never deletes a registered file — host owns the unlink.
        assert Path(p["path"]).exists()
    finally:
        rt.kill()
        rt.proc.wait(timeout=10)


def test_register_precedes_stream_chunk(tmp_path):
    rt = Runtime(env={"PYLAIR_BLOB_DIR": str(tmp_path)})
    try:
        rt.recv()
        rt.send(call(1, "stream_blob", task=9))
        rt.send(grant(9))
        frames = []
        while True:
            m = rt.recv()
            frames.append(m)
            if m.get("id") == 1:
                break
        kinds = [m.get("method") for m in frames]
        # chunk("lead") first, then register, then the $blob chunk,
        # then the response.
        assert kinds == ["stream.chunk", "blob.register", "stream.chunk", None]
        p = frames[1]["params"]
        assert p["task_id"] == 9
        tag = frames[2]["params"]["value"]["$blob"]
        assert tag["id"] == p["id"]
        assert frames[3]["result"] == {"done": True}
    finally:
        rt.kill()
        rt.proc.wait(timeout=10)


def test_worker_isolated_blob_registers_through_parent(tmp_path):
    rt = Runtime(env={"PYLAIR_BLOB_DIR": str(tmp_path)})
    try:
        rt.recv()
        rt.send(call(1, "isolated_blob", task=3))
        reg = rt.recv()
        assert reg.get("method") == "blob.register"
        p = reg["params"]
        assert p["task_id"] == 3, "worker blob must bind to the parent's task"
        assert Path(p["path"]).parent == tmp_path
        assert Path(p["path"]).read_bytes() == b"isolated-worker-bytes"
        res = rt.recv()
        assert res["result"]["blob"]["$blob"]["id"] == p["id"]
        assert res["result"]["dir"] == str(tmp_path), "worker inherits the dir"
    finally:
        rt.kill()
        rt.proc.wait(timeout=10)


def test_worker_warm_blob_registers_through_parent(tmp_path):
    """The warm path's private framed channel carries the same `blobs`
    field — and the same worker stays warm afterwards."""
    rt = Runtime(env={"PYLAIR_BLOB_DIR": str(tmp_path)})
    try:
        rt.recv()
        rt.send(call(1, "warm_blob", task=5))
        reg = rt.recv()
        assert reg.get("method") == "blob.register"
        p = reg["params"]
        assert p["task_id"] == 5
        assert Path(p["path"]).read_bytes() == b"warm-worker-bytes"
        res = rt.recv()
        assert res["result"]["blob"]["$blob"]["id"] == p["id"]
        pid = res["result"]["pid"]
        # Warm worker still alive — a second call lands on the same pid
        # (warm pools are per-function).
        rt.send(call(2, "warm_blob", task=6))
        while True:
            m = rt.recv()
            if m.get("id") == 2:
                break
        assert m["result"]["pid"] == pid
    finally:
        rt.kill()
        rt.proc.wait(timeout=10)


# ---- capability off (no PYLAIR_BLOB_DIR) ------------------------------------


def test_capability_off_falls_back_to_b64():
    rt = Runtime(env={"PYLAIR_BLOB_DIR": ""})
    try:
        rt.recv()
        rt.send(call(1, "make_blob", task=1))
        res = rt.recv()
        assert res["id"] == 1
        raw = base64.b64decode(res["result"]["$b64"])
        assert raw == b"tiny-png-ish-bytes"
    finally:
        rt.kill()
        rt.proc.wait(timeout=10)


def test_capability_off_oversized_names_the_feature(tmp_path):
    # A Blob whose $b64 can't fit a tiny frame cap → LIMIT_EXCEEDED with
    # the feature named, not a dropped value or a dead runtime.
    src = tmp_path / "big.bin"
    src.write_bytes(b"x" * 4096)
    rt = Runtime(env={"PYLAIR_BLOB_DIR": "", "PYLAIR_MAX_FRAME": "512"})
    try:
        rt.recv()
        rt.send(call(1, "make_blob_path", task=1, input={"tmp_name": str(src)}))
        res = rt.recv()
        err = res["error"]
        assert err["code"] == proto.LIMIT_EXCEEDED
        assert "PYLAIR_BLOB_DIR" in err["message"]
        assert err["data"]["feature"] == "blob"
        # Runtime still alive — a later normal call answers.
        rt.send(call(2, "add", task=2, input={"a": 1, "b": 2}))
        assert rt.recv()["result"] == 3
    finally:
        rt.kill()
        rt.proc.wait(timeout=10)
