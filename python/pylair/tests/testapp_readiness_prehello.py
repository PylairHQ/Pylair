"""Readiness test app: import-time (pre-server) reports and the
first-post-hello flush.

- `boot`: loading()+ready() at import -> coalesces to exactly one
  'ready' frame in the first flush.
- `early`: requires=-declared AND ready at import -> one 'ready'
  frame, never a 'loading' one.
- `model`: requires=-declared, hook parks -> exactly one 'loading'
  frame in the first flush, 'ready' only after release().
"""

import threading

import pylair
from pylair import expose, on_startup, ready

pylair.loading("boot", 0.1)
pylair.ready("boot")
pylair.ready("early")

_gate = threading.Event()


@on_startup
def warm():
    _gate.wait(timeout=30)
    ready("model")


@expose(requires=("early", "model"))
def chat(prompt: str) -> str:
    return f"answered:{prompt}"


@expose
def release() -> str:
    _gate.set()
    return "released"
