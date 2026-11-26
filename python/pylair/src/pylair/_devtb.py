"""Dev-only tracebacks in error data (protocol v1, error.data.traceback).

A Python exception raised by an exposed function carries a formatted
traceback in error.data ONLY when the host opts in with
PYLAIR_DEV_TRACEBACKS=1 — which the host sets solely in debug builds.
Release builds must never ship these: frames leak source paths and code
into end-user machines.

The traceback is trimmed to the app's own frames plus the raise site —
pylair's internal dispatch frames (this package) are noise for the
developer, who cares where THEIR code failed — and capped so a deep or
verbose recursion can't blow the frame budget on its own.
"""

from __future__ import annotations

import os
import traceback
from pathlib import Path

_CAP = 8192  # ~8 KB of formatted traceback, then a truncation marker
_PKG_DIR = str(Path(__file__).resolve().parent)


def enabled(env: dict | None = None) -> bool:
    env = os.environ if env is None else env
    return env.get("PYLAIR_DEV_TRACEBACKS") == "1"


def format_dev_traceback(exc: BaseException, env: dict | None = None) -> str | None:
    """The formatted traceback for `exc`, or None when the gate is closed.

    Frames inside the pylair package itself are dropped (dispatch,
    coercion and task-machinery frames); everything else — the app's own
    code, library frames it went through, and the raise site — stays in
    Python's own format so the console output reads like a normal crash.
    """
    if not enabled(env):
        return None
    tb = exc.__traceback__
    if tb is None:
        return None
    extracted = traceback.extract_tb(tb)
    kept = [f for f in extracted if not f.filename.startswith(_PKG_DIR)]
    if not kept:
        # The failure rose entirely inside pylair machinery — keep the
        # deepest frame so the output is never an empty, confusing list.
        kept = extracted[-1:]
    lines = ["Traceback (most recent call last):\n"]
    lines += traceback.format_list(kept)
    lines += traceback.format_exception_only(type(exc), exc)
    out = "".join(lines)
    if len(out) > _CAP:
        out = out[:_CAP] + "\n... [traceback truncated at 8 KB]\n"
    return out
