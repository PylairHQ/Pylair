"""Wrap mode launcher (docs/wrap-mode-brief.md, docs/decisions/0003-wrap-mode.md).

`python -m pylair.wrap --cmd <argv...>` supervises ONE web server process
on behalf of the Rust supervisor: it speaks the normal frame protocol on
the private frame fd, picks a free loopback port, starts the server,
reports `ready {url}` once the server answers HTTP, and exits non-zero
the moment the server is gone or wedged so the supervisor restarts it.
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
