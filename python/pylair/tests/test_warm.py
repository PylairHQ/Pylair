"""Warm workers (decision 0008, PR 1): the lazy per-function pool.

End-to-end proofs against a real spawned runtime — the same harness as
test_protocol.py. A warm worker's pid is the identity witness: same pid
on the next call means the process survived (warm), a different pid
means the pool respawned.
"""

from __future__ import annotations

import os
import signal
import sys
import time

import pytest

import pylair._expose as _ex
from conftest import Runtime
from pylair import _protocol as proto
from pylair._expose import expose, exposed_functions


def _with_clean_registry(fn):
    """Run fn() with a private registry, restoring the real one after —
    the same pattern test_expose.py uses for in-process decoration."""
    saved = dict(_ex._registry)
    _ex._registry.clear()
    try:
        return fn()
    finally:
        _ex._registry.clear()
        _ex._registry.update(saved)


def call(rid: int, fn: str, task: int, input: dict | None = None) -> dict:
    return proto.request(rid, "call", {"fn": fn, "task": task, "input": input or {}})


def cancel(rid: int, task: int) -> dict:
    return proto.request(rid, "cancel", {"task": task})


def _wait_pidfile(path, timeout: float = 15.0) -> int:
    """Poll for the worker's pidfile — never a fixed sleep."""
    deadline = time.time() + timeout
    while not path.exists():
        assert time.time() < deadline, f"pidfile {path} never appeared"
        time.sleep(0.05)
    return int(path.read_text().strip())


def _pid_dead(pid: int, timeout: float = 15.0) -> None:
    """Poll until the pid is gone (POSIX kill-0; Windows skips per the
    suite's existing convention — its kill path is proven elsewhere)."""
    if sys.platform == "win32":
        return
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            os.kill(pid, 0)
        except ProcessLookupError:
            return
        time.sleep(0.05)
    raise AssertionError(f"pid {pid} still alive after {timeout}s")


# ---- decoration-time validation ----------------------------------------------


def test_warm_option_implies_isolated_kind():
    def go():
        @expose(worker={"mode": "warm"})
        def probe() -> int:
            return 1

        entry = exposed_functions()["probe"]
        assert entry.kind == "isolated"
        assert entry.worker == {"mode": "warm", "pool": 1}
        assert entry.signature["meta"]["worker"] == {"mode": "warm", "pool": 1}

    _with_clean_registry(go)


def test_warm_option_rejects_unimplemented_keys():
    def go():
        with pytest.raises(ValueError, match="prewarm"):
            expose(worker={"mode": "warm", "prewarm": True})(lambda: 1)
        with pytest.raises(ValueError, match="cancel_grace_ms"):
            expose(worker={"mode": "warm", "cancel_grace_ms": 100})(lambda: 1)
        with pytest.raises(ValueError, match="idle_ttl_s"):
            expose(worker={"mode": "warm", "idle_ttl_s": 60})(lambda: 1)

    _with_clean_registry(go)


def test_warm_option_rejects_bad_pool_and_mode():
    def go():
        with pytest.raises(ValueError, match="pool"):
            expose(worker={"mode": "warm", "pool": 0})(lambda: 1)
        with pytest.raises(ValueError, match="pool"):
            expose(worker={"mode": "warm", "pool": 1.5})(lambda: 1)
        with pytest.raises(ValueError, match="mode"):
            expose(worker={"mode": "bogus"})(lambda: 1)
        with pytest.raises(ValueError, match="unknown worker option"):
            expose(worker={"mode": "warm", "ttl": 5})(lambda: 1)

    _with_clean_registry(go)


def test_warm_option_accepts_defaults_and_process_mode():
    def go():
        @expose(worker={"mode": "warm", "prewarm": False})
        def ok() -> int:
            return 1

        assert exposed_functions()["ok"].worker == {"mode": "warm", "pool": 1}

    _with_clean_registry(go)


# ---- pool behavior ------------------------------------------------------------


def test_warm_second_call_same_pid(runtime):
    """The signature property: the second call lands on the same process."""
    runtime.recv()
    runtime.send(call(1, "warm_pid", task=1))
    first = runtime.recv()["result"]
    runtime.send(call(2, "warm_pid", task=2))
    second = runtime.recv()["result"]
    assert first == second, "second call reached a different process"


def test_warm_mark_proves_state_and_fifo(runtime):
    """pool=1 serializes calls; the second mark sees the first's append —
    one process, FIFO order."""
    runtime.recv()
    runtime.send(call(1, "warm_mark", task=1, input={"tag": "a", "delay": 0.5}))
    runtime.send(call(2, "warm_mark", task=2, input={"tag": "b"}))
    r1 = runtime.recv_until(lambda m: m.get("id") == 1)
    r2 = runtime.recv_until(lambda m: m.get("id") == 2)
    assert r1["result"] == ["a"]
    assert r2["result"] == ["a", "b"], "b ran before a finished — not FIFO"


def test_warm_pool_two_members_serve_concurrently(runtime, tmp_path):
    """pool=2 allows two live members: two in-flight calls carry
    distinct pids — a serialized single member could never do that."""
    runtime.recv()
    p1, p2 = tmp_path / "m1.pid", tmp_path / "m2.pid"
    runtime.send(
        call(1, "warm2_sleep", task=1, input={"seconds": 1.5, "pidfile": str(p1)})
    )
    runtime.send(
        call(2, "warm2_sleep", task=2, input={"seconds": 1.5, "pidfile": str(p2)})
    )
    pid1, pid2 = _wait_pidfile(p1), _wait_pidfile(p2)
    assert pid1 != pid2, "pool=2 served two concurrent calls on one member"
    # Both calls sleep the same 1.5 s — either response may land first;
    # recv_until would discard the out-of-order one, so collect by id.
    results = {}
    deadline = time.time() + 15
    while len(results) < 2:
        m = runtime.recv(timeout=max(0.1, deadline - time.time()))
        if m.get("id") in (1, 2):
            results[m["id"]] = m
    assert results[1]["result"] == "slept"
    assert results[2]["result"] == "slept"


def test_warm_wake_is_not_stolen_by_a_fresh_call(runtime):
    """FIFO regression: a woken head-of-queue waiter must be served
    before a call that arrives after the member freed. Task 1 frees the
    member; task 2 is the woken waiter; task 3's dispatch races task 2's
    resume — task 2's queue entry holds the head until it claims, so
    task 3 can only join the tail."""
    runtime.recv()
    runtime.send(call(1, "warm_mark", task=1, input={"tag": "hold", "delay": 1}))
    runtime.send(call(2, "warm_mark", task=2, input={"tag": "woken"}))
    # Same gate as the queued-cancel test: prove task 2 reached the pool
    # queue before task 1 frees the member.
    runtime.send(call(99, "add", task=99, input={"a": 1, "b": 1}))
    runtime.recv_until(lambda m: m.get("id") == 99)
    assert runtime.recv_until(lambda m: m.get("id") == 1)["result"] == ["hold"]
    # Task 1's response is in hand — the member is free and task 2's wake
    # is in flight. Task 3 arriving now must still lose to task 2.
    runtime.send(call(3, "warm_mark", task=3, input={"tag": "fresh"}))
    assert runtime.recv_until(lambda m: m.get("id") == 2)["result"] == [
        "hold",
        "woken",
    ]
    assert runtime.recv_until(lambda m: m.get("id") == 3)["result"] == [
        "hold",
        "woken",
        "fresh",
    ], "a fresh call stole the freed member from the woken head-of-queue"


def test_warm_cancel_kills_and_respawns(runtime, tmp_path):
    """Running-call cancel = today's kill path: terminated ack, dead pid,
    lazy respawn on the next call. Pools are per-function, so every
    witness is the same fn's member."""
    runtime.recv()
    p1 = tmp_path / "a.pid"
    runtime.send(
        call(1, "warm_sleep", task=1, input={"seconds": 0, "pidfile": str(p1)})
    )
    runtime.recv()
    pid_a = _wait_pidfile(p1)

    p2 = tmp_path / "b.pid"
    runtime.send(
        call(2, "warm_sleep", task=2, input={"seconds": 60, "pidfile": str(p2)})
    )
    assert _wait_pidfile(p2) == pid_a, "second call reached a new process"
    runtime.send(cancel(9, 2))
    ack = terminal = None
    deadline = time.time() + 15
    while ack is None or terminal is None:
        m = runtime.recv(timeout=max(0.1, deadline - time.time()))
        if m.get("id") == 9:
            ack = m
        elif m.get("id") == 2:
            terminal = m
    assert ack["result"] == {"cancelled": True, "status": "terminated"}
    assert terminal["error"]["code"] == proto.TASK_CANCELLED
    _pid_dead(pid_a)

    p3 = tmp_path / "c.pid"
    runtime.send(
        call(3, "warm_sleep", task=3, input={"seconds": 0, "pidfile": str(p3)})
    )
    assert runtime.recv()["result"] == "slept"
    assert _wait_pidfile(p3) != pid_a, "cancel-killed worker was reused"


def test_warm_queued_cancel_never_runs(runtime):
    """A call cancelled while queued is dropped and never reaches a
    worker. Pools are per-function, so the queue only forms on the SAME
    fn — and warm_mark's own list is the side-effect witness: a dropped
    call never appends its tag."""
    runtime.recv()
    # Task 1 holds the pool's one member for 2 s; tasks 2 and 3 queue
    # behind it in FIFO order.
    runtime.send(call(1, "warm_mark", task=1, input={"tag": "hold", "delay": 2}))
    runtime.send(call(2, "warm_mark", task=2, input={"tag": "dropped"}))
    runtime.send(call(3, "warm_mark", task=3, input={"tag": "after"}))
    # The cancel must land while task 2 is actually QUEUED — cancel is a
    # prio-0 control frame that legally outruns queued data frames, so
    # ordering by send sequence alone can still hit "pending". `add`'s
    # result is the deterministic gate: it is a data frame dispatched on
    # the loop after calls 1-3 (registered tasks now) and its coroutine
    # ran after task 2's coro-step (FIFO ready queue) — so task 2 is
    # provably suspended on the pool queue when the cancel arrives.
    runtime.send(call(99, "add", task=99, input={"a": 1, "b": 1}))
    runtime.recv_until(lambda m: m.get("id") == 99)
    runtime.send(cancel(9, 2))
    ack = runtime.recv_until(lambda m: m.get("id") == 9)
    assert ack["result"] == {"cancelled": True}, (
        "queued-warm cancel must report the plain confirmed outcome, "
        f"got {ack['result']}"
    )
    assert runtime.recv_until(lambda m: m.get("id") == 2)["error"]["code"] == (
        proto.TASK_CANCELLED
    )
    assert runtime.recv_until(lambda m: m.get("id") == 1)["result"] == ["hold"]
    # Task 3 runs next in FIFO order; "dropped" must be absent forever.
    assert runtime.recv_until(lambda m: m.get("id") == 3)["result"] == [
        "hold",
        "after",
    ], "cancelled-while-queued call ran anyway"


def test_warm_death_mid_call_then_respawn(runtime):
    runtime.recv()
    runtime.send(call(1, "warm_maybe_crash", task=1, input={"crash": False}))
    pid_a = runtime.recv()["result"]

    runtime.send(call(2, "warm_maybe_crash", task=2, input={"crash": True}))
    assert runtime.recv()["error"]["code"] == proto.WORKER_DIED
    _pid_dead(pid_a)

    runtime.send(call(3, "warm_maybe_crash", task=3, input={"crash": False}))
    pid_b = runtime.recv()["result"]
    assert pid_b != pid_a, "dead worker was reused"


def test_warm_crash_loop_stops_pool(runtime):
    """3 consecutive deaths with no success stop the pool; a stopped pool
    fails fast with a typed error and does not affect other pools."""
    runtime.recv()
    for i in range(3):
        runtime.send(call(i + 1, "warm_maybe_crash", task=i + 1, input={"crash": True}))
        resp = runtime.recv_until(lambda m, i=i: m.get("id") == i + 1)
        assert resp["error"]["code"] == proto.WORKER_DIED

    runtime.send(call(4, "warm_maybe_crash", task=4, input={"crash": False}))
    resp = runtime.recv_until(lambda m: m.get("id") == 4)
    assert resp["error"]["code"] == proto.INTERNAL
    assert resp["error"]["data"]["type"] == "WorkerCrashLoop"

    # Per-function pools: warm_pid's pool is untouched.
    runtime.send(call(5, "warm_pid", task=5))
    assert isinstance(runtime.recv_until(lambda m: m.get("id") == 5)["result"], int)


def test_warm_crash_loop_resets_on_success(runtime):
    """A successful call between deaths resets the consecutive counter."""
    runtime.recv()
    for i in (1, 2):
        runtime.send(call(i, "warm_maybe_crash", task=i, input={"crash": True}))
        resp = runtime.recv_until(lambda m, i=i: m.get("id") == i)
        assert resp["error"]["code"] == proto.WORKER_DIED
    # Success resets deaths to 0 — the pool must tolerate two more deaths.
    runtime.send(call(3, "warm_maybe_crash", task=3, input={"crash": False}))
    assert "result" in runtime.recv_until(lambda m: m.get("id") == 3)
    for i in (4, 5):
        runtime.send(call(i, "warm_maybe_crash", task=i, input={"crash": True}))
        resp = runtime.recv_until(lambda m, i=i: m.get("id") == i)
        assert resp["error"]["code"] == proto.WORKER_DIED
    # deaths is 2, not 4 — the pool still serves.
    runtime.send(call(6, "warm_maybe_crash", task=6, input={"crash": False}))
    assert "result" in runtime.recv_until(lambda m: m.get("id") == 6)


# ---- cap accounting (0008 §3) ---------------------------------------------------


def test_warm_member_holds_cap_slot():
    """max_isolated_workers bounds live processes: one idle warm member
    leaves no room for a process-mode worker."""
    with Runtime(env={"PYLAIR_MAX_ISOLATED_WORKERS": "1"}) as rt:
        rt.recv()
        rt.send(call(1, "warm_pid", task=1))
        rt.recv()
        rt.send(call(2, "isolated_add", task=2, input={"a": 1, "b": 2}))
        resp = rt.recv()
        assert resp["error"]["code"] == proto.LIMIT_EXCEEDED
        assert resp["error"]["data"]["limit"] == "max_isolated_workers"


def test_warm_spawn_cap_fails_at_acquire():
    """pool=2 with cap=1: the second concurrent call hits the live-process
    cap at pool-acquire time — LIMIT_EXCEEDED, not a silent wait."""
    with Runtime(env={"PYLAIR_MAX_ISOLATED_WORKERS": "1"}) as rt:
        rt.recv()
        rt.send(call(1, "warm2_sleep", task=1, input={"seconds": 3}))
        rt.send(call(2, "warm2_sleep", task=2, input={"seconds": 3}))
        resp = rt.recv_until(lambda m: m.get("id") == 2)
        assert resp["error"]["code"] == proto.LIMIT_EXCEEDED
        assert resp["error"]["data"]["limit"] == "max_isolated_workers"
        # Task 1 still completes on its member.
        assert rt.recv_until(lambda m: m.get("id") == 1)["result"] == "slept"


# ---- lifecycle --------------------------------------------------------------------


def test_warm_idle_member_reaped_on_shutdown(tmp_path):
    """An idle warm member is not a task — the drain path still reaps it."""
    pidfile = tmp_path / "idle.pid"
    with Runtime() as rt:
        rt.recv()
        rt.send(
            call(1, "warm_sleep", task=1, input={"seconds": 0, "pidfile": str(pidfile)})
        )
        rt.recv()
        wpid = _wait_pidfile(pidfile)
        rt.send(proto.request(2, "shutdown", None))
        rt.proc.wait(timeout=20)
    _pid_dead(wpid)


def test_warm_eof_reaps_idle_member(tmp_path):
    """stdin EOF takes the same drain path — no orphans either way."""
    pidfile = tmp_path / "eof.pid"
    rt = Runtime()
    try:
        rt.recv()
        rt.send(
            call(1, "warm_sleep", task=1, input={"seconds": 0, "pidfile": str(pidfile)})
        )
        rt.recv()
        wpid = _wait_pidfile(pidfile)
        rt.close_stdin()
        rt.proc.wait(timeout=20)
    finally:
        rt.__exit__(None, None, None)
    _pid_dead(wpid)


@pytest.mark.skipif(sys.platform == "win32", reason="posix process groups")
def test_warm_worker_reaped_on_hard_runtime_death(runtime, tmp_path):
    """A warm member is a long-lived child — when the runtime is SIGKILLed
    the worker's own reaper fork (started once per spawn by
    _exit_with_parent) must end it. The same containment process-mode
    workers get: PYLAIR_WORKER_PARENT + killpg."""
    runtime.recv()
    pidfile = tmp_path / "warm.pid"
    runtime.send(
        call(1, "warm_sleep", task=1, input={"seconds": 0, "pidfile": str(pidfile)})
    )
    runtime.recv()
    wpid = _wait_pidfile(pidfile)
    os.kill(wpid, 0)  # alive — idle in the pool, no call in flight

    runtime.proc.kill()  # SIGKILL the runtime only — no cleanup runs
    runtime.proc.wait(timeout=10)

    deadline = time.time() + 10
    while time.time() < deadline:
        try:
            os.kill(wpid, 0)
        except ProcessLookupError:
            return
        time.sleep(0.2)
    os.kill(wpid, signal.SIGKILL)  # don't leak it from the test itself
    pytest.fail(f"warm worker {wpid} outlived a hard kill of the runtime")
