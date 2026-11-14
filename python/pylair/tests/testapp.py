"""Test application module loaded by the runtime under test.

Exercises every execution mode plus the stdout-hygiene guarantees.
"""

import asyncio
import enum
import os
import sys
import time
from collections.abc import AsyncGenerator
from dataclasses import dataclass

from pylair import expose


@dataclass
class Point:
    x: float
    y: float
    label: str = ""


class Color(str, enum.Enum):
    RED = "red"
    BLUE = "blue"


@expose
def add(a: int, b: int) -> int:
    return a + b


@expose
def print_then_answer(text: str) -> str:
    # Both print() and a raw fd-1 write must land on stderr, never the
    # frame channel (protocol §1 stdout hygiene).
    print(f"print: {text}")
    os.write(1, b"raw fd-1 write\n")
    return text


@expose
async def echo_later(text: str, delay: float = 0.01) -> str:
    await asyncio.sleep(delay)
    return text


@expose
async def counter(n: int):
    for i in range(n):
        yield i


@expose
async def infinite():
    i = 0
    while True:
        yield i
        i += 1


_produced = 0


@expose
async def infinite_counted() -> AsyncGenerator[int, None]:
    """Infinite producer that counts what it yields — lets a test prove the
    producer actually STOPPED (not just that the consumer stopped reading)."""
    global _produced
    i = 0
    while True:
        _produced += 1
        yield i
        i += 1
        await asyncio.sleep(0)


@expose
def produced_count() -> int:
    return _produced


@expose
async def sleep_forever() -> None:
    await asyncio.sleep(3600)


@expose
async def loop_block(seconds: float) -> str:
    """Blocks the EVENT LOOP with a sync sleep — the pathological
    "blocking code inside an async def" case: inbound data floods while
    the loop is wedged and control must still reach dispatch."""
    time.sleep(seconds)  # noqa: ASYNC251 - blocking the loop is the point
    return "unblocked"


@expose
def sync_sleep(seconds: float) -> str:
    time.sleep(seconds)
    return "slept"


@expose(isolated=True)
def isolated_sleep(seconds: float) -> str:
    time.sleep(seconds)
    return "slept"


@expose(isolated=True)
def isolated_add(a: int, b: int) -> int:
    return a + b


@expose(isolated=True)
def isolated_env_flag() -> dict:
    """Report the env the isolated WORKER actually inherited — the proof
    that release-spec vars like PYTHONDONTWRITEBYTECODE reach workers
    spawned by the runtime, not just the runtime itself."""
    import os
    import sys

    return {
        "PYTHONDONTWRITEBYTECODE": os.environ.get("PYTHONDONTWRITEBYTECODE"),
        "dont_write_bytecode": sys.dont_write_bytecode,
    }


@expose(isolated=True)
def isolated_print(text: str) -> str:
    # stdout hygiene inside the worker: print() must not corrupt the result.
    print(f"worker print: {text}")
    return text


@expose(isolated=True)
def orphan_maker(pidfile: str) -> str:
    """Spawn a grandchild that IGNORES SIGTERM, then sleep.

    Cancelling must kill the whole process group — the grandchild surviving
    is the leaked-orphan failure mode.
    """
    import subprocess

    proc = subprocess.Popen(
        [
            sys.executable,
            "-c",
            (
                "import signal, time\n"
                "signal.signal(signal.SIGTERM, signal.SIG_IGN)\n"
                "time.sleep(600)\n"
            ),
        ]
    )
    with open(pidfile, "w") as f:
        f.write(str(proc.pid))
    time.sleep(600)
    return "never"


@expose(isolated=True)
def sigterm_immune(ready_file: str) -> str:
    """Worker that ignores SIGTERM and sleeps: its terminate path must
    wait the whole ISOLATED_GRACE_S before SIGKILL lands. Used to prove
    a slow kill doesn't stall the dispatch lane."""
    import signal

    signal.signal(signal.SIGTERM, signal.SIG_IGN)
    with open(ready_file, "w") as f:
        f.write("ready")
    time.sleep(600)
    return "never"


@expose
def move(p: Point, dx: float = 0.0) -> Point:
    """Translate a point; dataclass in, dataclass out."""
    return Point(x=p.x + dx, y=p.y, label=p.label)


@expose
def pick(c: Color, fallback: str | None = None) -> str:
    return c.value if c is not None else (fallback or "none")


@expose
def roundtrip(data: bytes) -> bytes:
    return data[::-1]


@expose
async def numbers(n: int) -> AsyncGenerator[int, None]:
    for i in range(n):
        yield i


@expose
def nan_score() -> float:
    """Returns NaN — must fail this one call, never the runtime (§2)."""
    return float("nan")


@expose
def opaque_result() -> object:
    """A non-serializable result (stands in for numpy/tensor values)."""
    return object()


@expose
async def bad_stream() -> AsyncGenerator[int, None]:
    """A stream whose second chunk can't be serialized."""
    yield 1
    yield object()  # type: ignore[misc]


@expose
async def wedge_the_loop() -> None:
    # Starves the event loop in native code — the fault-injection case for
    # ping/pong liveness. Never returns on its own.
    time.sleep(3600)  # noqa: ASYNC251 - intentional event-loop starvation


@expose
async def block_loop(seconds: float) -> str:
    # Same starvation as wedge_the_loop but bounded and it returns: the
    # diagnostic-vs-kill case (a bounded stall must not be killed).
    time.sleep(seconds)  # noqa: ASYNC251 - intentional event-loop starvation
    return "unblocked"


def _nest(n: int):
    """n-deep nested dict: {"d": {"d": … {"d": 1}}}."""
    out = 1
    for _ in range(n):
        out = {"d": out}
    return out


@expose
def deep(n: int):
    # serde_json on the host refuses payloads nested deeper than 128 —
    # a legal Python result must fail THIS call, not the runtime.
    return _nest(n)


@expose
async def deep_stream(n: int):
    yield _nest(n)


@expose(isolated=True)
def isolated_deep(n: int):
    return _nest(n)


_spy_calls = 0


@expose
def spy(a: int) -> int:
    # Witness for input validation: on a rejected call this body must
    # never run — count invocations so tests can prove it.
    global _spy_calls
    _spy_calls += 1
    return a


@expose
def spy_count() -> int:
    return _spy_calls


@expose
def int_or_str(v: int | str):
    return v


@expose
def passthrough(x):
    # Unannotated parameters are not checked — any wire value passes.
    return type(x).__name__


@expose(isolated=True)
def isolated_spy(a: int) -> int:
    return a


@expose(isolated=True)
def isolated_pick(c: str | None = None) -> str:
    return c or "none"


@expose
def maybe(x: str = None) -> str:  # noqa: RUF013 - implicit-optional on purpose
    # A None *default* admits None even without an Optional annotation.
    return repr(x)


@expose(isolated=True)
def isolated_maybe(x: str = None) -> str:  # noqa: RUF013
    return repr(x)
