"""Readiness test app: gated startup hooks and requires= functions.

Both hooks park on gates the test opens through release_hooks() —
deterministic: no sleeps, every wait has a wire-reachable release.
"""

import asyncio
import os
import threading

from pylair import expose, loading, on_startup, ready

_async_gate: asyncio.Event | None = None
_sync_gate = threading.Event()
_records: dict[str, object] = {}


@on_startup
async def warm_model():
    """Async hook: reports loading with detail, parks, then ready."""
    global _async_gate
    _async_gate = asyncio.Event()
    _records["async_thread"] = threading.current_thread().name
    loading("model", 0.1, detail="checkpoint 1/10")
    await _async_gate.wait()
    loading("model", 0.9)
    ready("model")


@on_startup
def warm_index():
    """Sync hook: runs on the default executor — records its thread."""
    _records["sync_thread"] = threading.current_thread().name
    loading("index", detail="warming index")
    _sync_gate.wait(timeout=30)
    ready("index")


@expose
def add(a: int, b: int) -> int:
    return a + b


@expose
def release_hooks() -> str:
    """Open both gates — the wire-reachable release for parked hooks."""
    if _async_gate is not None:
        _async_gate.set()
    _sync_gate.set()
    return "released"


@expose
def hook_records() -> dict:
    return dict(_records)


@expose(requires=("model", "index"))
def chat(prompt: str) -> str:
    return f"answered:{prompt}"


@expose(requires="model")
def model_only() -> str:
    return "ok"


@expose(isolated=True, requires=("model",))
def isolated_ready_work(pidfile: str) -> str:
    """Isolated + gated: the worker must not spawn until `model` is
    ready — the pidfile is the spawn witness (0007 §2)."""
    tmp = pidfile + ".tmp"
    with open(tmp, "w") as f:
        f.write(str(os.getpid()))
    os.replace(tmp, pidfile)
    return "worker-ran"
