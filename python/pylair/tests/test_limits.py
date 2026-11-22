"""RC.8A: bounded-output and limit-error tests.

The e2e test at the bottom drives a real spawned runtime: a host that
grants credit and then never drains stdout is functionally gone — the
runtime must not accumulate frames without bound; it logs a distinct
reason and exits.
"""

from __future__ import annotations

import asyncio
import os
import queue
import subprocess
import sys
import threading
import time

import pytest

from conftest import _LEN, HERE
from pylair import _protocol as proto
from pylair import _server
from pylair._server import _EOF, _STOP, Server, _BoundedOut
from pylair._transport import MAX_FRAME, FrameChannel


class _FakeChannel:
    def __init__(self) -> None:
        self.written: list[bytes] = []

    def read_frame(self):
        return None

    def write_frame(self, body: bytes) -> None:
        self.written.append(body)

    def close_write(self) -> None:
        pass


# ---- _BoundedOut ------------------------------------------------------------


def test_bounded_out_frame_cap_raises_full():
    q = _BoundedOut(max_frames=2, max_bytes=1 << 30)
    q.put_nowait(b"a")
    q.put_nowait(b"b")
    with pytest.raises(queue.Full):
        q.put_nowait(b"c")


def test_bounded_out_byte_cap_raises_full():
    q = _BoundedOut(max_frames=1000, max_bytes=8)
    q.put_nowait(b"12345")
    with pytest.raises(queue.Full):
        q.put_nowait(b"4567")  # 5+4 > 8


def test_bounded_out_close_always_delivers_stop():
    q = _BoundedOut(max_frames=1, max_bytes=1 << 30)
    q.put_nowait(b"x")
    q.close()  # sentinel bypasses the cap — shutdown must be deliverable
    assert q.get() == b"x"
    assert q.get() is _STOP


# ---- Server._send ------------------------------------------------------------


def _server_with_loop() -> tuple[Server, asyncio.AbstractEventLoop, threading.Thread]:
    loop = asyncio.new_event_loop()
    t = threading.Thread(target=loop.run_forever, daemon=True)
    t.start()
    s = Server(_FakeChannel())
    s._loop = loop
    return s, loop, t


def test_send_full_marks_out_full_and_injects_eof(capsys):
    s, loop, _t = _server_with_loop()
    s._out = _BoundedOut(max_frames=1, max_bytes=1 << 30)
    s._send({"jsonrpc": "2.0", "method": "pong", "params": {"seq": 1}})
    # Second frame does not fit: _send must NOT block — it flags the
    # distinct reason and pushes the host-gone sentinel to the loop.
    s._send({"jsonrpc": "2.0", "method": "pong", "params": {"seq": 2}})
    assert s._out_full is True

    async def get():
        return await asyncio.wait_for(s._in.get(), 5)

    fut = asyncio.run_coroutine_threadsafe(get(), loop)
    # Queue items are (prio, seq, msg) tuples - the sentinel rides at 2.
    assert fut.result(timeout=5)[2].__class__.__name__ == "object"  # _EOF sentinel
    loop.call_soon_threadsafe(loop.stop)
    err = capsys.readouterr().err
    assert "outbound queue full" in err
    assert "host not draining" in err


def test_oversized_response_becomes_limit_error():
    s, loop, _t = _server_with_loop()
    s._send({"jsonrpc": "2.0", "id": 7, "result": "x" * (MAX_FRAME + 1)})
    body = s._out.get()
    import json as _j

    msg = _j.loads(body)
    assert msg["id"] == 7
    assert msg["error"]["code"] == proto.LIMIT_EXCEEDED
    assert msg["error"]["data"]["limit"] == "max_frame"
    loop.call_soon_threadsafe(loop.stop)


def test_oversized_notification_is_dropped_to_out():
    # Notifications have no caller to report to — the oversized frame is
    # still bounded by the byte cap (this is the documented asymmetry: a
    # chunk stream relies on the host-side caps to bound sizes).
    s, loop, _t = _server_with_loop()
    big = {
        "jsonrpc": "2.0",
        "method": "stream.chunk",
        "params": {"task": 1, "seq": 0, "value": "v"},
    }
    s._send(big)
    body = s._out.get()
    assert b"stream.chunk" in body
    loop.call_soon_threadsafe(loop.stop)


# ---- e2e: stalled host -------------------------------------------------------


def test_stalled_host_exits_with_distinct_reason():
    """Host grants credit then never drains stdout: the runtime's outbound
    queue fills, it logs the distinct reason and exits (never blocks the
    event loop)."""
    env = dict(os.environ)
    env.pop("PYTHONPATH", None)
    # Tiny cap so the flood fills it quickly; the byte cap stays large.
    env["PYLAIR_OUT_MAX_FRAMES"] = "32"
    env["PYLAIR_OUT_MAX_BYTES"] = str(1 << 30)
    # Credit headroom far above any realistic OS pipe buffer (~1.3 MiB of
    # chunks): if the pipe ever swallowed the whole flood, the queue would
    # never fill and the "stalled host" path could not be exercised.
    env["PYLAIR_MAX_STREAM_CREDIT"] = "32768"
    # Diagnostic (#142): if the runtime still hangs, every thread's stack
    # lands on stderr — the tail below shows where it actually is.
    env["PYLAIR_DEBUG_HANG"] = "15"
    proc = subprocess.Popen(
        [sys.executable, "-m", "pylair", "testapp"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        cwd=HERE,
        env=env,
    )
    try:
        stderr_lines: list[str] = []
        threading.Thread(
            target=lambda: [
                stderr_lines.append(l.decode(errors="replace")) for l in proc.stderr
            ],
            daemon=True,
        ).start()

        # hello frame, then a stream call + generous credit — stdout is
        # intentionally never read after this point.
        hdr = proc.stdout.read(4)
        (n,) = _LEN.unpack(hdr)
        proc.stdout.read(n)
        call = proto.request(
            1, "call", {"fn": "infinite_counted", "task": 1, "input": {}}
        )
        for msg in (
            call,
            proto.notification("stream.credit", {"task": 1, "n": 16384}),
        ):
            body = __import__("json").dumps(msg).encode()
            proc.stdin.write(_LEN.pack(len(body)) + body)
            proc.stdin.flush()

        try:
            proc.wait(timeout=90)
        except subprocess.TimeoutExpired:
            raise AssertionError(
                "runtime did not exit on stalled host; stderr tail:\n"
                + "".join(stderr_lines)[-2000:]
            )
        joined = "".join(stderr_lines)
        assert "outbound queue full" in joined, joined[-2000:]
    finally:
        if proc.poll() is None:
            proc.kill()


# ---- dead-channel / executor exit (#142) --------------------------------------


def test_dead_channel_writer_exits_without_closing(monkeypatch):
    """A writer mid-write on a pipe nobody drains must not be closed from
    the main thread — on Windows the close waits on the pending write and
    deadlocks the exit (#142). _finish bounds the join and exits instead."""
    rfd, wfd = os.pipe()
    ch = FrameChannel(rfd, wfd)  # the read end is deliberately never drained
    s = Server(ch)
    # A frame far bigger than any pipe buffer leaves the writer blocked
    # inside write() mid-frame on every OS.
    s._out.put_nowait(b"x" * (1 << 20))
    w = threading.Thread(target=s._writer, name="tp-writer", daemon=True)
    w.start()
    deadline = time.time() + 5
    while s._out.stats()[0] and time.time() < deadline:
        time.sleep(0.01)  # wait until the frame is dequeued into write()
    assert w.is_alive()
    s._out.close()  # _STOP lands behind the frame the writer still holds
    s._deadline = time.monotonic() + 0.05  # shrink the join budget

    exits = []
    monkeypatch.setattr(_server, "_hard_exit", exits.append)
    t0 = time.monotonic()
    s._finish(w)
    assert time.monotonic() - t0 < 5
    assert exits == [0]
    assert w.is_alive()  # still wedged — but skipped, not closed
    assert not ch._w.closed

    # Closing the read end breaks the pipe: write() fails and the daemon
    # writer exits, so the test leaks no blocked thread. Close through
    # the channel's own FileIO so its state stays consistent.
    ch._r.close()
    w.join(timeout=5)
    assert not w.is_alive()


def test_stuck_executor_exits_instead_of_hanging_teardown(monkeypatch):
    """A cancelled inline-sync call leaves its executor thread running;
    asyncio.run's teardown would join it unbounded (3.12) or for 300 s
    (3.15). _finish_executor bounds the join and exits instead (#142)."""
    started = threading.Event()
    gate = threading.Event()

    def work():
        started.set()
        gate.wait(30)

    exits = []
    monkeypatch.setattr(_server, "_hard_exit", exits.append)

    async def go():
        s = Server(_FakeChannel())
        s._deadline = time.monotonic() + 0.05
        loop = asyncio.get_running_loop()
        loop.run_in_executor(None, work)
        # Wait until the work item is actually running on a thread —
        # _finish_executor skips only an executor that was never used.
        while not started.is_set():
            await asyncio.sleep(0.01)
        await s._finish_executor()

    loop = asyncio.new_event_loop()
    try:
        t0 = time.monotonic()
        loop.run_until_complete(go())  # an asyncio.run teardown would hang here
        assert time.monotonic() - t0 < 5
        assert exits == [0]
    finally:
        gate.set()  # release the work, then close the loop normally
        loop.run_until_complete(loop.shutdown_default_executor())
        loop.close()


# ---- inbound bound: control never waits behind data ---------------------------


def test_inbound_flood_rejects_over_bound_but_never_stalls_control():
    """IN_MAX is a real bound: data frames past it get the standard
    LIMIT_EXCEEDED shape, while control frames sort ahead of the queued
    backlog - a cancel and a shutdown written behind the flood are still
    handled promptly."""
    from conftest import Runtime

    env = {"PYLAIR_IN_MAX": "4"}
    with Runtime(env=env) as rt:
        rt.recv()  # hello
        # Wedge the event loop so the inbound queue really fills.
        rt.send(
            proto.request(
                1, "call", {"fn": "loop_block", "task": 1, "input": {"seconds": 1.5}}
            )
        )
        # Flood data frames while the loop is blocked.
        for i in range(2, 40):
            rt.send(
                proto.request(
                    i, "call", {"fn": "add", "task": i, "input": {"a": 1, "b": 1}}
                )
            )
        # Control frames arrive behind the flood in the same pipe.
        rt.send(proto.request(100, "cancel", {"task": 1}))
        rt.send(proto.request(101, "shutdown", {"grace_ms": 4000}))

        saw_limit = got_cancel = got_shutdown = False
        deadline = time.time() + 15
        while time.time() < deadline and not got_shutdown:
            msg = rt.recv(timeout=max(0.1, deadline - time.time()))
            if msg.get("id") == 100:
                got_cancel = True
            elif msg.get("id") == 101:
                got_shutdown = True
            err = msg.get("error") or {}
            if (
                err.get("code") == proto.LIMIT_EXCEEDED
                and (err.get("data") or {}).get("limit") == "inbound_queue"
            ):
                saw_limit = True
        assert saw_limit, "flooded data never hit the inbound bound"
        assert got_cancel, "cancel ack never arrived behind the flood"
        assert got_shutdown, "shutdown ack never arrived behind the flood"
        rt.proc.wait(timeout=10)
        assert rt.proc.returncode == 0


# ---- depth cap (_send) --------------------------------------------------------


def _nest(n: int):
    out = 1
    for _ in range(n):
        out = {"d": out}
    return out


def test_deep_response_becomes_limit_error():
    s, loop, _t = _server_with_loop()
    s._send({"jsonrpc": "2.0", "id": 7, "result": _nest(200)})
    body = s._out.get()
    import json as _j

    msg = _j.loads(body)
    assert msg["id"] == 7
    assert msg["error"]["code"] == proto.LIMIT_EXCEEDED
    assert msg["error"]["data"]["limit"] == "max_depth"
    loop.call_soon_threadsafe(loop.stop)


def test_deep_notification_is_dropped_not_queued(capsys):
    # A notification has no caller to report to: dropping it (with a log)
    # beats writing a frame the host cannot parse.
    s, loop, _t = _server_with_loop()
    s._send(
        {
            "jsonrpc": "2.0",
            "method": "stream.chunk",
            "params": {"task": 1, "seq": 0, "value": _nest(200)},
        }
    )
    assert s._out.stats()[0] == 0
    loop.call_soon_threadsafe(loop.stop)
    assert "depth" in capsys.readouterr().err


def test_inbound_counters_converge_under_threaded_flood():
    """_in_read and _in_done each have a single writer: a reader-thread
    flood with dispatch interleaved must converge exactly. A shared
    +=/-= counter can lose an update across threads and leave the bound
    permanently tripped on an idle runtime."""
    import json as _j

    N = 20_000
    frames = [
        _j.dumps(
            {
                "jsonrpc": "2.0",
                "id": i,
                "method": "call",
                "params": {"fn": "noop", "task": i},
            }
        ).encode()
        for i in range(1, N + 1)
    ]

    class _FloodChannel(_FakeChannel):
        def __init__(self, bodies):
            super().__init__()
            self._it = iter(bodies)

        def read_frame(self):
            return next(self._it, None)

    class _SinkOut:
        # Reject responses pour in here; the bound under test is inbound.
        def put_nowait(self, _b):
            pass

    s = Server(_FloodChannel(frames))
    s._in_max = 64  # small, so the bound really does trip mid-flood
    s._out = _SinkOut()  # type: ignore[assignment]

    async def drain():
        loop = asyncio.get_running_loop()
        t = threading.Thread(target=s._reader, args=(loop,), daemon=True)
        t.start()
        popped = 0
        while True:
            prio, _, msg = await s._in.get()
            if prio == 1:
                s._in_done += 1
                popped += 1
                if popped % 50 == 0:
                    await asyncio.sleep(0)  # let the reader run ahead
            if msg is _EOF:
                break
        t.join(timeout=5)

    asyncio.run(drain())
    assert s._in_read == s._in_done
    # The predicate the reader evaluates: a fresh data frame is accepted.
    assert s._in_read - s._in_done < s._in_max
