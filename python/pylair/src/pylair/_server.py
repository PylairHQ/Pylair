"""The runtime frame loop (protocol v1).

Threads: a reader thread blocks on stdin and pushes parsed messages to the
event loop; a writer thread serializes outbound frames so the loop never
blocks on a full pipe. stdin EOF = host gone -> cancel everything and
exit (protocol §6).
"""

from __future__ import annotations

import asyncio
import collections
import os
import queue
import sys
import threading
import time
import traceback
from typing import Any

from pylair import _json
from pylair import _protocol as proto
from pylair._expose import exposed_functions, exposes_manifest
from pylair._tasks import MAX_STREAM_CREDIT, TaskRegistry
from pylair._transport import MAX_DEPTH, MAX_FRAME, FrameChannel, FrameTooLarge

_EOF = object()
_STOP = object()

# RC.8A outbound bound. The frame-count cap must exceed the worst-case
# LEGITIMATE burst — every frame the protocol allows to be in flight at
# once — so that "full" can only mean the host stopped draining:
#   max_concurrent_streams * max_stream_credit   (chunks produced under
#                                                 granted-but-unconsumed
#                                                 credit)
# + max_concurrent_tasks                       (call results in flight)
# + slack                                      (pongs, errors, control)
# Memory is bounded separately by OUT_MAX_BYTES — the byte cap is the real
# protection; the frame cap exists to bound queue bookkeeping.
_MAX_STREAMS = int(os.environ.get("PYLAIR_MAX_CONCURRENT_STREAMS", "32"))
_MAX_TASKS = int(os.environ.get("PYLAIR_MAX_CONCURRENT_TASKS", "64"))
OUT_MAX_FRAMES = int(
    os.environ.get(
        "PYLAIR_OUT_MAX_FRAMES",
        str(_MAX_STREAMS * MAX_STREAM_CREDIT + _MAX_TASKS + 4096),
    )
)
OUT_MAX_BYTES = int(os.environ.get("PYLAIR_OUT_MAX_BYTES", str(64 * 1024 * 1024)))
# Inbound bound: the reader stops pulling frames when the loop falls this
# far behind — backpressure travels through the OS pipe to the host's own
# bounded writer lane, so nothing here grows without bound either.
IN_MAX = int(os.environ.get("PYLAIR_IN_MAX", "4096"))


def _log(msg: str) -> None:
    print(f"[pylair] {msg}", file=sys.stderr, flush=True)


class _BoundedOut:
    """Frame+byte bounded queue for outbound frames (RC.8A).

    `put_nowait` NEVER blocks — it raises `queue.Full` when either bound is
    hit, which the caller turns into the host-gone path. `get` blocks the
    dedicated writer thread only.
    """

    def __init__(self, max_frames: int, max_bytes: int) -> None:
        self._items: collections.deque[Any] = collections.deque()
        self._bytes = 0
        self._max_frames = max_frames
        self._max_bytes = max_bytes
        self._cond = threading.Condition()

    def put_nowait(self, body: Any) -> None:
        with self._cond:
            n = len(body) if isinstance(body, (bytes, bytearray)) else 0
            if (
                len(self._items) >= self._max_frames
                or self._bytes + n > self._max_bytes
            ):
                raise queue.Full(
                    f"outbound queue full ({len(self._items)} frames, {self._bytes} bytes)"
                )
            self._items.append(body)
            self._bytes += n
            self._cond.notify()

    def close(self) -> None:
        # The _STOP sentinel bypasses the caps: shutdown must always be
        # deliverable, even on a full queue.
        with self._cond:
            self._items.append(_STOP)
            self._cond.notify()

    def get(self) -> Any:
        with self._cond:
            while not self._items:
                self._cond.wait()
            body = self._items.popleft()
            self._bytes -= len(body) if isinstance(body, (bytes, bytearray)) else 0
            return body

    def stats(self) -> tuple[int, int]:
        with self._cond:
            return len(self._items), self._bytes


class Server:
    def __init__(self, channel: FrameChannel) -> None:
        self._channel = channel
        self._out = _BoundedOut(OUT_MAX_FRAMES, OUT_MAX_BYTES)
        self._in: asyncio.Queue = asyncio.Queue()
        self._in_max = IN_MAX
        self._tasks = TaskRegistry(self._send)
        self._stopping = False
        self._out_full = False
        self._loop: asyncio.AbstractEventLoop | None = None
        # Last time the asyncio loop proved it was making progress; read by the
        # reader thread when answering pings (single float store: atomic).
        self._loop_beat = time.monotonic()

    def _send(self, msg: dict) -> None:
        # Depth check before serialization: a frame nested deeper than
        # the host's parser limit is a malformed frame on arrival — fail
        # just this call with the typed error instead of killing the
        # runtime. A notification has no caller to report to: drop it.
        if _json.exceeds_depth(msg, MAX_DEPTH):
            rid = msg.get("id")
            if isinstance(rid, (int, str)):
                msg = proto.error_response(
                    rid,
                    proto.LIMIT_EXCEEDED,
                    "response exceeds the depth cap",
                    {"limit": "max_depth", "max": MAX_DEPTH},
                )
            else:
                _log(
                    f"dropping outbound frame over the depth cap ({MAX_DEPTH} levels, "
                    f"{msg.get('method', 'response')}) — host could not parse it"
                )
                return
        # Serialize HERE, on the caller's thread (the event loop for task
        # results): a TypeError/ValueError can then be turned into an
        # INTERNAL response for that one call. The writer thread only ever
        # sees valid bytes and must never die (protocol §2).
        body = _json.dumps(msg).encode()
        if len(body) > MAX_FRAME and isinstance(msg.get("id"), (int, str)):
            # An oversized RESPONSE would trip the host's read cap and kill
            # the runtime — fail just this call with the typed error (RC.8A).
            body = _json.dumps(
                proto.error_response(
                    msg["id"],
                    proto.LIMIT_EXCEEDED,
                    "response exceeds the frame cap",
                    {"limit": "max_frame", "max": MAX_FRAME},
                )
            ).encode()
        try:
            self._out.put_nowait(body)
        except queue.Full:
            # Never block the caller (event loop or reader thread). A host
            # that stopped draining is functionally gone — take the same
            # drain-and-exit path as stdin EOF, with a distinct reason.
            if not self._out_full:
                self._out_full = True
                frames, nbytes = self._out.stats()
                _log(
                    f"outbound queue full ({frames} frames, {nbytes} bytes) "
                    "— host not draining; treating as gone"
                )
            loop = self._loop
            if loop is not None:
                loop.call_soon_threadsafe(self._in.put_nowait, _EOF)

    # ---- io threads --------------------------------------------------------

    def _writer(self) -> None:
        while True:
            body = self._out.get()
            if body is _STOP:
                return
            try:
                self._channel.write_frame(body)
            except (OSError, ValueError):
                # Frame fd dead — host is gone. OSError covers dead fds;
                # ValueError covers a closed buffered stream at shutdown
                # ("I/O operation on closed file").
                return
            except Exception as e:  # noqa: BLE001 - writer must never die
                _log(f"writer dropped a frame: {e!r}")

    def _reader(self, loop: asyncio.AbstractEventLoop) -> None:
        try:
            while True:
                body = self._channel.read_frame()
                if body is None:
                    loop.call_soon_threadsafe(self._in.put_nowait, _EOF)
                    return
                try:
                    msg = _json.loads(body)
                except Exception:  # noqa: BLE001 - any decode failure is PARSE_ERROR
                    msg = {
                        "__parse_error__": True,
                        "raw": body[:200].decode(errors="replace"),
                    }
                if (
                    isinstance(msg, dict)
                    and not msg.get("__parse_error__")
                    and msg.get("method") == "ping"
                ):
                    # Pongs are answered off the event loop (protocol §5):
                    # blocking code inside an `async def` must not starve
                    # liveness. Event-loop lag is reported separately.
                    params = msg.get("params")
                    seq = params.get("seq") if isinstance(params, dict) else None
                    stale_ms = int((time.monotonic() - self._loop_beat) * 1000)
                    self._send(
                        proto.notification(
                            "pong", {"seq": seq, "loop_stale_ms": stale_ms}
                        )
                    )
                    continue
                # Inbound bound (RC.8A): if the loop is behind, stop pulling
                # frames — the OS pipe backs up to the host's own bounded
                # writer, so the queue can't grow without limit.
                while self._in.qsize() >= self._in_max:
                    time.sleep(0.001)
                loop.call_soon_threadsafe(self._in.put_nowait, msg)
        except FrameTooLarge as e:
            _log(f"protocol violation: {e} — treating host as gone")
            loop.call_soon_threadsafe(self._in.put_nowait, _EOF)
        except OSError:
            loop.call_soon_threadsafe(self._in.put_nowait, _EOF)

    # ---- dispatch ----------------------------------------------------------

    async def _handle(self, msg: dict) -> None:
        if not isinstance(msg, dict):
            # A valid-JSON scalar/array frame is still not a request —
            # typed rejection, never an AttributeError on the loop.
            self._send(
                proto.error_response(None, proto.INVALID_REQUEST, "invalid request")
            )
            return
        if msg.get("__parse_error__"):
            _log(f"unparseable frame: {msg['raw']!r}")
            self._send(
                proto.error_response(None, proto.PARSE_ERROR, "malformed JSON frame")
            )
            return

        if proto.is_notification(msg):
            # A notification has no caller to report to — malformed
            # params drop the frame, never the loop.
            try:
                method, params = msg["method"], msg.get("params")
                if params is None:
                    params = {}
                if not isinstance(params, dict):
                    _log(f"dropping notification {method}: params is not an object")
                    return
                if method == "stream.credit":
                    task_id = params.get("task")
                    n = params.get("n", 0)
                    if isinstance(task_id, (int, str)) and isinstance(n, int) and n > 0:
                        self._tasks.grant_credit(task_id, n)
                else:
                    _log(f"unknown notification: {method}")
            except Exception as e:  # noqa: BLE001 - loop must survive
                _log(f"notification handler error: {e!r}")
            return

        if proto.is_response(msg):
            _log("unexpected response from host (runtime sends no requests)")
            return

        if not proto.is_request(msg):
            _log(f"invalid message shape: {msg!r}")
            self._send(
                proto.error_response(
                    msg.get("id"), proto.INVALID_REQUEST, "invalid request"
                )
            )
            return

        rid, method = msg["id"], msg["method"]
        params = msg.get("params")
        if params is None:
            params = {}
        if not isinstance(params, dict):
            self._send(
                proto.error_response(
                    rid, proto.INVALID_PARAMS, "params must be an object"
                )
            )
            return
        try:
            if method == "ping":
                # Unreachable — the reader thread answers pings (§5). Kept
                # defensively so a direct dispatch still gets a pong.
                self._send(proto.notification("pong", {"seq": params.get("seq")}))
            elif method == "call":
                await self._call(rid, params)
            elif method == "cancel":
                # Synchronous half only — the response is sent by
                # cancel() itself, or by a spawned waiter for slow
                # isolated kills, so a terminate/reap never occupies the
                # dispatch lane.
                self._tasks.cancel(params.get("task"), rid)
            elif method == "shutdown":
                self._send(proto.response(rid, {}))
                self._stopping = True
                grace = params.get("grace_ms", 5000) / 1000
                await self._drain(grace)
            elif method == "stream.credit":  # tolerate request-shaped credit
                self._send(
                    proto.error_response(
                        rid, proto.INVALID_REQUEST, "stream.credit is a notification"
                    )
                )
            else:
                self._send(
                    proto.error_response(
                        rid, proto.FUNCTION_NOT_FOUND, f"unknown method {method!r}"
                    )
                )
        except Exception as e:  # noqa: BLE001 - dispatch must not die
            _log(f"dispatch error for {method}: {e}\n{traceback.format_exc(limit=10)}")
            self._send(
                proto.error_response(
                    rid, proto.INTERNAL, str(e), {"type": type(e).__name__}
                )
            )

    async def _call(self, rid: Any, params: dict) -> None:
        fn_name = params.get("fn")
        task_id = params.get("task")
        # Advance the id high-water mark even on paths that reject below —
        # the call frame was handled, so a later cancel must not pend.
        self._tasks.note_call_id(task_id)
        input = params.get("input") or {}
        if not isinstance(input, dict):
            self._send(
                proto.error_response(
                    rid, proto.INVALID_PARAMS, "input must be an object"
                )
            )
            return
        ex = exposed_functions().get(fn_name)
        if ex is None:
            self._send(
                proto.error_response(
                    rid, proto.FUNCTION_NOT_FOUND, f"no such function {fn_name!r}"
                )
            )
            return
        if task_id is None:
            self._send(
                proto.error_response(
                    rid, proto.INVALID_PARAMS, "call requires a task id"
                )
            )
            return
        # The host declares client intent via `stream` (§3): plain call()
        # sends False, stream() sends True. A mismatch means the caller
        # used the wrong API — say so instead of streaming into the void
        # (call() drops chunks) or silently ending (stream() on a plain
        # fn). Absent on older hosts → no validation.
        want_stream = params.get("stream")
        if want_stream is not None and bool(want_stream) != (ex.kind == "generator"):
            if ex.kind == "generator":
                msg = (
                    f"{fn_name!r} is a stream function — call it with "
                    "stream(), not call()"
                )
            else:
                msg = (
                    f"{fn_name!r} is not a stream function — call it "
                    "with call(), not stream()"
                )
            self._send(proto.error_response(rid, proto.INVALID_PARAMS, msg))
            return
        try:
            input = _json.coerce_kwargs(ex.fn, input)
        except _json.InvalidParams as e:
            self._send(proto.error_response(rid, proto.INVALID_PARAMS, str(e)))
            return
        await self._tasks.start(task_id, rid, fn_name, ex.kind, ex.fn, input)
        # Handlers emit the response: value calls on completion, stream
        # calls at the terminal message (protocol §3.1).

    # ---- lifecycle ---------------------------------------------------------

    async def _drain(self, grace_s: float) -> None:
        """Cancel tasks, then exit. Called on shutdown and stdin EOF."""
        try:
            await asyncio.wait_for(self._tasks.cancel_all(), timeout=grace_s)
        except TimeoutError:
            _log("grace period expired during drain")
            for t in self._tasks.all():
                await self._tasks._kill_worker(
                    t
                )  # isolated workers must not be orphaned
        for t in self._tasks.all():
            if t.asyncio_task is not None:
                t.asyncio_task.cancel()

    async def _loop_heartbeat(self) -> None:
        """Stamp loop progress. If the loop is blocked this stops advancing,
        which the host sees as `loop_stale_ms` in the next pong."""
        while True:
            self._loop_beat = time.monotonic()
            await asyncio.sleep(0.25)

    async def _lag_monitor(self) -> None:
        """Diagnostic: report event-loop lag on stderr (forwarded to the
        UI). Blocking code inside an `async def` no longer fails
        liveness, so this is how a wedged loop stays visible (§5)."""
        t = asyncio.get_running_loop().time()
        while True:
            await asyncio.sleep(1.0)
            now = asyncio.get_running_loop().time()
            lag = now - t - 1.0
            t = now
            if lag > 1.0:
                _log(
                    f"event loop lag {lag:.2f}s — blocking code inside "
                    "an async def starves calls (it no longer fails "
                    "liveness; see docs/cancellation.md)"
                )

    async def run(self) -> None:
        loop = asyncio.get_running_loop()
        self._loop = loop
        writer = threading.Thread(target=self._writer, name="tp-writer", daemon=True)
        reader = threading.Thread(
            target=self._reader, args=(loop,), name="tp-reader", daemon=True
        )
        writer.start()
        self._send(proto.hello(exposes_manifest()))
        reader.start()
        lag = asyncio.create_task(self._lag_monitor())
        beat = asyncio.create_task(self._loop_heartbeat())

        while not self._stopping:
            msg = await self._in.get()
            if msg is _EOF:
                _log("stdin EOF — host gone, draining")
                break
            await self._handle(msg)

        for t in (lag, beat):
            t.cancel()
            try:
                await t
            except asyncio.CancelledError:
                pass
        await self._drain(grace_s=3.0)
        self._out.close()
        # _STOP is appended after every queued frame, so the writer exits
        # only once the queue is empty. Join it BEFORE closing the fd —
        # closing first makes write_frame fail on the closed stream and
        # every final frame (shutdown ack, TASK_CANCELLEDs) is dropped.
        writer.join(timeout=2)
        self._channel.close_write()


def run(channel: FrameChannel) -> None:
    try:
        asyncio.run(Server(channel).run())
    except BaseException as e:
        _log(f"fatal: {type(e).__name__}: {e}\n{traceback.format_exc(limit=15)}")
        raise
