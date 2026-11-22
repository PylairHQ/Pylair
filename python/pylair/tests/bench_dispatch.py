"""Python-dispatch latency bench — NOT a pytest module (bench_ prefix).

Run from this directory:

    uv run python tests/bench_dispatch.py

Sizes the Python-side halves of the bridge latency the webview and Rust
legs bracket (docs/performance.md): ``coerce_kwargs`` runs
``inspect.signature`` + ``typing.get_type_hints`` on EVERY call, and
``dumps`` emits the JSON text then re-parses it whole for the
JS-safe-integer guard. Payloads are identical to the other two legs so
the shares can be compared. Emits one ``py-bench:`` JSON line per case.

N and warmup match the end-to-end legs (override via env).
"""

from __future__ import annotations

import json
import os
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
sys.path.insert(0, str(Path(__file__).parent.parent / "src"))

import testapp
from pylair._json import coerce_kwargs, dumps

WARMUP = int(os.environ.get("PYLAIR_BENCH_WARMUP", "50"))
N = int(os.environ.get("PYLAIR_BENCH_N", "500"))

S64 = "x" * (64 * 1024)
NESTED = {
    "shape": {
        "name": "tri",
        "origin": {"x": 0, "y": 0, "label": "o"},
        "points": [
            {"x": 1, "y": 2, "label": "a"},
            {"x": 3, "y": 4, "label": "b"},
            {"x": 5, "y": 6, "label": "c"},
        ],
        "meta": {"n": 3},
    }
}

CASES = [
    ("coerce_noop", lambda: coerce_kwargs(testapp.bench_noop, {})),
    ("coerce_64k", lambda: coerce_kwargs(testapp.bench_echo, {"s": S64})),
    ("coerce_nested", lambda: coerce_kwargs(testapp.bench_nested, dict(NESTED))),
    ("dumps_int", lambda: dumps(1)),
    ("dumps_64k", lambda: dumps(S64)),
    ("dumps_nested", lambda: dumps(NESTED["shape"])),
]


def percentile(sorted_samples: list[float], p: float) -> float:
    import math

    idx = min(len(sorted_samples) - 1, max(0, math.ceil(p * len(sorted_samples)) - 1))
    return sorted_samples[idx]


def main() -> None:
    for name, fn in CASES:
        for _ in range(WARMUP):
            fn()
        samples: list[float] = []
        for _ in range(N):
            t0 = time.perf_counter()
            fn()
            samples.append((time.perf_counter() - t0) * 1000.0)
        samples.sort()
        print(
            "py-bench: "
            + json.dumps(
                {
                    "case": name,
                    "warmup": WARMUP,
                    "n": N,
                    "p50": round(percentile(samples, 0.50), 4),
                    "p95": round(percentile(samples, 0.95), 4),
                    "p99": round(percentile(samples, 0.99), 4),
                }
            )
        )


if __name__ == "__main__":
    main()
