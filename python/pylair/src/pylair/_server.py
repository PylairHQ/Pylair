"""The runtime frame loop (protocol v1).

Threads: a reader thread blocks on stdin and pushes parsed messages to the
event loop; a writer thread serializes outbound frames so the loop never
blocks on a full pipe. stdin EOF = host gone -> cancel everything and
exit (protocol §6).
"""

from __future__ import annotations

import asyncio
import collections
import itertools
import os
import queue
import sys
import threading
import time
import traceback
from typing import Any

from pylair import _json, _readiness
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
# Inbound bound (RC.8A): the reader NEVER waits. The bound counts data
# frames read but not yet dispatched - `_in_read` minus `_in_done` -
# because a `call_soon_threadsafe` put still parked on the wedged loop is
# invisible to `Queue.qsize()`. Each counter has exactly one writer:
# `_in_read` is incremented only on the reader thread, `_in_done` only on
# the loop at dispatch, so a single `+=` per side needs no lock and can
# never lose an update the way one shared `+=`/`-=` counter could. The
# reader may see a slightly stale `_in_done`, which only makes the bound
# a touch conservative, never permissive. A data frame over the bound is
# rejected (or dropped) on the spot, so a loop that falls behind cannot
# grow memory and a control frame later in the pipe is never stuck behind
# a sleeping reader. Dispatch order is a priority queue: 0 = control,
# 1 = data, 2 = _EOF. Control sorts ahead of queued data so a flood never
# delays a cancel/shutdown behind the backlog, and _EOF sorts after every
# received frame so stdin close can never preempt work that is already
# here.
IN_MAX = int(os.environ.get("PYLAIR_IN_MAX", "4096"))
# Requests and notifications that must reach dispatch even under a data
# flood. `ping` never reaches the queue - it is answered in the reader.
_CONTROL_METHODS = frozenset({"cancel", "stream.credit", "shutdown"})


def _log(msg: str) -> None:
    print(f"[pylair] {msg}", file=sys.stderr, flush=True)


def _hard_exit(code: int) -> None:
    """Last-resort process exit that skips every cleanup layer — used
    when a cleanup step itself can deadlock (#142). Tests monkeypatch
    this to observe the call without exiting the test process."""
    os._exit(code)


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
        # Priority queue of (prio, seq, msg): control (0) always ahead of
        # data (1); seq keeps FIFO inside a class since dicts don't
        # compare. The data bound lives in the reader, not here.
        self._in: asyncio.PriorityQueue = asyncio.PriorityQueue()
        self._in_seq = itertools.count()
        self._in_max = IN_MAX
        # Data frames read but not yet dispatched. `_in_read` is written
        # only by the reader thread, `_in_done` only by the loop - each
        # counter has a single writer, so no lock is needed.
        self._in_read = 0
        self._in_done = 0
        self._tasks = TaskRegistry(self._send)
        self._stopping = False
        self._out_full = False
        self._loop: asyncio.AbstractEventLoop | None = None
        # Set when `shutdown` arrives: grace_ms becomes ONE deadline the
        # drain, the post-loop drain, the worker kills and the writer
        # flush all share - never a fresh timeout per phase (protocol §7).
        self._deadline: float | None = None
        # Last time the asyncio loop proved it was making progress; read by the
        # reader thread when answering pings (single float store: atomic).
        self._loop_beat = time.monotonic()

    def _remaining(self, cap: float | None = None) -> float | None:
        """Time left in the shared shutdown budget.

        `cap` is a phase's own ceiling; a set deadline wins and the phase
        gets whichever is smaller. No deadline -> the cap stands alone.
        """
        if self._deadline is None:
            return cap
        rem = max(self._deadline - time.monotonic(), 0.0)
        return rem if cap is None else min(cap, rem)

    def _send(self, msg: dict, task_id: int | str | None = None) -> None:
        # Serialize HERE, on the caller's thread (the event loop for task
        # results): a TypeError/ValueError can then be turned into an
        # INTERNAL response for that one call. The writer thread only ever
        # sees valid bytes and must never die (protocol §2).
        # The depth check rides inside dumps' single pre-walk: a frame
        # nested deeper than the host's parser limit is a malformed frame
        # on arrival — fail just this call with the typed error instead of
        # killing the runtime. A notification has no caller: drop it.
        regs: dict = {}
        try:
            body = _json.dumps_collect(msg, regs, MAX_DEPTH).encode()
        except _json.DepthExceeded:
            rid = msg.get("id")
            if isinstance(rid, (int, str)):
                msg = proto.error_response(
                    rid,
                    proto.LIMIT_EXCEEDED,
                    "response exceeds the depth cap",
                    {"limit": "max_depth", "max": MAX_DEPTH},
                )
                body = _json.dumps(msg).encode()
            else:
                _log(
                    f"dropping outbound frame over the depth cap ({MAX_DEPTH} levels, "
                    f"{msg.get('method', 'response')}) — host could not parse it"
                )
                return
        except _json.BlobFallbackExceeded as e:
            # A Blob on a host without PYLAIR_BLOB_DIR whose $b64 can't
            # fit the frame cap — fail this call naming the feature
            # (decision 0010 §3); never silently drop the value.
            rid = msg.get("id")
            if isinstance(rid, (int, str)):
                body = _json.dumps(
                    proto.error_response(
                        rid,
                        proto.LIMIT_EXCEEDED,
                        str(e),
                        {"limit": "max_frame", "feature": "blob"},
                    )
                ).encode()
            else:
                _log(f"dropping outbound frame: {e}")
                return
        if len(body) > MAX_FRAME and isinstance(msg.get("id"), (int, str)):
            # An oversized RESPONSE would trip the host's read cap and kill
            # the runtime — fail just this call with the typed error (RC.8A).
            for h in regs.values():
                h._unlink()  # materialized but the tag never ships
            regs.clear()
            body = _json.dumps(
                proto.error_response(
                    msg["id"],
                    proto.LIMIT_EXCEEDED,
                    "response exceeds the frame cap",
                    {"limit": "max_frame", "max": MAX_FRAME},
                )
            ).encode()
        if regs and task_id is None:
            # A Blob can only travel a frame the host can bind to a task —
            # call responses and stream.chunks. Anything else (events,
            # readiness, stray notifications) would ship a tag whose
            # register the crate drops as owner-less and the URL 404s.
            # Refuse it instead: nothing registered, files unlinked, and
            # the developer gets the rule, not a dead link.
            for h in regs.values():
                h._unlink()
            regs.clear()
            kind = msg.get("method") or "response"
            why = (
                f"outbound {kind!r} frame carries a Blob but has no task "
                "binding — Blobs can only be returned from calls or streams"
            )
            rid = msg.get("id")
            if isinstance(rid, (int, str)):
                body = _json.dumps(
                    proto.error_response(rid, proto.INTERNAL, why)
                ).encode()
            else:
                _log(f"dropping {why}")
                return
        # Registration-before-use (decision 0010 §3): every $blob tag in
        # the carrying frame is registered with the host first — same
        # caller thread, sequential puts, one writer FIFO. The owner is
        # bound by the host from task_id, never trusted from here.
        for h in regs.values():
            h._registered = True
            self._out.put_nowait(
                _json.dumps(
                    proto.notification(
                        "blob.register",
                        {**h._register_params(), "task_id": task_id},
                    )
                ).encode()
            )
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
                loop.call_soon_threadsafe(
                    self._in.put_nowait, (2, next(self._in_seq), _EOF)
                )

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
                    loop.call_soon_threadsafe(
                        self._in.put_nowait, (2, next(self._in_seq), _EOF)
                    )
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
                # Inbound bound: control enqueues ahead of queued data;
                # data beyond IN_MAX is rejected or dropped - the reader
                # never waits, so control later in the pipe is never
                # stuck behind data (see the IN_MAX comment).
                method = msg.get("method") if isinstance(msg, dict) else None
                if method in _CONTROL_METHODS:
                    prio = 0
                elif self._in_read - self._in_done < self._in_max:
                    self._in_read += 1
                    prio = 1
                else:
                    rid = msg.get("id") if isinstance(msg, dict) else None
                    if isinstance(rid, (int, str)):
                        self._send(
                            proto.error_response(
                                rid,
                                proto.LIMIT_EXCEEDED,
                                "inbound queue full - loop is behind",
                                {"limit": "inbound_queue", "max": self._in_max},
                            )
                        )
                    else:
                        _log(
                            f"inbound queue full ({self._in_max}) - dropping data frame"
                        )
                    continue
                loop.call_soon_threadsafe(
                    self._in.put_nowait, (prio, next(self._in_seq), msg)
                )
        except FrameTooLarge as e:
            _log(f"protocol violation: {e} — treating host as gone")
            loop.call_soon_threadsafe(
                self._in.put_nowait, (2, next(self._in_seq), _EOF)
            )
        except OSError:
            loop.call_soon_threadsafe(
                self._in.put_nowait, (2, next(self._in_seq), _EOF)
            )

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
                # One deadline shared by every phase that follows: the
                # drain here, the post-loop drain, per-worker kills and
                # the writer flush/close all draw from it.
                self._deadline = time.monotonic() + grace
                self._tasks.deadline = self._deadline
                # Shutdown may have jumped ahead of frames already
                # received: those calls never registered, so cancel_all
                # cannot reach them - answer each one here or its
                # terminal is silently lost.
                while not self._in.empty():
                    p, _, pending = self._in.get_nowait()
                    if p == 1:
                        self._in_done += 1
                    if pending is _EOF or not isinstance(pending, dict):
                        continue
                    pid = pending.get("id")
                    if pid is None:
                        continue
                    if pending.get("method") == "cancel":
                        self._send(proto.response(pid, {"cancelled": True}))
                    else:
                        self._send(
                            proto.error_response(
                                pid,
                                proto.TASK_CANCELLED,
                                "runtime shutting down",
                            )
                        )
                await self._drain()
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
        await self._tasks.start(
            task_id,
            rid,
            fn_name,
            ex.kind,
            ex.fn,
            input,
            requires=ex.requires,
            worker=ex.worker,
            ret_schema=ex.ret_schema,
        )
        # Handlers emit the response: value calls on completion, stream
        # calls at the terminal message (protocol §3.1).

    # ---- lifecycle ---------------------------------------------------------

    async def _drain(self, grace_s: float | None = None) -> None:
        """Cancel tasks, then exit. Called on shutdown and stdin EOF.

        `grace_s` is this phase's own cap; a set shutdown deadline wins,
        so a slow earlier phase can never buy the later ones extra time.
        """
        try:
            await asyncio.wait_for(
                self._tasks.cancel_all(), timeout=self._remaining(grace_s)
            )
        except TimeoutError:
            _log("grace period expired during drain")
            for t in self._tasks.all():
                await self._tasks._kill_worker(
                    t
                )  # isolated workers must not be orphaned
        # Idle warm-pool members are not tasks — cancel_all never reaches
        # them. Reap them on both paths (0008 §1); kills draw from the
        # shared deadline via _budget.
        await self._tasks._reap_warm_pools()
        for t in self._tasks.all():
            if t.asyncio_task is not None:
                t.asyncio_task.cancel()

    def _finish(self, writer: threading.Thread) -> None:
        """Join the writer, then close the channel — or exit the process.

        A writer still inside write() after its join budget is on a pipe
        the host stopped draining: functionally gone. Closing the fd
        from this thread can then block indefinitely — on Windows the
        close waits on the pending synchronous write (#142). The host is
        gone and nothing more can be delivered, so leave the handle open
        and exit instead; the OS reaps it and the blocked writer with it.
        """
        writer.join(timeout=self._remaining(2.0) or 0)
        if writer.is_alive():
            _log("writer still blocked on a dead channel — leaving it open and exiting")
            sys.stderr.flush()
            _hard_exit(0)
            return  # unreachable in production; tests patch _hard_exit
        self._channel.close_write()

    async def _finish_executor(self) -> None:
        """Join the default executor within the shared budget.

        A cancelled inline-sync call leaves its executor thread running
        (docs/cancellation.md), and asyncio.run's teardown joins that
        executor unbounded (CPython 3.12) or for up to 300 s (3.15) —
        the same exit hang as a wedged channel, on a healthy pipe. Bound
        the join here; on expiry exit the same way, since those threads
        die with the process anyway.
        """
        loop = asyncio.get_running_loop()
        if getattr(loop, "_default_executor", None) is None:
            return
        try:
            await asyncio.wait_for(
                loop.shutdown_default_executor(),
                timeout=max(self._remaining(2.0) or 0, 0.05),
            )
        except TimeoutError:
            _log(
                "executor still running cancelled work past the "
                "shutdown budget — exiting without joining it"
            )
            sys.stderr.flush()
            _hard_exit(0)

    async def _startup(self) -> None:
        """Run the @on_startup hooks, then the end-of-startup sweep
        (decision 0007 §1). Shares the drain path: at shutdown this task
        is cancelled like lag/beat — async hooks cancel, sync executor
        hooks are discarded the same way inline sync calls are."""
        await _readiness.run_startup()

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
        # The first post-hello flush carries everything reported before
        # the server existed — import-time reports and requires=
        # declarations alike, one frame per component (0007 §4).
        _readiness.attach(self._send)
        reader.start()
        lag = asyncio.create_task(self._lag_monitor())
        beat = asyncio.create_task(self._loop_heartbeat())
        # Startup hooks run beside lag/beat, concurrent with dispatch —
        # calls are served while components still load (0007 §1).
        startup = asyncio.create_task(self._startup())
        # Warm-pool prewarming (0008 §6): spawn each prewarm pool's
        # first member and drive its ready handshake -> worker:<fn>
        # component. Beside the hooks, never blocking dispatch.
        prewarm = [
            asyncio.create_task(self._tasks._prewarm(name, ex.worker))
            for name, ex in exposed_functions().items()
            if ex.worker is not None
            and ex.worker.get("mode") == "warm"
            and ex.worker.get("prewarm")
        ]

        while not self._stopping:
            prio, _, msg = await self._in.get()
            if prio == 1:
                self._in_done += 1
            if msg is _EOF:
                _log("stdin EOF — host gone, draining")
                break
            await self._handle(msg)

        for t in (lag, beat, startup, *prewarm):
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
        self._finish(writer)
        await self._finish_executor()


def run(channel: FrameChannel) -> None:
    # Diagnostic for #142-class shutdown hangs: dump every thread's
    # stack to stderr on an interval. Off unless the env var carries
    # an interval in seconds.
    hang_probe = os.environ.get("PYLAIR_DEBUG_HANG")
    if hang_probe:
        import faulthandler

        faulthandler.dump_traceback_later(float(hang_probe), repeat=True)
    try:
        asyncio.run(Server(channel).run())
    except BaseException as e:
        _log(f"fatal: {type(e).__name__}: {e}\n{traceback.format_exc(limit=15)}")
        raise
