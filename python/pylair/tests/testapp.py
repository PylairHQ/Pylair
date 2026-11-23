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

import pylair
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
    """Blocks the EVENT LOOP with a sync sleep - the pathological
    "blocking code inside an async def" case: inbound data floods while
    the loop is wedged and control must still reach dispatch."""
    time.sleep(seconds)  # noqa: ASYNC251 - blocking the loop is the point
    return "unblocked"


@expose
def sync_sleep(seconds: float) -> str:
    time.sleep(seconds)
    return "slept"


def _write_pidfile(path: str, pid: int) -> None:
    """Publish a pid atomically: a reader polling `exists()` must never
    see the empty file `open(path, "w")` leaves before the write lands."""
    tmp = f"{path}.tmp"
    with open(tmp, "w") as f:
        f.write(str(pid))
    os.replace(tmp, path)


@expose(isolated=True)
def isolated_sleep(seconds: float, pidfile: str | None = None) -> str:
    if pidfile:
        _write_pidfile(pidfile, os.getpid())
    time.sleep(seconds)
    return "slept"


@expose(isolated=True)
def sigterm_ignoring_sleep(seconds: float, pidfile: str | None = None) -> str:
    """Sleeps while ignoring SIGTERM: the kill path must burn the full
    SIGTERM grace and finish via SIGKILL (POSIX semantics only)."""
    import signal

    signal.signal(signal.SIGTERM, signal.SIG_IGN)
    if pidfile:
        _write_pidfile(pidfile, os.getpid())
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
    _write_pidfile(pidfile, proc.pid)
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


# ---- warm workers (decision 0008) ------------------------------------------
# Each fixture runs in the function's own pool; `warm_pid` is the identity
# witness (same pid = same process = warm), the rest drive one behavior.


@expose(worker={"mode": "warm"})
def warm_pid() -> int:
    return os.getpid()


@expose(worker={"mode": "warm"})
def warm_sleep(seconds: float, pidfile: str | None = None) -> str:
    if pidfile:
        _write_pidfile(pidfile, os.getpid())
    time.sleep(seconds)
    return "slept"


@expose(worker={"mode": "warm"})
def warm_maybe_crash(crash: bool = True) -> int:
    """Hard-die on demand — WORKER_DIED and crash-loop proofs. With
    crash=False it doubles as the success that resets the death count."""
    if crash:
        os._exit(3)
    return os.getpid()


_WARM_MARKS: list[str] = []


@expose(worker={"mode": "warm"})
def warm_mark(tag: str, delay: float = 0.0) -> list:
    """Append to a module global — a later call sees it, which proves both
    state persistence (same process) and FIFO order inside the pool."""
    if delay:
        time.sleep(delay)
    _WARM_MARKS.append(tag)
    return list(_WARM_MARKS)


@expose(worker={"mode": "warm", "pool": 2})
def warm2_sleep(seconds: float, pidfile: str | None = None) -> str:
    if pidfile:
        _write_pidfile(pidfile, os.getpid())
    time.sleep(seconds)
    return "slept"


@expose(worker={"mode": "warm"})
def warm_interrupt_immune(seconds: float, pidfile: str | None = None) -> str:
    """Ignores the interrupt signal entirely — the stuck path: the grace
    expires and the kill fallback runs."""
    import signal as _signal

    _signal.signal(_signal.SIGINT, _signal.SIG_IGN)
    if sys.platform == "win32":
        _signal.signal(_signal.SIGBREAK, _signal.SIG_IGN)
    if pidfile:
        _write_pidfile(pidfile, os.getpid())
    time.sleep(seconds)
    return "slept"


@expose(worker={"mode": "warm", "cancel_grace_ms": 150})
def warm_immune_fastgrace(seconds: float, pidfile: str | None = None) -> str:
    """Same ignoring worker with a 150 ms grace — proves the configured
    bound reaches the stuck path far faster than the 3 s default."""
    import signal as _signal

    _signal.signal(_signal.SIGINT, _signal.SIG_IGN)
    if sys.platform == "win32":
        _signal.signal(_signal.SIGBREAK, _signal.SIG_IGN)
    if pidfile:
        _write_pidfile(pidfile, os.getpid())
    time.sleep(seconds)
    return "slept"


@expose(worker={"mode": "warm"})
def warm_break_witness(witness: str, seconds: float, started: str | None = None) -> str:
    """Installs its own interrupt handler that writes `witness` before
    raising KeyboardInterrupt — a delivered-signal proof: the file can
    only exist if the interrupt actually reached this worker. `started`
    is a running-gate marker the test polls before cancelling."""
    import signal as _signal

    def _on_interrupt(_signum, _frame) -> None:
        with open(witness, "w") as f:
            f.write("break")
        raise KeyboardInterrupt

    if sys.platform == "win32":
        _signal.signal(_signal.SIGBREAK, _on_interrupt)
    else:
        _signal.signal(_signal.SIGINT, _on_interrupt)
    if started:
        with open(started, "w") as f:
            f.write("running")
    time.sleep(seconds)
    return "slept"


@expose(worker={"mode": "warm"})
def warm_gated_start(gate: str, pidfile: str | None = None, seconds: float = 0) -> int:
    """Poll `gate` before doing the real work — the call sits inside the
    raise window doing nothing until the test releases it. Returns the
    serving pid so a reuse compare needs no files at all; the pidfile
    also lands in `finally` — an interrupt firing at the earliest legal
    point (fn entry, before any write) still records which member ran."""
    try:
        deadline = time.time() + 120
        while not os.path.exists(gate):
            assert time.time() < deadline, "gate never released"
            time.sleep(0.05)
        time.sleep(seconds)
        return os.getpid()
    finally:
        if pidfile:
            _write_pidfile(pidfile, os.getpid())


@expose(worker={"mode": "warm"})
def warm_crash_stderr(marker: str) -> None:
    """Write a marker to stderr then die hard — the member's bounded
    stderr tail must carry it into WORKER_DIED."""
    print(marker, file=sys.stderr, flush=True)
    os._exit(3)


@expose(worker={"mode": "warm"})
def warm_gated_result(marker: str, size: int) -> dict:
    """Write `marker` (carrying this worker's pid) then return a payload
    big enough that building and writing its frame takes measurable
    time — the interrupt-vs-completion race test lands signals inside
    exactly that window."""
    with open(marker, "w") as f:
        f.write(str(os.getpid()))
    return {"pid": os.getpid(), "items": ["x"] * size}


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


# Latency bench (docs/performance.md): crates/.../tests/bench_latency.rs
# drives these through Handle::call on `python -m pylair testapp`; the
# same fixtures live in examples/reference-app/backend/app.py for the
# webview leg and in tests/bench_dispatch.py for the dispatch-only leg.
@dataclass
class BenchPoint:
    x: float
    y: float
    label: str


@dataclass
class BenchShape:
    name: str
    origin: BenchPoint
    points: list[BenchPoint]
    meta: dict[str, int]


@expose
def bench_noop() -> int:
    return 1


@expose
def bench_echo(s: str) -> str:
    return s


@expose
def bench_nested(shape: BenchShape) -> int:
    return len(shape.points) + shape.meta["n"]


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


# ---- app data dir (decision 0009, model manager PR 1) ----------------------
# The host hands the app's data dir to the runtime as PYLAIR_APP_DATA_DIR.
# Same env-witness shape as isolated_env_flag: one read in the runtime,
# one inside a warm worker — workers spawn from the runtime, so the var
# must propagate to them with no extra plumbing.


@expose
def app_data_dir_env() -> str | None:
    return os.environ.get("PYLAIR_APP_DATA_DIR")


@expose(worker={"mode": "warm"})
def warm_app_data_dir_env() -> str | None:
    return os.environ.get("PYLAIR_APP_DATA_DIR")


# ---- blob values (decision 0010) ---------------------------------------------
# Small payload fixtures for the wire/runtime tests: the runtime's
# response carries a $blob tag, the file lands under PYLAIR_BLOB_DIR, and
# the worker variants prove registrations ride home through the parent.


@expose
def make_blob() -> "pylair.Blob":
    return pylair.Blob(b"tiny-png-ish-bytes", mime="image/png")


@expose
def sized_blob(n: int) -> "pylair.Blob":
    """A Blob of exactly n bytes — the crate's limit tests drive the
    registry caps (per-blob, generation total, per-window share) with
    sizes they choose instead of make_blob's fixed payload."""
    return pylair.Blob(b"x" * n, mime="application/octet-stream")


@expose
def three_blobs() -> list:
    """One result carrying three $blob tags — three registers ahead of
    one terminal. The orphan-release test breaks the last register and
    asserts the first two never linger."""
    return [
        pylair.Blob(b"multi-1", mime="text/plain"),
        pylair.Blob(b"multi-2", mime="text/plain"),
        pylair.Blob(b"multi-3", mime="text/plain"),
    ]


@expose
async def stream_mixed_blobs() -> AsyncGenerator:
    """Chunk 1 carries one blob; chunk 2 carries three — the host's
    orphan-release must free the two that registered inside the
    refused chunk while the delivered chunk's handle stays live."""
    yield [pylair.Blob(b"s-first", mime="text/plain")]
    yield [
        pylair.Blob(b"s-a", mime="text/plain"),
        pylair.Blob(b"s-b", mime="text/plain"),
        pylair.Blob(b"s-c", mime="text/plain"),
    ]


@expose
async def stream_blob() -> AsyncGenerator:
    """One scalar chunk, then a Blob chunk — ordering must hold for
    stream.chunk frames exactly as for responses (§3)."""
    import pylair

    yield "lead"
    yield pylair.Blob(b"streamed-blob-bytes", mime="text/plain")


@expose
def make_blob_path(tmp_name: str) -> "pylair.Blob":
    """from_path — hardlink into the blob dir (source lives beside the
    dir in the test so the hardlink path provably applies)."""
    return pylair.Blob.from_path(tmp_name, mime="audio/wav")


@expose(isolated=True)
def isolated_blob() -> dict:
    """A Blob made in an isolated worker — the registration must ride
    back in the payload and the parent must bind it to this task."""
    import os

    return {
        "pid": os.getpid(),
        "dir": os.environ.get("PYLAIR_BLOB_DIR"),
        "blob": pylair.Blob(b"isolated-worker-bytes", mime="text/plain"),
    }


@expose(worker={"mode": "warm"})
def warm_blob() -> dict:
    import os

    return {
        "pid": os.getpid(),
        "dir": os.environ.get("PYLAIR_BLOB_DIR"),
        "blob": pylair.Blob(b"warm-worker-bytes", mime="text/plain"),
    }


@expose
def make_blob_copy(src: str) -> "pylair.Blob":
    """copy=True forces the plain-copy path even when a hardlink would
    work — the test asserts a second inode, not a shared one."""
    import pylair

    return pylair.Blob.from_path(src, mime="text/plain", copy=True)


# The SAME Blob object returned by any number of calls — a module-level
# cache. Every call must mint its own handle: fresh id, fresh file.


@expose
def shared_blob() -> pylair.Blob:
    if getattr(shared_blob, "_cached", None) is None:
        shared_blob._cached = pylair.Blob(b"shared-cached-payload", mime="text/plain")
    return shared_blob._cached
