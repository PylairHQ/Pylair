"""Snapshot lifecycle (decision 0013 PR 2): the public API, current,
reader locks, GC, readiness no-flicker and the name-collision rules.

Loopback-only against the PR 1 fixture server. Covered: held-Path
immutability across an upgrade, the path()/GC interleaving never
handing out a partial directory, GC skipping a revision locked by a
live subprocess and collecting it after exit, readiness never leaving
ready during an upgrade, single-file/snapshot name collisions,
HF_TOKEN adoption and cross-origin stripping, the revision pin, a
failed upgrade still serving, and — Windows-only — a mmap'd shard
surviving GC until the holder exits. The e2e block drives a real
`python -m pylair testapp_snapshots` for requires= gating and the
warm-worker resolve-only contract. No sleeps for timing.
"""

from __future__ import annotations

import hashlib
import os
import subprocess
import sys
import threading
import time
from pathlib import Path

import pytest

from conftest import Runtime
from models_server import File, ModelServer, Repo
from pylair import _protocol as proto
from pylair import _readiness, _snapshots, models
from pylair.models import ModelError, ModelNotReady

REV = "a" * 40
REV2 = "b" * 40


@pytest.fixture(autouse=True)
def _state(monkeypatch, tmp_path):
    monkeypatch.setenv("PYLAIR_APP_DATA_DIR", str(tmp_path))
    monkeypatch.setenv("PYLAIR_MODELS_ALLOW_INSECURE", "1")
    monkeypatch.delenv("HF_TOKEN", raising=False)
    monkeypatch.delenv("HF_ENDPOINT", raising=False)
    models._reset()
    _snapshots._reset()
    _readiness.reset()
    yield
    models._reset()
    _snapshots._reset()
    _readiness.reset()


@pytest.fixture
def server():
    for _ in range(6):
        srv = ModelServer()
        if srv.probe():
            break
        srv.close()
    else:
        pytest.fail("loopback server never accepted a connection")
    yield srv
    srv.close()


def _wait(pred, timeout: float = 15.0, what: str = "condition") -> None:
    deadline = time.monotonic() + timeout
    while not pred():
        if time.monotonic() > deadline:
            raise AssertionError(f"{what} never met")
        time.sleep(0.005)


def _sha(body: bytes) -> str:
    return hashlib.sha256(body).hexdigest()


def _repo(rev: str, tag: bytes) -> Repo:
    return Repo(
        rev,
        {
            "config.json": b'{"tag": "' + tag + b'"}',
            "tokenizer.json": b'{"v": "' + tag + b'"}' * 20,
            "model.safetensors": tag * 30_000,
        },
        lfs={"model.safetensors"},
    )


def _add_two_revisions(server) -> None:
    server.add_repo("org/model", _repo(REV, b"one"))
    server.add_repo("org/model", _repo(REV2, b"two"))


def _fetch_public(name, server, rev=REV, **kw):
    return models.fetch_snapshot(
        name, repo="org/model", revision=rev, endpoint=server.base, **kw
    )


def _component(name: str):
    return _readiness._default._components[name]


# ---- current + path() --------------------------------------------------------


def test_current_written_and_path_resolves(server, tmp_path):
    server.add_repo("org/model", _repo(REV, b"one"))
    out = _fetch_public("m1", server)
    assert (tmp_path / "models" / "m1" / "current").read_text() == REV
    p = models.path("m1")
    assert p == out
    assert (p / "config.json").read_bytes() == b'{"tag": "one"}'
    assert (p / ".use").exists()  # the reader lock file is part of the layout


def test_held_path_immutable_across_upgrade(server, tmp_path):
    _add_two_revisions(server)
    p1 = _fetch_public("m1", server)
    before = {
        f: (p1 / f).read_bytes()
        for f in ("config.json", "tokenizer.json", "model.safetensors")
    }
    models.path("m1")  # the process now holds a shared lock on rev1

    p2 = _fetch_public("m1", server, rev=REV2)
    assert p2 != p1
    assert (tmp_path / "models" / "m1" / "current").read_text() == REV2
    # the held Path still reads the old revision's bytes, exactly
    for f, data in before.items():
        assert (p1 / f).read_bytes() == data
    # fresh resolution lands on the new revision
    assert models.path("m1") == p2
    assert (p2 / "config.json").read_bytes() == b'{"tag": "two"}'
    # GC did not delete the in-use old revision
    assert p1.is_dir()


def test_revision_pin_returns_specific(server, tmp_path):
    _add_two_revisions(server)
    p1 = _fetch_public("m1", server)
    models.path("m1")  # lock rev1 in this process — unpinned GC would collect it
    p2 = _fetch_public("m1", server, rev=REV2)
    assert models.path("m1", revision=REV) == p1
    assert models.path("m1", revision=REV2) == p2
    assert models.path("m1") == p2  # unpinned follows current
    with pytest.raises(ModelNotReady):
        models.path("m1", revision="c" * 40)


def test_path_without_fetch_is_not_ready(server):
    with pytest.raises(ModelNotReady, match="not downloaded and verified"):
        models.path("m1")


# ---- GC ----------------------------------------------------------------------


def test_gc_removes_unlocked_old_revision(server, tmp_path):
    _add_two_revisions(server)
    p1 = _fetch_public("m1", server)
    p2 = _fetch_public("m1", server, rev=REV2)
    # no reader ever locked rev1 in this process, so the end-of-fetch
    # GC pass collected it already or the next one does
    if p1.is_dir():
        _snapshots.gc_name(tmp_path / "models" / "m1")
    assert not p1.is_dir()
    assert p2.is_dir()  # current is never collected
    assert (tmp_path / "models" / "m1" / "current").read_text() == REV2


_LOCK_CHILD = """
import sys, time
from pathlib import Path
from pylair import _snapshots
fd = _snapshots._open_use(Path(sys.argv[1]) / ".use")
assert _snapshots._lock_use(fd, shared=True)
print("locked", flush=True)
time.sleep(120)
"""


def test_gc_subprocess_lock_lifecycle(server, tmp_path):
    """A revision shared-locked by a live subprocess survives GC; once
    the child exits the next pass removes it. This process never locks
    rev1 itself, so the child's lock is the only protection."""
    _add_two_revisions(server)
    p1 = _fetch_public("m1", server)
    child = subprocess.Popen(
        [sys.executable, "-c", _LOCK_CHILD, str(p1)],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env={**os.environ, "PYLAIR_APP_DATA_DIR": str(tmp_path)},
        text=True,
    )
    try:
        assert child.stdout.readline().strip() == "locked"
        _fetch_public("m1", server, rev=REV2)
        for _ in range(3):
            _snapshots.gc_name(tmp_path / "models" / "m1")
        assert p1.is_dir(), "GC deleted a revision a live process holds"
        assert (p1 / "model.safetensors").is_file()
    finally:
        child.kill()
        child.wait(timeout=10)
    deadline = time.monotonic() + 10
    while p1.is_dir() and time.monotonic() < deadline:
        _snapshots.gc_name(tmp_path / "models" / "m1")
        if not p1.is_dir():
            break
        time.sleep(0.05)
    assert not p1.is_dir(), "GC never collected the unlocked revision"


def test_gc_collects_partial_residue(server, tmp_path):
    server.add_repo("org/model", _repo(REV, b"one"))
    _fetch_public("m1", server)
    leftover = tmp_path / "models" / "m1" / f"{REV2}.partial"
    leftover.mkdir()
    (leftover / "stray").write_text("x")
    _snapshots.gc_name(tmp_path / "models" / "m1")
    assert not leftover.exists()


def test_gc_never_touches_current(server, tmp_path):
    server.add_repo("org/model", _repo(REV, b"one"))
    p1 = _fetch_public("m1", server)
    _snapshots.gc_name(tmp_path / "models" / "m1")
    assert p1.is_dir()
    assert (p1 / ".ready").is_file()


def test_ready_deleted_first_blocks_handout(server, tmp_path):
    """The GC-half invariant: a revision whose .ready is gone is never
    handed out even while the rest of the tree still exists."""
    server.add_repo("org/model", _repo(REV, b"one"))
    p1 = _fetch_public("m1", server)
    (p1 / ".ready").unlink()
    with pytest.raises(ModelNotReady):
        models.path("m1", revision=REV)


# ---- the path()/GC race ------------------------------------------------------


def test_path_gc_race_never_yields_partial(server, tmp_path):
    """Readers resolving in a loop while upgrades switch current back
    and forth and GC runs at every fetch boundary: every resolution is
    a complete revision, never a partial directory, never a flicker."""
    _add_two_revisions(server)
    _fetch_public("m1", server)
    errors = []
    stop = threading.Event()

    def reader():
        while not stop.is_set():
            try:
                p = models.path("m1")
                if not (p / ".ready").is_file():
                    errors.append(f"partial handed out: {p}")
            except ModelNotReady:
                errors.append("ModelNotReady during upgrade — flicker")

    threads = [threading.Thread(target=reader, daemon=True) for _ in range(3)]
    for t in threads:
        t.start()
    for _ in range(3):
        _fetch_public("m1", server, rev=REV2)
        _fetch_public("m1", server, rev=REV)
    stop.set()
    for t in threads:
        t.join(timeout=10)
    assert errors == [], errors[:3]


# ---- readiness ---------------------------------------------------------------


def test_readiness_no_flicker_during_upgrade(server, tmp_path):
    """With a verified current serving, an upgrade never moves the
    component off ready — progress surfaces in its detail instead."""
    _add_two_revisions(server)
    _fetch_public("m1", server)
    assert _component("m1").phase == "ready"

    # gate the big file mid-upgrade so the observation window is wide
    f = server.files[f"/org/model/resolve/{REV2}/model.safetensors"]
    f.gate = threading.Event()
    phases = []
    done = threading.Event()

    def upgrade():
        try:
            _fetch_public("m1", server, rev=REV2)
        finally:
            done.set()

    t = threading.Thread(target=upgrade, daemon=True)
    t.start()
    _wait(
        lambda: server.hits(f"/org/model/resolve/{REV2}/model.safetensors") >= 1,
        what="upgrade started",
    )
    phases.append(_component("m1").phase)
    detail = _component("m1").detail or ""
    f.gate.set()
    done.wait(timeout=15)
    phases.append(_component("m1").phase)
    assert phases == ["ready", "ready"], phases
    assert "upgrading" in detail
    assert (tmp_path / "models" / "m1" / "current").read_text() == REV2


def test_failed_upgrade_still_serves(server, tmp_path):
    server.add_repo("org/model", _repo(REV, b"one"))
    bad = _repo(REV2, b"two")
    # corrupt the LFS content after tree registration: the tree lies,
    # verification must fail
    server.add_repo("org/model", bad)
    key = f"/org/model/resolve/{REV2}/model.safetensors"
    server.files[key] = File(b"corrupted" * 1000)
    p1 = _fetch_public("m1", server)
    assert _component("m1").phase == "ready"
    with pytest.raises(ModelError):
        _fetch_public("m1", server, rev=REV2)
    assert _component("m1").phase == "ready"
    assert "upgrade failed" in (_component("m1").detail or "")
    assert models.path("m1") == p1
    assert (tmp_path / "models" / "m1" / "current").read_text() == REV


def test_readiness_failed_first_install_names_file(server):
    bad = _repo(REV, b"one")
    server.add_repo("org/model", bad)
    key = f"/org/model/resolve/{REV}/model.safetensors"
    server.files[key] = File(b"corrupted" * 100)
    with pytest.raises(ModelError, match="model.safetensors"):
        _fetch_public("m1", server)
    assert _component("m1").phase == "failed"
    assert "model.safetensors" in _component("m1").detail


# ---- kind collisions ---------------------------------------------------------


def test_name_collision_both_directions(server, tmp_path):
    body = os.urandom(1000)
    url = server.add("/f", File(body))
    models.model("single1", url=url, sha256=_sha(body), size=len(body))
    with pytest.raises(ValueError, match="one kind or the other"):
        models.model_snapshot("single1", repo="org/model", revision=REV)

    models.model_snapshot("snap1", repo="org/model", revision=REV)
    with pytest.raises(ValueError, match="one kind or the other"):
        models.model("snap1", url=url, sha256=_sha(body), size=len(body))
    with pytest.raises(ValueError, match="one kind or the other"):
        models.fetch("snap1", url=url, sha256=_sha(body), size=len(body))
    with pytest.raises(ValueError, match="one kind or the other"):
        _fetch_public("single1", server)


# ---- auth env ----------------------------------------------------------------


def test_hf_token_adopted_and_stripped_cross_origin(server, tmp_path, monkeypatch):
    monkeypatch.setenv("HF_TOKEN", "hf_secret_token")
    cdn = ModelServer()
    try:
        r = _repo(REV, b"one")
        server.add_repo("org/model", r)
        server.redirects[f"/org/model/resolve/{REV}/model.safetensors"] = (
            cdn.base + "/w"
        )
        cdn.add("/w", File(r.files["model.safetensors"]))
        _fetch_public("m1", server)
        tree = next(
            r
            for r in server.requests
            if r["path"].startswith("/api/models/org/model/tree/")
        )
        assert {k.lower(): v for k, v in tree["headers"].items()}.get(
            "authorization"
        ) == "Bearer hf_secret_token"
        assert cdn.last_headers("/w").get("authorization") is None
        assert "hf_secret_token" not in str([h["headers"] for h in cdn.requests])
    finally:
        cdn.close()


def test_explicit_authorization_beats_hf_token(server, monkeypatch):
    monkeypatch.setenv("HF_TOKEN", "hf_secret_token")
    server.add_repo("org/model", _repo(REV, b"one"))
    _fetch_public("m1", server, headers={"Authorization": "Bearer explicit"})
    tree = next(
        r
        for r in server.requests
        if r["path"].startswith("/api/models/org/model/tree/")
    )
    assert {k.lower(): v for k, v in tree["headers"].items()}.get(
        "authorization"
    ) == "Bearer explicit"


def test_hf_endpoint_env_used(server, monkeypatch):
    monkeypatch.setenv("HF_ENDPOINT", server.base)
    server.add_repo("org/model", _repo(REV, b"one"))
    models.fetch_snapshot("m1", repo="org/model", revision=REV)
    assert (
        sum(
            1
            for r in server.requests
            if r["path"].startswith("/api/models/org/model/tree/")
        )
        == 1
    )


# ---- Windows mmap interop ----------------------------------------------------


_MMAP_CHILD = """
import sys, time, mmap
f = open(sys.argv[1], "rb")
mm = mmap.mmap(f.fileno(), 0, access=mmap.ACCESS_READ)
print("mmapped", flush=True)
time.sleep(120)
"""


@pytest.mark.skipif(os.name != "nt", reason="Windows sharing-violation interop")
def test_mmap_subprocess_blocks_then_releases_gc(server, tmp_path):
    """A subprocess mmaping a shard in the old revision: the upgrade
    switches current, GC cannot remove the mmap'd tree (sharing
    violation = still in use, never an error), and after the child
    exits the next pass removes it."""
    _add_two_revisions(server)
    p1 = _fetch_public("m1", server)
    child = subprocess.Popen(
        [sys.executable, "-c", _MMAP_CHILD, str(p1 / "model.safetensors")],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    try:
        assert child.stdout.readline().strip() == "mmapped"
        _fetch_public("m1", server, rev=REV2)
        for _ in range(3):
            _snapshots.gc_name(tmp_path / "models" / "m1")
        # the mmap'd shard survives; GC never errored
        assert (p1 / "model.safetensors").is_file()
    finally:
        child.kill()
        child.wait(timeout=10)
    deadline = time.monotonic() + 10
    while p1.exists() and time.monotonic() < deadline:
        _snapshots.gc_name(tmp_path / "models" / "m1")
        if not p1.exists():
            break
        time.sleep(0.05)
    assert not p1.is_dir(), "GC never collected after the mmap holder exited"


# ---- e2e through a real runtime ----------------------------------------------


def _e2e_env(tmp_path: Path, server) -> dict:
    return {
        "PYLAIR_APP_DATA_DIR": str(tmp_path),
        "PYLAIR_MODELS_ALLOW_INSECURE": "1",
        "PYLAIR_TEST_SNAPSHOT_ENDPOINT": server.base,
        "PYLAIR_TEST_SNAPSHOT_REV": REV,
    }


def test_e2e_requires_gating_and_warm_worker(server, tmp_path):
    server.add_repo("org/model", _repo(REV, b"one"))
    rt = Runtime(module="testapp_snapshots", env=_e2e_env(tmp_path, server))
    try:
        rt.recv()  # hello
        rt.send(proto.request(1, "call", {"fn": "gated_list", "task": 1, "input": {}}))
        result = rt.recv_until(lambda m: m.get("id") == 1, timeout=30)
        assert sorted(result["result"]) == [
            "config.json",
            "model.safetensors",
            "tokenizer.json",
        ]
        rt.send(
            proto.request(
                2, "call", {"fn": "warm_path", "task": 2, "input": {"name": "m1"}}
            )
        )
        resolved = rt.recv_until(lambda m: m.get("id") == 2, timeout=15)
        assert (Path(resolved["result"]) / "config.json").read_bytes() == (
            b'{"tag": "one"}'
        )
        rt.send(
            proto.request(
                3, "call", {"fn": "warm_path", "task": 3, "input": {"name": "ghost"}}
            )
        )
        assert rt.recv_until(lambda m: m.get("id") == 3)["result"] == "not-ready"
        rt.send(
            proto.request(4, "call", {"fn": "warm_fetch_probe", "task": 4, "input": {}})
        )
        assert rt.recv_until(lambda m: m.get("id") == 4)["result"] == (
            "refused:ModelError"
        )
        # one tree query total — the worker added nothing
        assert (
            sum(
                1
                for r in server.requests
                if r["path"].startswith("/api/models/org/model/tree/")
            )
            == 1
        )
    finally:
        rt.kill()
        rt.proc.wait(timeout=10)
