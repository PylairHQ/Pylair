"""Bench fixture for the cold-vs-warm headline (0008 §8,
docs/performance.md): importing this module costs a fixed ~300 ms —
the stand-in for a model/torch import. Both functions have the same
trivial body; the only difference is the worker mode, so the measured
gap is spawn+import-per-call (process) versus pay-once-then-reuse
(warm).

bench_latency.rs drives a second runtime with `python -m pylair
testapp_benchcost`, so each member's import of THIS module carries the
300 ms — not the runtime's own import (that one is paid once, before
the timer starts).
"""

from __future__ import annotations

import time

from pylair import expose

time.sleep(0.3)


@expose(worker={"mode": "process"})
def bench_cost_cold() -> int:
    """Every call spawns a worker that pays the 300 ms import."""
    return 1


@expose(worker={"mode": "warm"})
def bench_cost_warm() -> int:
    """Only the first call pays; the member then serves every call."""
    return 1
