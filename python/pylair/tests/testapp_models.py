"""Model-manager e2e app (decision 0009, PR 2).

`models.model("m1", ...)` runs at module import — declarative: it
declares the readiness component managed and auto-starts the download
after hello. The gated fn proves the first-call race is solved: a
`requires=("m1",)` call that arrives before the download started waits
on the managed component and succeeds once it verifies. Warm-worker
probes prove workers only resolve (path) — never fetch.
"""

from __future__ import annotations

import os

from pylair import expose, models

_URL = os.environ.get("PYLAIR_TEST_MODEL_URL", "")
_SHA = os.environ.get("PYLAIR_TEST_MODEL_SHA", "0" * 64)
_SIZE = int(os.environ.get("PYLAIR_TEST_MODEL_SIZE", "0")) or None

if _URL:
    models.model("m1", url=_URL, sha256=_SHA, size=_SIZE)


@expose(requires=("m1",))
def gated_path() -> str:
    return str(models.path("m1"))


@expose(worker={"mode": "warm"})
def warm_path(name: str) -> str:
    """Workers resolve-and-check only — no lock, no download."""
    try:
        return str(models.path(name))
    except models.ModelNotReady:
        return "not-ready"


@expose(worker={"mode": "warm"})
def warm_fetch_probe() -> str:
    """fetch() in a worker is refused: the main runtime owns downloads."""
    try:
        models.fetch("late", url="http://127.0.0.1:1/x", sha256="0" * 64)
    except models.ModelError as e:
        return f"refused:{type(e).__name__}"
    return "unexpected-success"
