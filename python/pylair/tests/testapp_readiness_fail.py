"""Readiness test app: hook failures, attribution, and the sweep.

- bad_hook touches `model` then raises -> model failed w/ exception
- quiet_hook raises untouched -> synthesized `startup:quiet_hook`
- survivor runs anyway -> `sidecar` ready
- `ghost` is requires=-declared and never reported -> swept failed
"""

from pylair import expose, loading, on_startup, ready


@on_startup
def bad_hook():
    loading("model")
    raise RuntimeError("checkpoint corrupt")


@on_startup
def quiet_hook():
    raise KeyError("nothing reported")


@on_startup
def survivor():
    ready("sidecar")


@expose(requires="model")
def chat(prompt: str = "") -> str:
    return "unreachable"


@expose(requires="ghost")
def ghost_call() -> str:
    return "unreachable"


@expose
def ping() -> str:
    return "pong"
