"""Readiness test app: zero hooks — the sweep fails a declared
component immediately at end-of-startup (0007 §1)."""

from pylair import expose


@expose(requires="ghost")
def ghost_call() -> str:
    return "unreachable"


@expose
def ping() -> str:
    return "pong"
