"""Reference app backend — a local-AI workflow over the Pylair bridge.

Covers the four M6 acceptance criteria: token-by-token streaming from a
real model (Ollama), safe cancellation, deliberate crash + supervised
recovery, and packaged installs on machines with no Python.

`add` and `count` stay exposed: the webview self-test
(PYLAIR_SELF_TEST_WEBVIEW, run by the acceptance job) drives them.
"""

import asyncio
import os
import sys
import time
from collections.abc import AsyncIterator
from dataclasses import dataclass

from pylair import expose
from pylair_ollama import Message, chat as ollama_chat, status as ollama_status

DEFAULT_MODEL = os.environ.get("REFERENCE_MODEL", "qwen2.5:0.5b")


@expose
async def status() -> dict:
    """Ollama reachability + model list — drives the UI status badge.
    Never raises: the app must still work when Ollama is absent (clean
    install machines have no Ollama either)."""
    s = await ollama_status()
    return {
        "installed": s.installed,
        "running": s.running,
        "version": s.version,
        "models": s.models,
        "default_model": DEFAULT_MODEL,
        "error": s.error,
    }


@expose
async def chat(prompt: str, model: str | None = None) -> AsyncIterator[str]:
    """Token-by-token stream from a local Ollama model. Cancelling the
    consuming task aborts the HTTP request mid-stream (pylair_ollama owns
    the httpx lifecycle), so a cancelled chat releases its connection."""
    async for delta in ollama_chat(
        model or DEFAULT_MODEL, [Message(role="user", content=prompt)]
    ):
        yield delta


@expose
async def demo_stream(tokens: int = 80, delay_ms: int = 60) -> AsyncIterator[str]:
    """Deterministic token-ish stream when no Ollama is present — same
    streaming and cancellation path as `chat`, exercisable on clean CI
    machines that have no model server."""
    for i in range(tokens):
        yield f"token-{i} "
        await asyncio.sleep(delay_ms / 1000)


@expose
def crash() -> None:
    """Kill the runtime process on purpose. The supervisor must detect the
    exit, restart the runtime, and the UI watches the state transitions."""
    os._exit(3)


# ---- exercised by the packaged self-tests (acceptance job) ----
# `hello` is the plugin's headless/GUI self-test call; `add`/`count` are
# driven through the webview by PYLAIR_SELF_TEST_WEBVIEW.


@expose
def hello(name: str = "world") -> str:
    return f"hello, {name} — from python"


@expose
async def add(a: int, b: int) -> int:
    return a + b


@expose
async def count(n: int = 5) -> AsyncIterator[int]:
    for i in range(n):
        yield i
        await asyncio.sleep(0.2)


@expose
def log_stderr(msg: str) -> None:
    # The two-window self-test writes a marker line and asserts it reaches
    # only the owner window's pylair://stderr subscription.
    print(msg, file=sys.stderr)


@expose
def fn_permissions_fixture() -> None:
    # Presence tells the two-window self-test that this build enforces
    # per-function permissions, so selftest-winb.json's deny entry for win-b
    # must be honoured (CI appends the same function to scaffolded apps).
    return None


# Per-window fairness self-test (docs/decisions/0005): `hold` occupies a
# task slot until cancelled (or the timeout passes); `holds_active` is the
# rendezvous win-b polls to learn that main is actually holding before it
# probes its own share. Presence arms the self-test's window-fairness check
# (CI appends the same functions to scaffolded apps).
_holds_active = 0


@expose
async def hold(seconds: float = 30.0) -> str:
    global _holds_active
    _holds_active += 1
    try:
        await asyncio.sleep(seconds)
        return "held"
    finally:
        _holds_active -= 1


@expose
def holds_active() -> int:
    return _holds_active


# `b_ping` is win-b's liveness mark: the call itself is the bridge command
# that subscribes win-b to the fairness live set, so `b_seen` >= 1 on the
# main window is deterministic proof win-b holds a live entry before the
# flood — no event, no timing (CI appends the same functions).
_b_seen = 0


@expose
def b_ping() -> int:
    global _b_seen
    _b_seen += 1
    return _b_seen


@expose
def b_seen() -> int:
    return _b_seen


# App-readiness self-test (decisions 0007): the startup hook reports the
# "selftest" component loading then ready, and `selftest_ping` is gated
# on it via requires=. The ping/done pair re-reports the cycle as two
# REAL transitions — a single loading+ready pair inside one call would
# coalesce to a no-op on the wire, so a webview that subscribed to
# pylair://app after startup observes a phase event deterministically
# (CI appends the same functions to the scaffolded app).
from pylair import loading, on_startup, ready


@on_startup
def _selftest_component() -> None:
    loading("selftest", 0.5)
    ready("selftest")


@expose(requires=("selftest",))
def selftest_ping() -> str:
    loading("selftest", 0.9)
    return "pong"


@expose
def selftest_done() -> str:
    ready("selftest")
    return "done"


# Latency bench (docs/performance.md): PYLAIR_SELF_TEST_BENCH drives these
# through the installed webview; the same payloads go through Handle::call
# in crates/tauri-plugin-pylair/tests/bench_latency.rs and through
# coerce_kwargs/dumps in python/pylair/tests/bench_dispatch.py, so each
# layer's share of the total can be subtracted out.
@dataclass
class BenchPoint:
    x: float
    y: float
    label: str


@dataclass
class BenchShape:
    name: str
    origin: BenchPoint
    points: list[BenchPoint]
    meta: dict[str, int]


@expose
def bench_noop() -> int:
    return 1


@expose
def bench_echo(s: str) -> str:
    return s


@expose
def bench_nested(shape: BenchShape) -> int:
    return len(shape.points) + shape.meta["n"]


# Warm-worker selftest witness (docs/decisions/0008 §8, docs/cancellation.md):
# the webview "warm-workers" check drives this through the real bridge —
# two calls must share the member, and a cancelled sleep diverges per
# platform (POSIX interrupt survives with the same pid; Windows kill-only
# respawns with a new pid). The report carries both, so the check asserts
# per platform without a second fixture.
@expose(worker={"mode": "warm"})
def warm_selftest(seconds: float = 0.0) -> dict:
    """Sleep `seconds`, then report the serving member's pid + platform."""
    time.sleep(seconds)
    return {"pid": os.getpid(), "platform": sys.platform}


# Model-manager self-test (docs/decisions/0009): a loopback-only HTTP
# server inside the runtime serves a known blob; `models_selftest_fetch`
# downloads + verifies it and asserts the second fetch took the sidecar
# fast path (the server counts hits — it must stay 1). The warm-worker
# fixture resolves the verified file read-only — it can never fetch —
# and cleanup removes the model from the app data dir afterwards.
# http:// is loopback-only here; the opt-in is scoped to this app.
import hashlib as _m_hashlib
import threading as _m_threading
from http.server import BaseHTTPRequestHandler as _m_BHRH
from http.server import ThreadingHTTPServer as _m_THS
from pathlib import Path as _m_Path

_MODEL_BLOB = _m_hashlib.sha256(b"pylair-selftest-model").digest() * 8192
_MODEL_SHA = _m_hashlib.sha256(_MODEL_BLOB).hexdigest()
_model_state: dict = {"server": None, "hits": 0, "url": None, "snap_hits": {}}
_SNAP_FILES = {
    "config.json": b'{"selftest": true}',
    "weights.bin": _m_hashlib.sha256(b"pylair-selftest-snapshot").digest() * 64,
}


def _snap_manifest() -> list:
    return [
        {"path": n, "sha256": _m_hashlib.sha256(b).hexdigest(), "size": len(b)}
        for n, b in _SNAP_FILES.items()
    ]


class _ModelHandler(_m_BHRH):
    def log_message(self, *args):
        pass

    def do_GET(self):
        if self.path.startswith("/snap/"):
            name = self.path[len("/snap/"):]
            blob = _SNAP_FILES.get(name)
            if blob is None:
                self.send_response(404)
                self.send_header("Content-Length", "0")
                self.end_headers()
                return
            _model_state["snap_hits"][name] = _model_state["snap_hits"].get(name, 0) + 1
            self.send_response(200)
            self.send_header("Content-Length", str(len(blob)))
            self.end_headers()
            self.connection.sendall(blob)
            return
        blob = _MODEL_BLOB
        _model_state["hits"] += 1
        start = 0
        status = 200
        rng = self.headers.get("Range")
        if rng and rng.startswith("bytes="):
            try:
                start = int(rng[6:].split("-", 1)[0])
            except ValueError:
                start = 0
            if 0 < start < len(blob):
                status = 206
            else:
                start = 0
        body = blob[start:]
        self.send_response(status)
        self.send_header("Content-Length", str(len(body)))
        if status == 206:
            self.send_header("Content-Range", f"bytes {start}-{len(blob) - 1}/{len(blob)}")
        self.end_headers()
        # wfile.write is a single send() and can return short — sendall.
        self.connection.sendall(body)


def _models_server_url() -> str:
    if _model_state["server"] is None:
        srv = _m_THS(("127.0.0.1", 0), _ModelHandler)
        _m_threading.Thread(target=srv.serve_forever, daemon=True).start()
        _model_state["server"] = srv
        _model_state["url"] = f"http://127.0.0.1:{srv.server_port}/m"
    return _model_state["url"]


@expose
def models_selftest_fetch() -> dict:
    # Loopback http is allowed only around the two fetch calls: the
    # previous value is saved, then restored in finally (deleted when
    # it was unset), so plain-http stays refused for the rest of the
    # process.
    prev = os.environ.get("PYLAIR_MODELS_ALLOW_INSECURE")
    os.environ["PYLAIR_MODELS_ALLOW_INSECURE"] = "1"
    try:
        from pylair import models

        url = _models_server_url()
        kw = {"url": url, "sha256": _MODEL_SHA, "size": len(_MODEL_BLOB)}
        models.fetch("selftest-model", **kw)
        models.fetch("selftest-model", **kw)
    finally:
        if prev is None:
            os.environ.pop("PYLAIR_MODELS_ALLOW_INSECURE", None)
        else:
            os.environ["PYLAIR_MODELS_ALLOW_INSECURE"] = prev
    if _model_state["hits"] != 1:
        raise RuntimeError(
            f"second fetch was not the sidecar fast path (hits={_model_state['hits']})"
        )
    root = models.dir("selftest-model")
    app_data = os.environ.get("PYLAIR_APP_DATA_DIR", "")
    if not app_data or root != _m_Path(app_data) / "models":
        raise RuntimeError(f"model root {root} is not <app-data>/models ({app_data!r})")
    p = models.path("selftest-model")
    if p.parent != root:
        raise RuntimeError(f"models.path {p} is not under {root}")
    return {
        "path": str(p),
        "root": str(root),
        "hits": _model_state["hits"],
        "insecure_after": os.environ.get("PYLAIR_MODELS_ALLOW_INSECURE"),
    }


@expose
def snapshot_selftest_fetch() -> dict:
    # The non-HF manifest form over the same loopback server: two tiny
    # files, fetched twice — the second fetch must be the fast path
    # (zero extra hits per file).
    prev = os.environ.get("PYLAIR_MODELS_ALLOW_INSECURE")
    os.environ["PYLAIR_MODELS_ALLOW_INSECURE"] = "1"
    try:
        from pylair import models

        base = _models_server_url().rsplit("/m", 1)[0] + "/snap/"
        kw = {"base_url": base, "manifest": _snap_manifest()}
        models.fetch_snapshot("selftest-snapshot", **kw)
        models.fetch_snapshot("selftest-snapshot", **kw)
    finally:
        if prev is None:
            os.environ.pop("PYLAIR_MODELS_ALLOW_INSECURE", None)
        else:
            os.environ["PYLAIR_MODELS_ALLOW_INSECURE"] = prev
    hits = dict(_model_state["snap_hits"])
    if any(h != 1 for h in hits.values()) or len(hits) != len(_SNAP_FILES):
        raise RuntimeError(f"second snapshot fetch was not the fast path (hits={hits})")
    p = models.path("selftest-snapshot")
    if not (p / "config.json").is_file() or not (p / "weights.bin").is_file():
        raise RuntimeError(f"snapshot at {p} is incomplete")
    if (p / "weights.bin").read_bytes() != _SNAP_FILES["weights.bin"]:
        raise RuntimeError("snapshot weights.bin content mismatch")
    return {"path": str(p), "hits": hits}


@expose(worker={"mode": "warm"})
def snapshot_warm_path() -> dict:
    from pylair import models

    return {"path": str(models.path("selftest-snapshot")), "pid": os.getpid()}


@expose
def snapshot_selftest_cleanup() -> dict:
    import shutil as _shutil

    from pylair import models

    root = models.dir("selftest-snapshot") / "selftest-snapshot"
    if root.is_dir():
        _shutil.rmtree(root, ignore_errors=True)
    srv = _model_state.get("server")
    if srv is not None:
        srv.shutdown()
        _model_state["server"] = None
    return {"removed": True}


@expose(worker={"mode": "warm"})
def models_warm_path() -> dict:
    # Resolve-only inside a warm worker — it can never download, so a
    # returned path proves the verified file is shared across processes.
    from pylair import models

    return {"path": str(models.path("selftest-model")), "pid": os.getpid()}


@expose
def models_selftest_cleanup() -> dict:
    from pylair import models

    root = models.dir("selftest-model")
    if root.is_dir():
        for f in root.glob("selftest-model*"):
            f.unlink(missing_ok=True)
    srv = _model_state.get("server")
    if srv is not None:
        srv.shutdown()
        _model_state["server"] = None
    return {"removed": True}


@expose
def make_blob() -> "pylair.Blob":
    """Blob fixture (decision 0010): a tiny real PNG the acceptance PR
    fetches through the pylair-blob scheme — the bridge carries the
    $blob tag, the client resolves it to a URL. The payload must be a
    decodable image: the self-test's <img> step fires error otherwise."""
    import pylair

    return pylair.Blob(bytes(_SELFTEST_PNG), mime="image/png")


# The deterministic 1x1 RGBA PNG make_blob returns — 70 bytes, the same
# literal the client check and the CI heredocs carry.
_SELFTEST_PNG = (
    137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82,
    0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0, 31, 21, 196, 137, 0,
    0, 0, 13, 73, 68, 65, 84, 120, 156, 99, 96, 96, 96, 96, 0, 0,
    0, 5, 0, 1, 165, 246, 69, 64, 0, 0, 0, 0, 73, 69, 78, 68, 174,
    66, 96, 130,
)


# Blob self-test relay (decision 0010 §7): the wire id is minted at
# serialization time, so neither Python side can see it — the owner
# stores the id its BlobHandle decoded (selftest_blob_store) and win-b
# reads it back (selftest_blob_id), fetches that URL, and records the
# refusal through selftest_blob_outcome for the owner to poll
# (selftest_blob_verdict). Same Python-state rendezvous pattern as
# b_seen/holds_active: it keeps the win-b report's timing independent of
# the owner check's progress (CI appends the same functions to
# scaffolded apps).
_selftest_blob: dict = {"id": "", "verdict": ""}


@expose
def selftest_blob_store(id: str) -> None:
    _selftest_blob["id"] = id


@expose
def selftest_blob_id() -> str:
    return _selftest_blob["id"]


@expose
def selftest_blob_outcome(v: str) -> None:
    _selftest_blob["verdict"] = v


@expose
def selftest_blob_verdict() -> str:
    return _selftest_blob["verdict"]
