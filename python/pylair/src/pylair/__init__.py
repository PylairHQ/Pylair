"""Pylair Python runtime.

Provides the @expose decorator, the JSON-RPC protocol server, the task
runner, cancellation handling and schema export. See docs/protocol-v1.md.
"""

from importlib.metadata import PackageNotFoundError, version

from pylair import models
from pylair._blob import Blob
from pylair._expose import expose
from pylair._readiness import failed, loading, on_startup, ready

__all__ = ["Blob", "expose", "failed", "loading", "models", "on_startup", "ready"]


def _version() -> str:
    try:
        return version("pylair")
    except PackageNotFoundError:
        # Source checkout that was never installed: no dist-info exists.
        return "0.0.0+unknown"


__version__ = _version()
