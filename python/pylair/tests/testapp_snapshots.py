"""Snapshot e2e app (decision 0013, PR 2).

`models.model_snapshot("m1", ...)` runs at module import — declarative:
it declares the readiness component managed and auto-starts the fetch
after hello. The gated fn proves the first-call race is solved for
snapshots: a `requires=("m1",)` call that arrives before the fetch
started waits on the managed component and succeeds once current is
verified. Warm-worker probes prove workers only resolve — never fetch.
"""

from __future__ import annotations

import os

from pylair import expose, models

_ENDPOINT = os.environ.get("PYLAIR_TEST_SNAPSHOT_ENDPOINT", "")
_REV = os.environ.get("PYLAIR_TEST_SNAPSHOT_REV", "a" * 40)

if _ENDPOINT:
    models.model_snapshot("m1", repo="org/model", revision=_REV, endpoint=_ENDPOINT)


@expose(requires=("m1",))
def gated_list() -> list[str]:
    p = models.path("m1")
    return sorted(f.name for f in p.iterdir() if not f.name.startswith("."))


@expose(worker={"mode": "warm"})
def warm_path(name: str) -> str:
    """Workers resolve-and-check only — no download."""
    try:
        return str(models.path(name))
    except models.ModelNotReady:
        return "not-ready"


@expose(worker={"mode": "warm"})
def warm_fetch_probe() -> str:
    """fetch_snapshot() in a worker is refused: the main runtime owns downloads."""
    try:
        models.fetch_snapshot("late", repo="org/model", revision="a" * 40)
    except models.ModelError as e:
        return f"refused:{type(e).__name__}"
    return "unexpected-success"
