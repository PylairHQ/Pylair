"""pylair.models — the model download manager (decision 0009 §3-§7).

In-process tests drive a loopback Range-capable server (models_server):
first download, resume after a mid-body drop, Range-ignored restarts,
stale If-Range, hash-mismatch restart, disk-space preflight, https-only,
redirect secret-stripping, single-download concurrency, kernel lock
release on holder death, sidecar fast path, cancellation, the sweep
hint and declarative autostart. The e2e block at the bottom drives a
real `python -m pylair testapp_models` for the requires= race and the
warm-worker resolve-only contract. No external network, no fixed
sleeps — every wait polls state or releases a server-side gate.
"""

from __future__ import annotations

import asyncio
import hashlib
import json
import os
import subprocess
import sys
import threading
import time
from pathlib import Path

import pytest

from conftest import Runtime
from models_server import File, ModelServer
from pylair import _protocol as proto
from pylair import _readiness, models


@pytest.fixture(autouse=True)
def _state(monkeypatch, tmp_path):
    monkeypatch.setenv("PYLAIR_APP_DATA_DIR", str(tmp_path))
    monkeypatch.setenv("PYLAIR_MODELS_ALLOW_INSECURE", "1")
    models._reset()
    _readiness.reset()
    yield
    models._reset()
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


def _model_dir(tmp_path: Path) -> Path:
    return tmp_path / "models"


def _component(name: str):
    return _readiness._default._components[name]


# ---- download + verify -----------------------------------------------------


def test_first_download_verifies(server, tmp_path):
    body = os.urandom(200_000)
    url = server.add("/f", File(body))
    p = models.fetch("m1", url=url, sha256=_sha(body), size=len(body))
    assert p == _model_dir(tmp_path) / "m1"
    assert p.read_bytes() == body
    rec = json.loads((_model_dir(tmp_path) / "m1.json").read_text())
    assert rec["sha256"] == _sha(body)
    assert rec["size"] == len(body)
    assert rec["mtime_ns"] == p.stat().st_mtime_ns
    assert rec["etag"] == '"v1"'
    assert rec["url"] == url
    assert _component("m1").phase == "ready"
    assert server.hits("/f") == 1


def test_resume_after_midbody_drop(server, tmp_path):
    """A truncated transfer leaves .part; the retry re-hashes the
    prefix, sends Range + If-Range, appends at 206 and verifies."""
    body = os.urandom(400_000)
    server.add("/f", File(body, drop_after=100_000))
    url = server.base + "/f"
    p = models.fetch("m1", url=url, sha256=_sha(body), size=len(body))
    assert p.read_bytes() == body
    reqs = [r for r in server.requests if r["path"] == "/f"]
    assert len(reqs) == 2
    assert "Range" not in reqs[0]["headers"]
    assert reqs[1]["headers"]["Range"] == "bytes=100000-"
    assert reqs[1]["headers"]["If-Range"] == '"v1"'


def test_server_ignoring_range_restarts_clean(server, tmp_path):
    """Range -> 200 means the whole body arrives: .part is rewritten
    from scratch and the hash still verifies."""
    body = os.urandom(150_000)
    url = server.add("/f", File(body, ignore_range=True))
    part = _model_dir(tmp_path) / "m1.part"
    part.parent.mkdir(parents=True, exist_ok=True)
    part.write_bytes(b"stale-prefix" * 4_000)
    p = models.fetch("m1", url=url, sha256=_sha(body), size=len(body))
    assert p.read_bytes() == body


def test_if_range_stale_restarts(server, tmp_path):
    """A validator that no longer matches -> 200 -> fresh body."""
    body = os.urandom(150_000)
    url = server.add("/f", File(body, if_range_stale=True))
    part = _model_dir(tmp_path) / "m1.part"
    part.parent.mkdir(parents=True, exist_ok=True)
    part.write_bytes(body[:50_000])
    (_model_dir(tmp_path) / "m1.part.json").write_text(
        json.dumps({"etag": '"old-etag"'})
    )
    p = models.fetch("m1", url=url, sha256=_sha(body), size=len(body))
    assert p.read_bytes() == body
    assert server.last_headers("/f")["if-range"] == '"old-etag"'


def test_416_range_unsatisfiable_restarts(server, tmp_path):
    body = os.urandom(150_000)
    url = server.add("/f", File(body))
    part = _model_dir(tmp_path) / "m1.part"
    part.parent.mkdir(parents=True, exist_ok=True)
    part.write_bytes(b"x" * (len(body) + 50))
    p = models.fetch("m1", url=url, sha256=_sha(body), size=len(body))
    assert p.read_bytes() == body
    assert server.hits("/f") == 2


def test_hash_mismatch_one_restart_then_failed(server, tmp_path):
    """A bad hash wipes .part and restarts exactly once; the terminal
    detail carries expected and actual digests."""
    body = os.urandom(150_000)
    url = server.add("/f", File(body))
    with pytest.raises(models.ModelError, match="sha256 mismatch"):
        models.fetch("m1", url=url, sha256="0" * 64, size=len(body))
    assert server.hits("/f") == 2
    c = _component("m1")
    assert c.phase == "failed"
    assert "0" * 20 in c.detail and _sha(body)[:16] in c.detail
    d = _model_dir(tmp_path)
    assert not (d / "m1.part").exists() and not (d / "m1").exists()


def test_5xx_retries_then_serves(server, tmp_path):
    body = os.urandom(120_000)
    url = server.add("/f", File(body, status_queue=[500, 503]))
    p = models.fetch("m1", url=url, sha256=_sha(body), size=len(body))
    assert p.read_bytes() == body
    assert server.hits("/f") == 3


def test_4xx_not_retried(server, tmp_path):
    with pytest.raises(models.ModelError, match="network error"):
        models.fetch("m1", url=server.base + "/missing", sha256="0" * 64)
    assert server.hits("/missing") == 1


def test_disk_space_preflight(server, tmp_path, monkeypatch):
    body = os.urandom(100)
    url = server.add("/f", File(body))

    class _Usage:
        total, used, free = 10**9, 10**9 - 10, 10

    monkeypatch.setattr("shutil.disk_usage", lambda p: _Usage())
    with pytest.raises(models.ModelError, match="not enough disk space"):
        models.fetch("m1", url=url, sha256=_sha(body), size=len(body))
    assert server.hits("/f") == 0
    assert "not enough disk space" in (_component("m1").detail or "")


def test_https_only_by_default(server, monkeypatch):
    monkeypatch.delenv("PYLAIR_MODELS_ALLOW_INSECURE")
    with pytest.raises(models.ModelError, match="https"):
        models.fetch("m1", url=server.base + "/f", sha256="0" * 64)
    assert not server.requests


def test_env_unset_fails_component_clearly(server, monkeypatch):
    monkeypatch.delenv("PYLAIR_APP_DATA_DIR")
    with pytest.raises(models.ModelNotReady, match="PYLAIR_APP_DATA_DIR"):
        models.fetch("m1", url=server.base + "/f", sha256="0" * 64)
    assert not server.requests
    c = _component("m1")
    assert c.phase == "failed" and "PYLAIR_APP_DATA_DIR" in c.detail


# ---- redirects ---------------------------------------------------------------


def test_cross_origin_redirect_strips_sensitive(tmp_path, server):
    """urllib's default handler replays headers onto redirected requests
    across hosts — a gated-model 302 to a CDN would leak Authorization.
    Ours drops the sensitive set when scheme+host+port changes."""
    target = ModelServer()
    try:
        body = os.urandom(120_000)
        url = target.add("/model", File(body))
        server.redirects["/go"] = url
        p = models.fetch(
            "m1",
            url=server.base + "/go",
            sha256=_sha(body),
            size=len(body),
            headers={
                "Authorization": "Bearer TOPSECRET",
                "Cookie": "sess=1",
                "X-Api-Key": "k3y",
                "X-Keep": "yes",
            },
            sensitive_headers=["X-Api-Key"],
        )
        assert p.read_bytes() == body
        hdrs = target.last_headers("/model")
        assert "authorization" not in hdrs
        assert "cookie" not in hdrs
        assert "x-api-key" not in hdrs  # caller-declared sensitive name
        assert hdrs["x-keep"] == "yes"  # non-sensitive headers continue
    finally:
        target.close()


def test_same_origin_redirect_keeps_sensitive(tmp_path, server):
    body = os.urandom(120_000)
    url = server.add("/model", File(body))
    server.redirects["/go"] = url
    p = models.fetch(
        "m1",
        url=server.base + "/go",
        sha256=_sha(body),
        headers={"Authorization": "Bearer TOPSECRET"},
    )
    assert p.read_bytes() == body
    assert server.last_headers("/model")["authorization"] == "Bearer TOPSECRET"


def test_redirects_capped_at_five(server, tmp_path):
    """More than five hops fails instead of looping forever."""
    for i in range(1, 8):
        server.redirects[f"/r{i}"] = (
            server.base + f"/r{i + 1}" if i < 7 else server.base + "/f"
        )
    server.add("/f", File(b"never"))
    with pytest.raises(models.ModelError):
        models.fetch("m1", url=server.base + "/r1", sha256="0" * 64)
    assert server.hits("/f") == 0


def test_redirect_to_non_http_scheme_refused(server, tmp_path):
    """The scheme is re-validated on every hop — a 302 to file://,
    gopher://, etc. is refused even when insecure http is allowed."""
    server.redirects["/evil"] = "file:///etc/hostname"
    with pytest.raises(models.ModelError, match="not allowed"):
        models.fetch("m1", url=server.base + "/evil", sha256="0" * 64)


def test_headers_never_leak_into_failures(server, tmp_path):
    url = server.add("/f", File(b"x" * 100, status_queue=[500] * 10))
    with pytest.raises(models.ModelError) as e:
        models.fetch(
            "m1",
            url=url,
            sha256="0" * 64,
            headers={"Authorization": "Bearer TOPSECRET"},
        )
    assert "TOPSECRET" not in str(e.value)
    assert "TOPSECRET" not in (_component("m1").detail or "")


# ---- concurrency + locking ---------------------------------------------------


def test_concurrent_fetches_one_download_shared_progress(server, tmp_path, monkeypatch):
    """Two callers, one name: exactly one transfer; the second waits on
    the locks and reports the shared .part progress."""
    body = os.urandom(300_000)
    gate = threading.Event()
    url = server.add("/f", File(body, gate=gate, throttle=32_768))
    seen = []
    orig_loading = _readiness.loading
    monkeypatch.setattr(
        _readiness,
        "loading",
        lambda *a, **k: (seen.append(a[0:3]), orig_loading(*a, **k)),
    )
    results, errors = [], []

    def go():
        try:
            results.append(
                models.fetch("m1", url=url, sha256=_sha(body), size=len(body))
            )
        except Exception as e:  # noqa: BLE001 - collected, asserted below
            errors.append(e)

    t1 = threading.Thread(target=go)
    t1.start()
    _wait(lambda: server.hits("/f") == 1, what="first request")
    t2 = threading.Thread(target=go)
    t2.start()
    _wait(
        lambda: any("(shared)" in str(s[2]) for s in seen if len(s) > 2),
        what="shared progress report",
    )
    gate.set()
    t1.join(15)
    t2.join(15)
    assert not errors
    assert len(results) == 2
    assert results[0].read_bytes() == body
    assert server.hits("/f") == 1


def test_file_lock_released_on_holder_death(server, tmp_path):
    """The kernel lock is tied to the holder's open handle — killing the
    holder by pid frees it with no stale-lock cleanup."""
    d = _model_dir(tmp_path)
    d.mkdir(parents=True, exist_ok=True)
    lock = d / "m1.lock"
    lock.touch()
    holder = subprocess.Popen(
        [
            sys.executable,
            "-c",
            (
                "import os,sys,time\n"
                "fd = os.open(sys.argv[1], os.O_RDWR)\n"
                "if os.name == 'nt':\n"
                "    import msvcrt; os.lseek(fd,0,0)\n"
                "    msvcrt.locking(fd, msvcrt.LK_NBLCK, 1)\n"
                "else:\n"
                "    import fcntl; fcntl.flock(fd, fcntl.LOCK_EX|fcntl.LOCK_NB)\n"
                "print('locked', flush=True)\n"
                "time.sleep(60)\n"
            ),
            str(lock),
        ],
        stdout=subprocess.PIPE,
        text=True,
    )
    try:
        assert holder.stdout.readline().strip() == "locked"
        fd = models._try_file_lock(lock)
        assert fd is None, "a live holder must contend the lock"
        holder.kill()
        holder.wait(timeout=10)

        def freed() -> bool:
            fd = models._try_file_lock(lock)
            if fd is None:
                return False
            models._release_file_lock(fd)
            return True

        _wait(freed, what="lock released after holder death")
    finally:
        if holder.poll() is None:
            holder.kill()
            holder.wait(timeout=10)


def test_sidecar_fast_path_no_rehash(server, tmp_path, monkeypatch):
    """File + matching sidecar -> ready with one stat, no hashing —
    the multi-GB launch path must not re-hash on every start. A stale
    mtime (copy, backup restore) is NOT path()'s job: it refuses with
    zero file reads, and fetch/autostart do the one verify+repair."""
    body = os.urandom(150_000)
    url = server.add("/f", File(body))
    p = models.fetch("m1", url=url, sha256=_sha(body), size=len(body))
    calls = []
    orig = models._hash_file
    spy = lambda f: (calls.append(f), orig(f))[1]
    monkeypatch.setattr(models, "_hash_file", spy)
    assert models.path("m1") == p
    assert calls == []
    # Stale mtime: path() is strict-check only — ModelNotReady without
    # a single byte read (multi-GB hashing would sit inside an
    # arbitrary call, e.g. the first call into a warm worker).
    st = p.stat()
    os.utime(p, ns=(st.st_atime_ns, st.st_mtime_ns + 1_000_000))
    monkeypatch.setattr(
        models,
        "_hash_file",
        lambda f: pytest.fail(f"path() must not read {f}"),
    )
    with pytest.raises(models.ModelNotReady):
        models.path("m1")
    # The next fetch (also what the launch autostart runs) does exactly
    # one verify hash, repairs the sidecar, and does not re-download.
    monkeypatch.setattr(models, "_hash_file", spy)
    assert models.fetch("m1", url=url, sha256=_sha(body), size=len(body)) == p
    assert calls == [p]
    assert server.hits("/f") == 1
    monkeypatch.setattr(
        models,
        "_hash_file",
        lambda f: pytest.fail("verified file must not be re-read"),
    )
    assert models.path("m1") == p
    assert p.read_bytes() == body


def test_path_raises_for_unverified(server, tmp_path):
    with pytest.raises(models.ModelNotReady):
        models.path("m1")
    body = b"content"
    url = server.add("/f", File(body))
    p = models.fetch("m1", url=url, sha256=_sha(body))
    assert models.path("m1") == p
    assert models.dir("m1") == _model_dir(tmp_path)


# ---- cancellation -----------------------------------------------------------


def test_cancel_leaves_resumable_part(server, tmp_path):
    body = os.urandom(400_000)
    # drop_after severs the stream once mid-body: the client banks the
    # partial bytes, reports a retryable drop, and parks in the retry
    # backoff — where the abort event is checked. No timing gates: the
    # .part size itself is the sync point.
    url = server.add("/f", File(body, drop_after=100_000))
    abort = threading.Event()
    done = threading.Event()
    part = _model_dir(tmp_path) / "m1.part"

    def go():
        try:
            models.fetch("m1", url=url, sha256=_sha(body), size=len(body), abort=abort)
        except models.ModelCancelled:
            done.set()

    t = threading.Thread(target=go)
    t.start()
    _wait(lambda: part.exists() and part.stat().st_size > 0, what=".part bytes")
    abort.set()
    assert done.wait(15), "cancelled fetch did not return"
    t.join(15)
    partial = part.stat().st_size
    assert 0 < partial < len(body)
    p = models.fetch("m1", url=url, sha256=_sha(body), size=len(body))
    assert p.read_bytes() == body
    assert server.last_headers("/f")["range"] == f"bytes={partial}-"


# ---- names, workers, sweep hint --------------------------------------------


@pytest.mark.parametrize(
    "bad",
    [
        "../x",
        "a/b",
        "a\\b",
        "..",
        "x.lock",
        "x.part",
        "x.json",
        "",
        ".h",
        "a" * 65,
        "a..b",
    ],
)
def test_invalid_names_rejected(bad):
    with pytest.raises(ValueError, match="invalid model name"):
        models._check_name(bad)


def test_valid_names_accepted():
    for ok in ["m1", "llm-7b-q4", "model.v2_final", "a" * 64]:
        assert models._check_name(ok) == ok


def test_worker_fetch_refused(server, monkeypatch):
    """Workers inherit the data dir but only resolve — fetch/model
    downloads belong to the main runtime."""
    monkeypatch.setenv("PYLAIR_WORKER_PARENT", "1234")
    with pytest.raises(models.ModelError, match="main runtime"):
        models.fetch("m1", url=server.base + "/f", sha256="0" * 64)
    models.model("m1", url=server.base + "/f", sha256="0" * 64)
    assert not server.requests  # registered, never started


def test_sweep_hint_for_late_model(tmp_path, server):
    """requires=(name,) declared unmanaged -> sweep fails it; once the
    model registers (late, e.g. inside a hook) the composed detail
    names the remedy instead of the generic wording."""
    _readiness.declare("m1")  # what requires=("m1",) does at decoration
    _readiness._default.sweep()
    models.model("m1", url=server.base + "/f", sha256="0" * 64)
    c = _component("m1")
    assert c.phase == "failed"
    detail = _readiness._emit_detail("m1", c)
    assert "models.model" in detail and "module import" in detail
    # An unregistered name still gets the generic wording.
    _readiness.declare("ghost")
    _readiness._default.sweep()
    assert _readiness._emit_detail("ghost", _component("ghost")) == (
        "never reported by a startup hook"
    )


# ---- declarative autostart ---------------------------------------------------


def test_model_at_import_waits_for_startup(server, tmp_path):
    """models.model() registers + declares managed but does not download
    until the post-hello startup pass — a codegen scan importing the
    module must not fetch gigabytes."""
    body = os.urandom(150_000)
    url = server.add("/f", File(body))
    models.model("m1", url=url, sha256=_sha(body), size=len(body))
    assert server.hits("/f") == 0
    assert _component("m1").managed
    asyncio.run(_readiness.run_startup())
    # The component phase is the completion signal — the file lands at
    # os.replace, slightly before sidecar+ready report.
    _wait(lambda: _component("m1").phase == "ready", what="model ready")
    assert (_model_dir(tmp_path) / "m1").read_bytes() == body
    assert server.hits("/f") == 1


def test_model_registered_during_startup_starts(server, tmp_path):
    """A models.model() inside an on_startup hook can't rely on the
    autostart hook (the hook list was already snapshotted) — it starts
    directly because startup has begun."""
    body = os.urandom(150_000)
    url = server.add("/f", File(body))

    @_readiness.on_startup
    def late():
        models.model("m1", url=url, sha256=_sha(body), size=len(body))

    asyncio.run(_readiness.run_startup())
    _wait(lambda: (_model_dir(tmp_path) / "m1").exists(), what="late download")
    assert (_model_dir(tmp_path) / "m1").read_bytes() == body


# ---- e2e: real runtime -------------------------------------------------------


def _models_env(tmp_path: Path, url: str, body: bytes) -> dict:
    return {
        "PYLAIR_APP_DATA_DIR": str(tmp_path),
        "PYLAIR_MODELS_ALLOW_INSECURE": "1",
        "PYLAIR_TEST_MODEL_URL": url,
        "PYLAIR_TEST_MODEL_SHA": _sha(body),
        "PYLAIR_TEST_MODEL_SIZE": str(len(body)),
    }


def test_declarative_model_beats_first_call_race(server, tmp_path):
    """models.model() at import + a requires= call arriving before the
    download starts: the managed gate waits, the auto-start completes
    it, the call resolves."""
    body = os.urandom(250_000)
    url = server.add("/f", File(body))
    rt = Runtime(module="testapp_models", env=_models_env(tmp_path, url, body))
    try:
        rt.recv()  # hello
        rt.send(proto.request(1, "call", {"fn": "gated_path", "task": 1, "input": {}}))
        result = rt.recv_until(lambda m: m.get("id") == 1, timeout=30)
        assert Path(result["result"]).read_bytes() == body
        assert server.hits("/f") == 1
    finally:
        rt.kill()
        rt.proc.wait(timeout=10)


def test_warm_worker_path_never_downloads(server, tmp_path):
    """models.path() in a warm worker resolves the verified file and
    raises ModelNotReady on an unknown name — zero extra requests."""
    body = os.urandom(250_000)
    url = server.add("/f", File(body))
    rt = Runtime(module="testapp_models", env=_models_env(tmp_path, url, body))
    try:
        rt.recv()  # hello
        rt.send(proto.request(1, "call", {"fn": "gated_path", "task": 1, "input": {}}))
        rt.recv_until(lambda m: m.get("id") == 1, timeout=30)  # m1 ready
        rt.send(
            proto.request(
                2,
                "call",
                {"fn": "warm_path", "task": 2, "input": {"name": "m1"}},
            )
        )
        resolved = rt.recv_until(lambda m: m.get("id") == 2)
        assert Path(resolved["result"]).read_bytes() == body
        rt.send(
            proto.request(
                3,
                "call",
                {"fn": "warm_path", "task": 3, "input": {"name": "ghost"}},
            )
        )
        assert rt.recv_until(lambda m: m.get("id") == 3)["result"] == "not-ready"
        rt.send(
            proto.request(4, "call", {"fn": "warm_fetch_probe", "task": 4, "input": {}})
        )
        assert rt.recv_until(lambda m: m.get("id") == 4)["result"] == (
            "refused:ModelError"
        )
        assert server.hits("/f") == 1  # the worker never touched the network
    finally:
        rt.kill()
        rt.proc.wait(timeout=10)
