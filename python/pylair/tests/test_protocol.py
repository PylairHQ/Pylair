"""End-to-end protocol tests against a real spawned runtime.

Each test gets a fresh runtime via the `runtime` fixture (conftest.py).
Frames are asserted at the wire level — including the stdout-hygiene and
credit-flow guarantees.
"""

import os
import queue
import signal
import struct
import subprocess
import sys
import time

import pytest

from pylair import _protocol as proto

_LEN = struct.Struct(">I")


def call(
    rid: int,
    fn: str,
    task: int,
    input: dict | None = None,
    stream: bool | None = None,
) -> dict:
    params = {"fn": fn, "task": task, "input": input or {}}
    if stream is not None:
        params["stream"] = stream
    return proto.request(rid, "call", params)


def grant(task: int, n: int = proto.INITIAL_STREAM_CREDIT) -> dict:
    """The host's initial window — it owns the grant (protocol §3.1)."""
    return proto.notification("stream.credit", {"task": task, "n": n})


# ---- handshake ------------------------------------------------------------


def test_hello_is_first_frame_and_lists_exposes(runtime):
    hello = runtime.recv()
    assert hello["method"] == "hello"
    p = hello["params"]
    assert p["protocol"] == proto.PROTOCOL_VERSION
    names = {e["name"]: e["kind"] for e in p["exposes"]}
    assert names["add"] == "sync"
    assert names["echo_later"] == "async"
    assert names["counter"] == "generator"
    assert names["isolated_add"] == "isolated"


# ---- calls ----------------------------------------------------------------


def test_sync_call(runtime):
    runtime.recv()  # hello
    runtime.send(call(1, "add", task=1, input={"a": 2, "b": 3}))
    resp = runtime.recv()
    assert resp == {"jsonrpc": "2.0", "id": 1, "result": 5}


def test_async_call(runtime):
    runtime.recv()
    runtime.send(call(1, "echo_later", task=1, input={"text": "hi"}))
    assert runtime.recv()["result"] == "hi"


def test_unknown_function(runtime):
    runtime.recv()
    runtime.send(call(1, "nope", task=1))
    resp = runtime.recv()
    assert resp["error"]["code"] == proto.FUNCTION_NOT_FOUND


def test_malformed_frame(runtime):
    runtime.recv()
    body = b"this is not json"
    runtime.send_raw(_LEN.pack(len(body)) + body)
    resp = runtime.recv()
    assert resp["error"]["code"] == proto.PARSE_ERROR


def test_print_and_fd1_writes_cannot_corrupt_frames(runtime):
    """Stray stdout writes land on stderr; the next frame still parses."""
    runtime.recv()
    runtime.send(call(1, "print_then_answer", task=1, input={"text": "ok"}))
    resp = runtime.recv()
    assert resp["result"] == "ok"
    # and the stream is still clean for the NEXT call
    runtime.send(call(2, "add", task=2, input={"a": 1, "b": 1}))
    assert runtime.recv()["result"] == 2


def test_ping_pong(runtime):
    runtime.recv()
    runtime.send(proto.request(7, "ping", {"seq": 42}))
    msg = runtime.recv()
    assert msg["method"] == "pong" and msg["params"]["seq"] == 42
    # additive field (protocol §5): how long the asyncio loop has been stuck
    assert isinstance(msg["params"]["loop_stale_ms"], int)


# ---- streaming + credit ----------------------------------------------------


def test_stream_terminal_is_the_call_response(runtime):
    runtime.recv()
    runtime.send(call(9, "counter", task=5, input={"n": 3}))
    runtime.send(grant(5))
    chunks = [
        runtime.recv_until(lambda m: m.get("method") == "stream.chunk")
        for _ in range(3)
    ]
    assert [c["params"]["value"] for c in chunks] == [0, 1, 2]
    assert [c["params"]["seq"] for c in chunks] == [0, 1, 2]
    terminal = runtime.recv_until(lambda m: m.get("id") == 9)
    assert terminal["result"] == {"done": True}


def test_producer_runs_only_with_credit(runtime):
    """An infinite generator must pause at the initial credit boundary."""
    runtime.recv()
    runtime.send(call(1, "infinite", task=1))
    runtime.send(grant(1))
    got = 0
    # the host granted INITIAL_STREAM_CREDIT; drain exactly that many
    for _ in range(proto.INITIAL_STREAM_CREDIT):
        runtime.recv_until(lambda m: m.get("method") == "stream.chunk", timeout=5)
        got += 1
    # now no more chunks should arrive without credit
    with pytest.raises(queue.Empty):
        runtime.recv(timeout=1.5)
    # grant 5 more -> exactly 5 more chunks, then silence again
    runtime.send(proto.notification("stream.credit", {"task": 1, "n": 5}))
    for _ in range(5):
        runtime.recv_until(lambda m: m.get("method") == "stream.chunk", timeout=5)
        got += 1
    with pytest.raises(queue.Empty):
        runtime.recv(timeout=1.5)
    assert got == proto.INITIAL_STREAM_CREDIT + 5


# ---- cancellation -----------------------------------------------------------


def test_cancel_async_task(runtime):
    runtime.recv()
    runtime.send(call(1, "sleep_forever", task=1))
    runtime.send(proto.request(2, "cancel", {"task": 1}))
    # cancel ack may interleave with the task's terminal response
    ack = runtime.recv_until(lambda m: m.get("id") == 2)
    assert ack["result"]["cancelled"] is True
    terminal = runtime.recv_until(lambda m: m.get("id") == 1)
    assert terminal["error"]["code"] == proto.TASK_CANCELLED


def test_cancel_unknown_task(runtime):
    runtime.recv()
    runtime.send(proto.request(1, "cancel", {"task": 999}))
    resp = runtime.recv()
    # An unknown id is recorded as a pending cancel — under supervisor
    # routing it can only mean the call frame is still in flight.
    assert resp["result"]["cancelled"] is True
    assert resp["result"]["status"] == "pending"


def test_cancel_stream(runtime):
    runtime.recv()
    runtime.send(call(7, "infinite", task=7))
    runtime.send(grant(7))
    for _ in range(5):
        runtime.recv_until(lambda m: m.get("method") == "stream.chunk")
    runtime.send(proto.request(8, "cancel", {"task": 7}))
    terminal = runtime.recv_until(lambda m: m.get("id") == 7)
    assert terminal["error"]["code"] == proto.TASK_CANCELLED


def test_cancel_before_registration_still_terminates(runtime):
    """A cancel that lands BEFORE its call (control frames can overtake
    the data lane) is held in _pending_cancels and applied at
    registration. A task cancelled before its first step never enters a
    handler — without the finisher the call's terminal would be lost.
    RC.8B found this exact hang."""
    runtime.recv()
    # cancel first, call second — both queued in the same pipe order.
    runtime.send(proto.request(9, "cancel", {"task": 9}))
    runtime.send(call(3, "sleep_forever", task=9))
    # The cancel ack reports pending — the cancel is recorded and will
    # apply at registration, so the honest outcome is cancelled...
    ack = runtime.recv_until(lambda m: m.get("id") == 9)
    assert ack["result"]["cancelled"] is True
    assert ack["result"]["status"] == "pending"
    # ...but the call's terminal must still arrive, and it must be the
    # cancelled outcome — never a hang or a completed result.
    terminal = runtime.recv_until(lambda m: m.get("id") == 3)
    assert terminal["error"]["code"] == proto.TASK_CANCELLED


def test_early_cancel_survives_pending_set_eviction(runtime):
    """More than 4096 cancels for already-seen ids must not evict a
    genuinely-early cancel: ids below the high-water mark are answered
    task_not_found, never stored, so the pending set only holds ids
    greater than every call handled so far."""
    runtime.recv()
    # Establish the high-water mark with one real call.
    runtime.send(call(1, "add", task=100_000, input={"a": 1, "b": 1}))
    assert runtime.recv_until(lambda m: m.get("id") == 1)["result"] == 2
    # Flood cancels for ids below the mark — each must answer
    # task_not_found and none may occupy the pending set.
    for i in range(4100):
        runtime.send(proto.request(50_000 + i, "cancel", {"task": 10_000 + i}))
    # The genuinely-early cancel: the task's call hasn't arrived yet.
    runtime.send(proto.request(9, "cancel", {"task": 200_000}))
    ack = runtime.recv_until(lambda m: m.get("id") == 9, timeout=30)
    assert ack["result"]["cancelled"] is True
    assert ack["result"]["status"] == "pending"
    # The call arrives after its cancel — it must end TASK_CANCELLED,
    # not run to completion because the pending cancel was evicted.
    runtime.send(call(2, "sleep_forever", task=200_000))
    terminal = runtime.recv_until(lambda m: m.get("id") == 2, timeout=15)
    assert terminal["error"]["code"] == proto.TASK_CANCELLED


def test_early_credit_survives_pending_set_eviction(runtime):
    """Same shape for stream credit: grants for ids below the mark are
    dropped, so a grant that arrives before its call still lands."""
    runtime.recv()
    runtime.send(call(1, "add", task=100_000, input={"a": 1, "b": 1}))
    assert runtime.recv_until(lambda m: m.get("id") == 1)["result"] == 2
    for i in range(4100):
        runtime.send(grant(10_000 + i, 1))
    # Grant past n so the pump can reach StopAsyncIteration for the
    # terminal, not stall on zero credit after the third chunk.
    runtime.send(grant(200_001, 8))
    runtime.send(call(2, "counter", task=200_001, input={"n": 3}))
    chunks = [
        runtime.recv_until(
            lambda m: (
                m.get("method") == "stream.chunk"
                and m.get("params", {}).get("task") == 200_001
            ),
            timeout=15,
        )
        for _ in range(3)
    ]
    assert [c["params"]["value"] for c in chunks] == [0, 1, 2]
    terminal = runtime.recv_until(lambda m: m.get("id") == 2, timeout=10)
    assert terminal["result"] == {"done": True}


def test_isolated_call(runtime):
    runtime.recv()
    runtime.send(call(1, "isolated_add", task=1, input={"a": 4, "b": 5}))
    assert runtime.recv()["result"] == 9


def test_isolated_worker_inherits_release_env():
    """rc.15 H: PYTHONDONTWRITEBYTECODE=1 set by the release spawn spec
    must be visible INSIDE an isolated call — workers are spawned by the
    runtime with os.environ, so the flag reaches them automatically."""
    from conftest import Runtime

    with Runtime(env={"PYTHONDONTWRITEBYTECODE": "1"}) as rt:
        rt.recv()  # hello
        rt.send(call(1, "isolated_env_flag", task=1))
        resp = rt.recv()
        assert "result" in resp, resp
        assert resp["result"]["PYTHONDONTWRITEBYTECODE"] == "1"
        assert resp["result"]["dont_write_bytecode"] is True


def test_isolated_fn_in_app_submodule_resolves():
    """An isolated function may live in a submodule the app imports —
    the worker resolves it through the registry, not getattr on the app
    module (testapp_subapp does not re-export sub_ping by name)."""
    from conftest import Runtime

    with Runtime("testapp_subapp") as rt:
        rt.recv()  # hello
        rt.send(call(1, "sub_ping", task=1, input={"text": "hi"}))
        resp = rt.recv()
        assert "result" in resp, resp
        assert resp["result"] == {"echoed": "hi", "via": "testapp_pkg.sub"}


def test_isolated_print_does_not_corrupt_result(runtime):
    runtime.recv()
    runtime.send(call(1, "isolated_print", task=1, input={"text": "hi"}))
    resp = runtime.recv()
    assert resp["result"] == "hi", resp


def test_isolated_call_and_cancel_kills_worker(runtime):
    runtime.recv()
    runtime.send(call(1, "isolated_sleep", task=1, input={"seconds": 60}))
    time.sleep(1.0)  # let the worker spawn
    t0 = time.time()
    runtime.send(proto.request(2, "cancel", {"task": 1}))
    # The ack now reports the actual kill outcome, so it waits for the
    # terminate/reap path — the task's own terminal can arrive first.
    # Collect both regardless of order.
    ack = terminal = None
    deadline = time.time() + 15
    while ack is None or terminal is None:
        m = runtime.recv(timeout=max(0.1, deadline - time.time()))
        if m.get("id") == 2:
            ack = m
        elif m.get("id") == 1:
            terminal = m
    elapsed = time.time() - t0
    assert ack["result"]["cancelled"] is True
    assert ack["result"]["status"] == "terminated", ack
    assert terminal["error"]["code"] == proto.TASK_CANCELLED
    assert elapsed < 10, "worker should be terminated promptly, not at 60s"


@pytest.mark.skipif(
    sys.platform == "win32", reason="process-group kill semantics are unix-specific"
)
def test_cancel_isolated_kills_sigterm_immune_grandchild(runtime, tmp_path):
    """A grandchild ignoring SIGTERM must still die: group SIGKILL."""
    runtime.recv()
    pidfile = tmp_path / "grandchild.pid"
    runtime.send(call(1, "orphan_maker", task=1, input={"pidfile": str(pidfile)}))
    deadline = time.time() + 15
    while not pidfile.exists():
        assert time.time() < deadline, "grandchild never spawned"
        time.sleep(0.1)
    gpid = int(pidfile.read_text().strip())
    os.kill(gpid, 0)  # alive

    runtime.send(proto.request(2, "cancel", {"task": 1}))
    terminal = runtime.recv_until(lambda m: m.get("id") == 1)
    assert terminal["error"]["code"] == proto.TASK_CANCELLED

    deadline = time.time() + 5
    while time.time() < deadline:
        try:
            os.kill(gpid, 0)
        except ProcessLookupError:
            break
        time.sleep(0.1)
    else:
        pytest.fail(f"grandchild {gpid} survived the group kill")


@pytest.mark.skipif(
    sys.platform == "win32", reason="SIGTERM-grace semantics are posix-specific"
)
def test_isolated_cancel_report_does_not_block_dispatch(runtime, tmp_path):
    """A worker ignoring SIGTERM takes the full grace window to die. The
    cancel's waiting half must not sit on the dispatch lane: a fast call
    sent right after the cancel answers inside 1s while the kill is
    still being awaited."""
    runtime.recv()
    ready = tmp_path / "armed"
    runtime.send(call(1, "sigterm_immune", task=1, input={"ready_file": str(ready)}))
    deadline = time.time() + 15
    while not ready.exists():
        assert time.time() < deadline, "worker never armed"
        time.sleep(0.05)

    t0 = time.time()
    runtime.send(proto.request(2, "cancel", {"task": 1}))
    runtime.send(call(3, "add", task=3, input={"a": 1, "b": 1}))
    # The terminal and the cancel ack land together once the kill
    # completes — collect all three frames without discarding any.
    resp = ack = terminal = None
    t_resp = None
    deadline = t0 + 15
    while resp is None or ack is None or terminal is None:
        m = runtime.recv(timeout=max(0.1, deadline - time.time()))
        if m.get("id") == 3:
            resp, t_resp = m, time.time()
        elif m.get("id") == 2:
            ack = m
        elif m.get("id") == 1:
            terminal = m
    assert t_resp - t0 < 1.0, (
        f"dispatch stalled {t_resp - t0:.2f}s behind an in-flight isolated cancel"
    )
    assert resp["result"] == 2

    # The cancel still reports the real outcome once the kill lands.
    assert ack["result"]["cancelled"] is True
    assert ack["result"]["status"] == "terminated"
    assert terminal["error"]["code"] == proto.TASK_CANCELLED


@pytest.mark.skipif(
    sys.platform == "win32", reason="SIGTERM-grace semantics are posix-specific"
)
def test_two_isolated_cancels_report_concurrently(runtime, tmp_path):
    """Two slow isolated cancels must overlap: neither ack may be delayed
    by the other's terminate/reap wait."""
    runtime.recv()
    r1, r2 = tmp_path / "r1", tmp_path / "r2"
    runtime.send(call(1, "sigterm_immune", task=1, input={"ready_file": str(r1)}))
    runtime.send(call(2, "sigterm_immune", task=2, input={"ready_file": str(r2)}))
    deadline = time.time() + 15
    while not (r1.exists() and r2.exists()):
        assert time.time() < deadline, "workers never armed"
        time.sleep(0.05)

    t0 = time.time()
    runtime.send(proto.request(3, "cancel", {"task": 1}))
    runtime.send(proto.request(4, "cancel", {"task": 2}))
    t3 = t4 = None
    deadline = t0 + 15
    while t3 is None or t4 is None:
        m = runtime.recv(timeout=max(0.1, deadline - time.time()))
        if m.get("id") == 3:
            t3 = time.time()
        elif m.get("id") == 4:
            t4 = time.time()
    assert t3 - t0 < 5.0 and t4 - t0 < 5.0, (
        f"acks landed {t3 - t0:.2f}s / {t4 - t0:.2f}s after cancel — "
        "one waited behind the other"
    )
    assert abs(t3 - t4) < 1.5, f"acks {abs(t3 - t4):.2f}s apart — serialized"


@pytest.mark.skipif(sys.platform == "win32", reason="posix process groups")
def test_runtime_killed_hard_reaps_isolated_worker_and_grandchildren(runtime, tmp_path):
    """Workers live in their own session, outside the runtime's process
    group, so a forced kill of the runtime (what the host does) cannot reach
    them. The worker must reap its own group when its parent disappears."""
    runtime.recv()
    pidfile = tmp_path / "grandchild.pid"
    runtime.send(call(1, "orphan_maker", task=1, input={"pidfile": str(pidfile)}))
    deadline = time.time() + 15
    while not pidfile.exists():
        assert time.time() < deadline, "grandchild never spawned"
        time.sleep(0.1)
    gpid = int(pidfile.read_text().strip())
    os.kill(gpid, 0)  # alive, and it ignores SIGTERM

    runtime.proc.kill()  # SIGKILL the runtime only — no cleanup runs
    runtime.proc.wait(timeout=10)

    deadline = time.time() + 10
    while time.time() < deadline:
        try:
            os.kill(gpid, 0)
        except ProcessLookupError:
            return
        time.sleep(0.2)
    os.kill(gpid, signal.SIGKILL)  # don't leak it from the test itself
    pytest.fail(f"grandchild {gpid} outlived a hard kill of the runtime")


# ---- lifecycle --------------------------------------------------------------


def test_stdin_eof_exits_cleanly(runtime):
    runtime.recv()
    runtime.close_stdin()
    rc = runtime.proc.wait(timeout=10)
    assert rc == 0


def test_oversized_frame_rejected():
    """A >MAX_FRAME length prefix must not allocate/read that much."""
    from pathlib import Path

    proc = subprocess.Popen(
        [sys.executable, "-m", "pylair", "testapp"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        cwd=Path(__file__).parent,
    )
    (hello_len,) = struct.unpack(">I", proc.stdout.read(4))
    proc.stdout.read(hello_len)
    proc.stdin.write(_LEN.pack(64 * 1024 * 1024))  # oversize len, no body
    proc.stdin.flush()
    proc.stdin.close()
    # runtime should exit (violation -> EOF path), not hang waiting for 64 MiB
    rc = proc.wait(timeout=10)
    assert rc is not None


def test_shutdown_emits_ack_and_task_terminals(runtime):
    """Shutdown while a stream is live: the ack AND a TASK_CANCELLED for the
    task must both reach the wire before the process exits. Regression: the
    drain used to close the frame fd before the writer thread flushed, and
    cancel_all never gave the terminal-emitting finishers a loop turn —
    every final frame was silently dropped."""
    runtime.recv()  # hello
    runtime.send(call(1, "infinite_counted", task=7))
    runtime.send(proto.request(2, "shutdown", None))

    msgs = []
    deadline = time.time() + 10
    while time.time() < deadline:
        try:
            msgs.append(runtime.recv(timeout=max(0.1, deadline - time.time())))
        except queue.Empty:
            break
        if "__io_error__" in msgs[-1]:
            break
    runtime.proc.wait(timeout=10)

    by_id = {m.get("id"): m for m in msgs if "id" in m}
    assert by_id[2].get("result") == {}, "shutdown ack missing or wrong"
    err = by_id[1].get("error") or {}
    assert err.get("code") == proto.TASK_CANCELLED, (
        f"stream terminal across shutdown was {msgs!r}"
    )


# ---- depth cap ---------------------------------------------------------------
# serde_json on the host refuses frames nested deeper than 128, so a
# legitimate deep Python result is indistinguishable from a malformed
# frame — the runtime must check depth itself and fail only that call.


def _nest(n: int):
    out = 1
    for _ in range(n):
        out = {"d": out}
    return out


def test_deep_result_fails_only_that_call(runtime):
    runtime.recv()  # hello
    runtime.send(call(1, "deep", task=1, input={"n": 200}))
    runtime.send(call(2, "add", task=2, input={"a": 2, "b": 3}))
    err = runtime.recv_until(lambda m: m.get("id") == 1)
    assert err["error"]["code"] == proto.LIMIT_EXCEEDED
    assert err["error"]["data"]["limit"] == "max_depth"
    assert runtime.recv_until(lambda m: m.get("id") == 2)["result"] == 5
    runtime.send(call(3, "add", task=3, input={"a": 1, "b": 1}))
    assert runtime.recv_until(lambda m: m.get("id") == 3)["result"] == 2


def test_deep_stream_chunk_fails_only_the_stream(runtime):
    runtime.recv()
    runtime.send(call(4, "deep_stream", task=4, input={"n": 200}))
    runtime.send(grant(4))
    terminal = runtime.recv_until(lambda m: m.get("id") == 4)
    assert terminal["error"]["code"] == proto.LIMIT_EXCEEDED
    runtime.send(call(5, "add", task=5, input={"a": 1, "b": 2}))
    assert runtime.recv_until(lambda m: m.get("id") == 5)["result"] == 3


def test_deep_isolated_result_fails_only_that_call(runtime):
    runtime.recv()
    runtime.send(call(6, "isolated_deep", task=6, input={"n": 200}))
    err = runtime.recv_until(lambda m: m.get("id") == 6, timeout=30)
    assert err["error"]["code"] == proto.LIMIT_EXCEEDED
    runtime.send(call(7, "add", task=7, input={"a": 2, "b": 2}))
    assert runtime.recv_until(lambda m: m.get("id") == 7)["result"] == 4


# ---- input validation --------------------------------------------------------
# Wrong primitives used to reach the function body (add("x","y") -> "xy");
# missing/extra keys used to surface as an INTERNAL TypeError. Both are now
# INVALID_PARAMS at the boundary, on every call mode including isolated.


def test_wrong_primitive_is_invalid_params_and_body_never_runs(runtime):
    runtime.recv()  # hello
    runtime.send(call(1, "add", task=1, input={"a": "x", "b": "y"}))
    err = runtime.recv_until(lambda m: m.get("id") == 1)
    assert err["error"]["code"] == proto.INVALID_PARAMS
    assert "'a'" in err["error"]["message"]

    runtime.send(call(2, "spy", task=2, input={"a": "not-an-int"}))
    assert runtime.recv_until(lambda m: m.get("id") == 2)["error"]["code"] == (
        proto.INVALID_PARAMS
    )
    runtime.send(call(3, "spy_count", task=3))
    assert runtime.recv_until(lambda m: m.get("id") == 3)["result"] == 0


def test_whole_float_for_int_ok_fraction_and_bool_rejected(runtime):
    runtime.recv()
    runtime.send(call(1, "add", task=1, input={"a": 3.0, "b": 2}))
    assert runtime.recv_until(lambda m: m.get("id") == 1)["result"] == 5
    runtime.send(call(2, "add", task=2, input={"a": 3.5, "b": 2}))
    assert runtime.recv_until(lambda m: m.get("id") == 2)["error"]["code"] == (
        proto.INVALID_PARAMS
    )
    runtime.send(call(3, "add", task=3, input={"a": True, "b": 2}))
    assert runtime.recv_until(lambda m: m.get("id") == 3)["error"]["code"] == (
        proto.INVALID_PARAMS
    )


def test_missing_and_extra_keys_are_invalid_params(runtime):
    runtime.recv()
    runtime.send(call(1, "add", task=1, input={"a": 1}))
    err = runtime.recv_until(lambda m: m.get("id") == 1)
    assert err["error"]["code"] == proto.INVALID_PARAMS
    assert "'b'" in err["error"]["message"]
    runtime.send(call(2, "add", task=2, input={"a": 1, "b": 2, "c": 3}))
    err = runtime.recv_until(lambda m: m.get("id") == 2)
    assert err["error"]["code"] == proto.INVALID_PARAMS
    assert "'c'" in err["error"]["message"]


def test_optional_and_union_arms(runtime):
    runtime.recv()
    runtime.send(call(1, "pick", task=1, input={"c": "red"}))
    assert runtime.recv_until(lambda m: m.get("id") == 1)["result"] == "red"
    # `fallback: str | None` is the Optional arm — null is legal for it.
    runtime.send(call(2, "pick", task=2, input={"c": "blue", "fallback": None}))
    assert runtime.recv_until(lambda m: m.get("id") == 2)["result"] == "blue"
    # `c: Color` is NOT Optional — null is invalid for it.
    runtime.send(call(6, "pick", task=6, input={"c": None}))
    assert runtime.recv_until(lambda m: m.get("id") == 6)["error"]["code"] == (
        proto.INVALID_PARAMS
    )
    runtime.send(call(3, "pick", task=3, input={"c": 5}))
    assert runtime.recv_until(lambda m: m.get("id") == 3)["error"]["code"] == (
        proto.INVALID_PARAMS
    )
    runtime.send(call(4, "int_or_str", task=4, input={"v": "x"}))
    assert runtime.recv_until(lambda m: m.get("id") == 4)["result"] == "x"
    runtime.send(call(5, "int_or_str", task=5, input={"v": 3.5}))
    assert runtime.recv_until(lambda m: m.get("id") == 5)["error"]["code"] == (
        proto.INVALID_PARAMS
    )


def test_unannotated_param_still_passes_through(runtime):
    runtime.recv()
    runtime.send(call(1, "passthrough", task=1, input={"x": "str"}))
    assert runtime.recv_until(lambda m: m.get("id") == 1)["result"] == "str"
    runtime.send(call(2, "passthrough", task=2, input={"x": 42}))
    assert runtime.recv_until(lambda m: m.get("id") == 2)["result"] == "int"


def test_isolated_calls_validate_the_same_way(runtime):
    runtime.recv()
    runtime.send(call(1, "isolated_spy", task=1, input={"a": "x"}))
    err = runtime.recv_until(lambda m: m.get("id") == 1, timeout=30)
    assert err["error"]["code"] == proto.INVALID_PARAMS
    runtime.send(call(2, "isolated_spy", task=2, input={"a": 1, "b": 2}))
    err = runtime.recv_until(lambda m: m.get("id") == 2, timeout=30)
    assert err["error"]["code"] == proto.INVALID_PARAMS
    runtime.send(call(3, "isolated_spy", task=3, input={}))
    err = runtime.recv_until(lambda m: m.get("id") == 3, timeout=30)
    assert err["error"]["code"] == proto.INVALID_PARAMS
    runtime.send(call(4, "isolated_spy", task=4, input={"a": 7}))
    assert runtime.recv_until(lambda m: m.get("id") == 4, timeout=30)["result"] == 7
    runtime.send(call(5, "isolated_pick", task=5, input={"c": None}))
    assert (
        runtime.recv_until(lambda m: m.get("id") == 5, timeout=30)["result"] == "none"
    )


def test_none_default_admits_null(runtime):
    runtime.recv()
    # `x: str = None` — the default admits null even without Optional.
    runtime.send(call(1, "maybe", task=1, input={"x": None}))
    assert runtime.recv_until(lambda m: m.get("id") == 1)["result"] == "None"
    runtime.send(call(2, "maybe", task=2, input={"x": "s"}))
    assert runtime.recv_until(lambda m: m.get("id") == 2)["result"] == "'s'"
    runtime.send(call(3, "maybe", task=3, input={"x": 5}))
    assert runtime.recv_until(lambda m: m.get("id") == 3)["error"]["code"] == (
        proto.INVALID_PARAMS
    )
    # Same rule through an isolated worker.
    runtime.send(call(4, "isolated_maybe", task=4, input={"x": None}))
    assert (
        runtime.recv_until(lambda m: m.get("id") == 4, timeout=30)["result"] == "None"
    )


# ---- call/stream intent flag (rc.15 PR I) --------------------------------


def test_stream_flag_false_on_generator_is_actionable_error(runtime):
    """A plain call() that reaches a generator must name stream(), not
    produce a stream nobody reads."""
    runtime.recv()
    runtime.send(call(1, "counter", task=1, input={"n": 3}, stream=False))
    resp = runtime.recv()
    assert resp["error"]["code"] == proto.INVALID_PARAMS, resp
    assert "stream()" in resp["error"]["message"], resp
    assert "counter" in resp["error"]["message"], resp


def test_stream_flag_true_on_plain_fn_is_actionable_error(runtime):
    """stream() against a non-generator must name call(), not silently
    yield nothing."""
    runtime.recv()
    runtime.send(call(1, "add", task=1, input={"a": 1, "b": 2}, stream=True))
    resp = runtime.recv()
    assert resp["error"]["code"] == proto.INVALID_PARAMS, resp
    assert "call()" in resp["error"]["message"], resp
    assert "add" in resp["error"]["message"], resp


def test_stream_flag_matching_kinds_still_work(runtime):
    """stream=true on a generator streams; stream=false on a sync fn
    returns the result — the flag validates intent, it doesn't gate."""
    runtime.recv()
    runtime.send(call(1, "counter", task=1, input={"n": 2}, stream=True))
    runtime.send(grant(1))
    terminal = runtime.recv_until(lambda m: m.get("id") == 1, timeout=10)
    assert terminal["result"] == {"done": True}, terminal
    runtime.send(call(2, "add", task=2, input={"a": 2, "b": 3}, stream=False))
    assert runtime.recv()["result"] == 5


def test_absent_stream_flag_keeps_legacy_behaviour(runtime):
    """An older host sends no `stream` — no validation either way."""
    runtime.recv()
    runtime.send(call(1, "add", task=1, input={"a": 1, "b": 1}))
    assert runtime.recv()["result"] == 2
    runtime.send(call(2, "counter", task=2, input={"n": 1}))
    runtime.send(grant(2))
    terminal = runtime.recv_until(lambda m: m.get("id") == 2, timeout=10)
    assert terminal["result"] == {"done": True}, terminal
