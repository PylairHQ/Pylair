"""Test app: a prewarmed warm worker whose import always fails.

The runtime imports this fine (no PYLAIR_WORKER_PARENT in its env); the
worker's own import explodes, so the prewarm handshake answers
{"ready": false} and the worker:<fn> component reports failed.
"""

from __future__ import annotations

import os

from pylair import expose

if os.environ.get("PYLAIR_WORKER_PARENT"):
    raise RuntimeError("import exploded inside the worker")


@expose(worker={"mode": "warm", "prewarm": True})
def warmfail_pid() -> int:
    """Never callable — the worker import itself fails."""
    return os.getpid()
