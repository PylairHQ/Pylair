"""Wrap mode launcher (docs/wrap-mode-brief.md, docs/decisions/0003-wrap-mode.md).

`python -m pylair.wrap --cmd <argv...>` supervises ONE web server process
on behalf of the Rust supervisor: it speaks the normal frame protocol on
the private frame fd, picks a free loopback port, starts the server,
reports `ready {url}` once the server answers HTTP, and exits non-zero
the moment the server is gone or wedged so the supervisor restarts it.

`--framework gradio|streamlit --entry app.py` swaps the server for the
matching adapter module (`_gradio`, `_streamlit`): the adapter runs the
user's entry with the framework's host/port/share/telemetry knobs pinned
and a cheap health endpoint probed instead of the heavy page. The
framework itself comes from the app's env, never from pylair.
"""

from pylair.wrap._launcher import (
    EXIT_SERVER_EXITED,
    EXIT_START_FAILED,
    EXIT_UNHEALTHY,
    EXIT_USAGE,
    main,
)

__all__ = [
    "EXIT_SERVER_EXITED",
    "EXIT_START_FAILED",
    "EXIT_UNHEALTHY",
    "EXIT_USAGE",
    "main",
]
