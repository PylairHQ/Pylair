"""Test app: prewarmed warm workers (decision 0008 §6).

`prewarm_pid` spawns its first member at startup and reports the
`worker:prewarm_pid` readiness component. `lazy_pid` is the lazy-spawn
control. `after_worker` gates on the component via requires=.

Env-controlled worker-import witnesses (the runtime's own import
skips them — PYLAIR_WORKER_PARENT is only set for worker children).
Each `--serve` worker knows its fn name (sys.argv), so the witnesses
key on which member imported:

- PYLAIR_PREWARM_PIDFILE: the prewarm_pid member writes its pid there
  at import, proving which process the prewarmed member is.
- PYLAIR_LAZY_PIDFILE: a lazy_pid member writes its pid there at
  import — its absence proves no lazy member ever spawned.
- PYLAIR_PREWARM_GATE: the worker's import blocks until the file
  exists — stretches the loading window so requires= waits and
  dispatch-not-blocked are observable.
"""

from __future__ import annotations

import os
import sys
import time

from pylair import expose

if os.environ.get("PYLAIR_WORKER_PARENT"):
    _serves = sys.argv[-1]  # --serve <module> <fn>
    _pf = os.environ.get("PYLAIR_PREWARM_PIDFILE")
    if _pf and _serves == "prewarm_pid":
        with open(_pf, "w") as f:
            f.write(str(os.getpid()))
    _lz = os.environ.get("PYLAIR_LAZY_PIDFILE")
    if _lz and _serves == "lazy_pid":
        with open(_lz, "w") as f:
            f.write(str(os.getpid()))
    _gate = os.environ.get("PYLAIR_PREWARM_GATE")
    if _gate:
        _deadline = time.time() + 60
        while not os.path.exists(_gate):
            assert time.time() < _deadline, "prewarm gate never released"
            time.sleep(0.05)


@expose(worker={"mode": "warm", "prewarm": True})
def prewarm_pid() -> int:
    """Served by the member spawned at startup."""
    return os.getpid()


@expose(worker={"mode": "warm"})
def lazy_pid() -> int:
    """Warm but not prewarmed — no member should exist until a call."""
    return os.getpid()


@expose(requires=("worker:prewarm_pid",))
def after_worker() -> str:
    """Gates on the warm pool's readiness — answered only after the
    prewarmed member completed its import+resolve handshake."""
    return "answered"


@expose
def add(a: int, b: int) -> int:
    """The dispatch-liveness probe — answers while another call waits."""
    return a + b
