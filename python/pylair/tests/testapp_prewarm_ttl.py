"""Test app: a prewarmed warm worker WITH idle_ttl_s (0008 §4).

Isolated in its own module so the extra `worker:<fn>` component frames
can't perturb testapp_prewarm's tests — every frame in this app's
channel is from the ttl pool.

PYLAIR_PREWARM_TTL_PIDFILE: the member writes its pid there at import
(the runtime's own import skips it — PYLAIR_WORKER_PARENT is only set
for worker children), proving which process the prewarmed member is.
"""

from __future__ import annotations

import os
import sys

from pylair import expose

if os.environ.get("PYLAIR_WORKER_PARENT"):
    _tp = os.environ.get("PYLAIR_PREWARM_TTL_PIDFILE")
    if _tp and sys.argv[-1] == "prewarm_ttl_pid":  # --serve <module> <fn>
        with open(_tp, "w") as f:
            f.write(str(os.getpid()))


@expose(worker={"mode": "warm", "prewarm": True, "idle_ttl_s": 0.3})
def prewarm_ttl_pid() -> int:
    """Prewarmed AND idle-reaped (0008 §4): the member dies ~0.3 s after
    its handshake, worker:<fn> stays ready, the next call cold-starts."""
    return os.getpid()
