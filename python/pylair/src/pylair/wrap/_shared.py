"""Shared helpers for the wrap framework adapters (`_gradio`, `_streamlit`)."""

from __future__ import annotations

import sys
from importlib import metadata


def log(msg: str) -> None:
    print(f"[pylair.wrap] {msg}", file=sys.stderr, flush=True)


# Majors each adapter was built and tested against (CI installs the
# `wrap` dependency group, pinned to these majors; see pyproject.toml).
# Anything else warns and still runs: launch() kwargs move between
# majors, but the failure mode is a broken page, not a dead launcher.
SUPPORTED_MAJORS: dict[str, tuple[int, ...]] = {
    "gradio": (5,),
    "streamlit": (1,),
}


def check_version(name: str, installed: str | None = None) -> str | None:
    """A warning string when the installed major is off the pin, else None.

    `installed` injects a version for tests; None reads the environment.
    A package that is not installed answers None too: its import error
    names the problem and a version warning on top would only be noise.
    """
    supported = SUPPORTED_MAJORS[name]
    pinned = "/".join(str(m) for m in supported)
    if installed is None:
        try:
            installed = metadata.version(name)
        except metadata.PackageNotFoundError:
            return None
    try:
        major = int(installed.split(".", 1)[0])
    except (AttributeError, ValueError):
        return (
            f"{name} version {installed!r} is not parseable; wrap mode "
            f"supports major {pinned}. Continuing anyway."
        )
    if major not in supported:
        return (
            f"{name} {installed} is not a supported major version (wrap "
            f"mode supports {pinned}). Continuing anyway."
        )
    return None
