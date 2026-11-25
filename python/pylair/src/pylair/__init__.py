"""Pylair Python runtime.

Provides the @expose decorator, the JSON-RPC protocol server, the task
runner, cancellation handling and schema export. See docs/protocol-v1.md.
"""

from importlib.metadata import PackageNotFoundError, version

from pylair import models, update
from pylair._bigint import BigInt
from pylair._blob import Blob
from pylair._expose import WorkerConfig, expose, isolated, warm
from pylair._readiness import failed, loading, on_startup, ready

__all__ = [
    "BigInt",
    "Blob",
    "WorkerConfig",
    "expose",
    "failed",
    "isolated",
    "loading",
    "models",
    "on_startup",
    "ready",
    "update",
    "warm",
]


def _version() -> str:
    try:
        return version("pylair")
    except PackageNotFoundError:
        # Source checkout that was never installed: no dist-info exists.
        return "0.0.0+unknown"


__version__ = _version()
