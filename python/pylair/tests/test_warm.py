"""Warm workers (decision 0008, PR 1): the lazy per-function pool.

End-to-end proofs against a real spawned runtime — the same harness as
test_protocol.py. A warm worker's pid is the identity witness: same pid
on the next call means the process survived (warm), a different pid
means the pool respawned.
"""

from __future__ import annotations

import os
import queue
import signal
import subprocess
import sys
import threading
import time

import pytest

import pylair._expose as _ex
from conftest import Runtime
from pylair import _protocol as proto
from pylair import _readiness
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


# Bound for every wait below. Each one is on a real event — a frame, an
# EOF, a pidfile, a process exit — so the bound is only the hang guard,
# never the mechanism under test. It is generous because what the waits
# block on is the runtime or member PROCESS being scheduled: a saturated
# shared runner can starve a child for tens of seconds, and a 90 s guard
# still lost one death-detection wait in a 20-run loop on macOS.
_HANG_S = 180.0


# The runtime the active test is driving — set by the _hang_guard
# fixture so the file/pid waiters below can dump state on a guard trip
# without threading the fixture through every helper signature.
_ACTIVE_RT = None


def _hang_dump(runtime) -> str:
    """State capture for a tripped hang guard (POSIX loop legs only):
    member ps states first — Z = exited and nobody reaped (child
    watcher / proc.wait suspect), R/S = never crashed, absent = reaped
    and WORKER_DIED was lost — plus the runtime's open pipe fds (the
    EOF check: a dead member's pipe still open means EOF never
    arrived). Then SIGUSR1 makes _diag_run print every asyncio task's
    coroutine stack — the await that never resolved — and SIGABRT adds
    faulthandler's thread dump (conftest sets PYTHONFAULTHANDLER; the
    harness already drains the child's stderr)."""
    parts: list[str] = []
    if runtime is None or sys.platform == "win32":
        return "<no runtime diagnostics on this path>"
    # Process states FIRST — the runtime's death below makes member
    # reapers sweep the group, changing what ps would show.
    pids = {runtime.proc.pid}
    try:
        kids = subprocess.run(
            ["pgrep", "-P", str(runtime.proc.pid)],
            check=False,
            capture_output=True,
            text=True,
        ).stdout.split()
        for k in kids:
            pids.add(int(k))
            gkids = subprocess.run(
                ["pgrep", "-P", k], check=False, capture_output=True, text=True
            ).stdout.split()
            pids.update(int(g) for g in gkids)
    except OSError as e:
        parts.append(f"pgrep failed: {e}")
    ps = subprocess.run(
        [
            "ps",
            "-o",
            "pid,ppid,stat,etime,command",
            "-p",
            ",".join(map(str, sorted(pids))),
        ],
        check=False,
        capture_output=True,
        text=True,
    )
    parts.append("process states:\n" + ps.stdout + ps.stderr)
    lsof = subprocess.run(
        ["lsof", "-p", str(runtime.proc.pid)],
        check=False,
        capture_output=True,
        text=True,
    )
    parts.append(
        "runtime pipe fds:\n"
        + "\n".join(x for x in lsof.stdout.splitlines() if "PIPE" in x)
    )
    parts.append(
        "harness state: "
        f"stdout_drain_alive={runtime._stdout_t.is_alive()} "
        f"io_error={runtime.io_error!r} queue_depth~{runtime._q.qsize()} "
        f"rc={runtime.proc.poll()}"
    )
    # SIGUSR1: _diag_run's handler prints every asyncio task's coroutine
    # stack — the await that never resolved. SIGABRT after it:
    # faulthandler's thread dump as backup.
    try:
        os.kill(runtime.proc.pid, signal.SIGUSR1)
        time.sleep(1.5)
    except OSError as e:
        parts.append(f"SIGUSR1 to runtime {runtime.proc.pid} failed: {e}")
    try:
        os.kill(runtime.proc.pid, signal.SIGABRT)
        time.sleep(2.0)
    except OSError as e:
        parts.append(f"SIGABRT to runtime {runtime.proc.pid} failed: {e}")
    parts.append(
        "runtime stderr tail (task dump then thread dump, newest last):\n"
        + ("\n".join(list(runtime.stderr_lines)[-300:]) or "<empty>")
    )
    return "\n".join(parts)


def _guard(rt) -> None:
    """Default a Runtime's recv/recv_until to _HANG_S and make a guard
    trip dump ITS state — required on custom `Runtime(env=...)` handles
    too, not only the fixture's."""
    global _ACTIVE_RT
    _ACTIVE_RT = rt
    orig_recv, orig_until = rt.recv, rt.recv_until

    def recv(timeout=_HANG_S):
        try:
            return orig_recv(timeout=timeout)
        except queue.Empty:
            raise AssertionError(
                f"recv starved past {timeout}s\n{_hang_dump(rt)}"
            ) from None

    def recv_until(pred, timeout=_HANG_S):
        try:
            return orig_until(pred, timeout=timeout)
        except (TimeoutError, queue.Empty):
            raise AssertionError(
                f"recv_until starved past {timeout}s\n{_hang_dump(rt)}"
            ) from None

    rt.recv, rt.recv_until = recv, recv_until


@pytest.fixture(autouse=True)
def _hang_guard(runtime):
    """Wrap the fixture Runtime; tests spawning extra `Runtime(env=...)`
    handles call _guard on each — a trip must dump the runtime that
    actually wedged, not the fixture's idle one."""
    _guard(runtime)
    yield runtime
    _ACTIVE_RT = None


def _wait_file(path, timeout: float = _HANG_S) -> None:
    """Poll for a marker file — never a fixed sleep."""
    deadline = time.time() + timeout
    while not path.exists():
        if time.time() >= deadline:
            raise AssertionError(
                f"marker {path} never appeared\n{_hang_dump(_ACTIVE_RT)}"
            )
        time.sleep(0.05)


def _wait_pidfile(path, timeout: float = _HANG_S) -> int:
    """Poll for the worker's pidfile — never a fixed sleep. The file
    must also PARSE as a pid: a writer that doesn't publish atomically
    exposes a create-then-write gap where exists() is already true but
    the content hasn't landed yet."""
    deadline = time.time() + timeout
    while True:
        try:
            return int(path.read_text().strip())
        except (OSError, ValueError):
            if time.time() >= deadline:
                raise AssertionError(
                    f"pidfile {path} never appeared/parsed\n{_hang_dump(_ACTIVE_RT)}"
                ) from None
            time.sleep(0.05)


def _pid_alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
        return True
    except OSError:
        return False


def _children(pid: int) -> list[int]:
    """Direct children of a worker pid — on POSIX the parent-death
    reaper is the one expected child; pgrep covers Linux + macOS, and
    Windows spawns no reaper."""
    if sys.platform == "win32":
        return []
    out = subprocess.run(
        ["pgrep", "-P", str(pid)], check=False, capture_output=True, text=True
    ).stdout
    return [int(x) for x in out.split()]


def _pid_state(pid: int) -> str:
    """A ps state letter (R/S/Z/…) or 'gone' — diagnostic only."""
    out = subprocess.run(
        ["ps", "-o", "stat=", "-p", str(pid)],
        check=False,
        capture_output=True,
        text=True,
    ).stdout.strip()
    return out or "gone"


def _watch_deaths(pids: list, stop: threading.Event, out: list) -> None:
    """Append (role, pid) to `out` in the order each watched pid goes
    away — run beside a SIGINT hammer so a dead-worker failure names
    who died first instead of only reporting the channel EOF."""
    pending = list(pids)
    while pending and not stop.is_set():
        for role, pid in pending[:]:
            if not _pid_alive(pid):
                out.append((role, pid))
                pending.remove((role, pid))
        time.sleep(0.0005)


def _diagnose(runtime, pid: int | None = None, deaths: list | None = None) -> str:
    """Failure context for a silent/hung warm call: whether the serving
    member is still alive, the state of its children (the POSIX reaper),
    a death-order list when a watcher ran, plus the runtime's stderr
    tail — member stderr is forwarded there live, so a worker exception
    shows up, and the parent's EOF line carries the worker's rc."""
    parts = []
    if pid is not None:
        state = "alive" if _pid_alive(pid) else "gone"
        parts.append(f"member pid {pid} is {state}")
        kids = [(p, _pid_state(p)) for p in _children(pid)]
        parts.append(f"member children (reaper): {kids or 'none'}")
    if deaths is not None:
        order = ", ".join(f"{role} {p}" for role, p in deaths) or "<none yet>"
        parts.append(f"death order (first listed died first): {order}")
    tail = list(runtime.stderr_lines)[-30:]
    parts.append("runtime stderr tail:\n" + ("\n".join(tail) or "<empty>"))
    return "\n".join(parts)


def _pid_dead(pid: int, timeout: float = _HANG_S) -> None:
    """Poll until the pid is gone. POSIX uses kill-0; Windows polls
    tasklist — os.kill(pid, 0) is NOT a probe there (signal 0 maps to
    TerminateProcess and would kill the member itself)."""
    deadline = time.time() + timeout
    if sys.platform == "win32":
        while time.time() < deadline:
            # /FI filters server-side: a live pid emits one CSV row
            # starting with a quote; a dead pid only an INFO line.
            out = subprocess.run(
                ["tasklist", "/FI", f"PID eq {pid}", "/NH", "/FO", "CSV"],
                check=False,
                capture_output=True,
                text=True,
            ).stdout.strip()
            if not out.startswith('"'):
                return
            time.sleep(0.1)
    else:
        while time.time() < deadline:
            try:
                os.kill(pid, 0)
            except ProcessLookupError:
                return
            time.sleep(0.05)
    raise AssertionError(
        f"pid {pid} still alive after {timeout}s\n{_hang_dump(_ACTIVE_RT)}"
    )


def _collect(runtime, ids: set, timeout: float = _HANG_S) -> dict:
    """Gather one frame per id, order-agnostic — recv_until DISCARDS
    non-matching frames, so two sequential recv_untils lose whichever
    message arrives first."""
    out = {}
    deadline = time.time() + timeout
    while ids - out.keys():
        m = runtime.recv(timeout=max(0.1, deadline - time.time()))
        if m.get("id") in ids:
            out[m["id"]] = m
    return out


# ---- decoration-time validation ----------------------------------------------


def test_warm_option_implies_isolated_kind():
    def go():
        @expose(worker={"mode": "warm"})
        def probe() -> int:
            return 1

        entry = exposed_functions()["probe"]
        assert entry.kind == "isolated"
        assert entry.worker == {
            "mode": "warm",
            "pool": 1,
            "cancel_grace_ms": 3000,
        }
        assert entry.signature["meta"]["worker"] == entry.worker

    _with_clean_registry(go)


def test_warm_option_rejects_bad_prewarm_type():
    def go():
        with pytest.raises(TypeError, match="prewarm"):
            expose(worker={"mode": "warm", "prewarm": "yes"})(lambda: 1)

    _with_clean_registry(go)


def test_warm_option_idle_ttl_s_validation():
    """0008 §4: idle_ttl_s is None (default — absent from the normalized
    config) or a finite number > 0; bool, 0, negatives, NaN/inf and
    non-numbers are rejected at decoration like every other worker key."""

    def go():
        @expose(worker={"mode": "warm", "idle_ttl_s": 60})
        def probe() -> int:
            return 1

        assert exposed_functions()["probe"].worker == {
            "mode": "warm",
            "pool": 1,
            "cancel_grace_ms": 3000,
            "idle_ttl_s": 60,
        }

        @expose(worker={"mode": "warm", "idle_ttl_s": 0.5})
        def probe_float() -> int:
            return 1

        assert exposed_functions()["probe_float"].worker["idle_ttl_s"] == 0.5

        # Default stays absent — a normalized config carries no
        # idle_ttl_s key at all.
        @expose(worker={"mode": "warm"})
        def probe_default() -> int:
            return 1

        assert "idle_ttl_s" not in exposed_functions()["probe_default"].worker

        for bad in (True, 0, 0.0, -1, -0.5, float("nan"), float("inf"), "30"):
            with pytest.raises(ValueError, match="idle_ttl_s"):
                expose(worker={"mode": "warm", "idle_ttl_s": bad})(lambda: 1)

    _with_clean_registry(go)


def test_warm_option_prewarm_declares_component():
    """0008 §6: prewarm=True normalizes into the config and declares the
    managed worker:<fn> component in phase loading at decoration — the
    first post-hello flush shows it and the sweep must not fail it."""

    def go():
        @expose(worker={"mode": "warm", "prewarm": True})
        def probe() -> int:
            return 1

        entry = exposed_functions()["probe"]
        assert entry.worker["prewarm"] is True
        comp = _readiness._default._components["worker:probe"]
        assert comp.phase == "loading"
        assert comp.managed is True

        # A lazy warm fn declares nothing.
        @expose(worker={"mode": "warm"})
        def lazy() -> int:
            return 1

        assert "worker:lazy" not in _readiness._default._components

    saved = dict(_ex._registry)
    _ex._registry.clear()
    _readiness.reset()
    try:
        go()
    finally:
        _ex._registry.clear()
        _ex._registry.update(saved)
        _readiness.reset()


def test_warm_option_cancel_grace_ms():
    """0008 §2: the interrupt grace is accepted, bounded by the shared
    kill budget (0..3000 ms)."""

    def go():
        @expose(worker={"mode": "warm", "cancel_grace_ms": 250})
        def probe() -> int:
            return 1

        assert exposed_functions()["probe"].worker == {
            "mode": "warm",
            "pool": 1,
            "cancel_grace_ms": 250,
        }
        with pytest.raises(ValueError, match="cancel_grace_ms"):
            expose(worker={"mode": "warm", "cancel_grace_ms": -1})(lambda: 1)
        with pytest.raises(ValueError, match="cancel_grace_ms"):
            expose(worker={"mode": "warm", "cancel_grace_ms": 3001})(lambda: 1)
        with pytest.raises(ValueError, match="cancel_grace_ms"):
            expose(worker={"mode": "warm", "cancel_grace_ms": "slow"})(lambda: 1)

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

        assert exposed_functions()["ok"].worker == {
            "mode": "warm",
            "pool": 1,
            "cancel_grace_ms": 3000,
        }

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
    deadline = time.time() + _HANG_S
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
    """The stuck-call cancel takes the kill path: terminated ack, dead
    pid, lazy respawn on the next call. The member must be genuinely
    uninterruptible — a plain sleep now survives the interrupt-first
    cancel on POSIX, so the target is the signal-immune fixture
    (150 ms grace keeps the test fast). Pools are per-function, so
    every witness is the same fn's member."""
    runtime.recv()
    p1 = tmp_path / "a.pid"
    runtime.send(
        call(
            1,
            "warm_immune_fastgrace",
            task=1,
            input={"seconds": 0, "pidfile": str(p1)},
        )
    )
    runtime.recv()
    pid_a = _wait_pidfile(p1)

    p2 = tmp_path / "b.pid"
    runtime.send(
        call(
            2,
            "warm_immune_fastgrace",
            task=2,
            input={"seconds": 60, "pidfile": str(p2)},
        )
    )
    assert _wait_pidfile(p2) == pid_a, "second call reached a new process"
    runtime.send(cancel(9, 2))
    msgs = _collect(runtime, {2, 9})
    assert msgs[9]["result"] == {"cancelled": True, "status": "terminated"}
    assert msgs[2]["error"]["code"] == proto.TASK_CANCELLED
    _pid_dead(pid_a)

    p3 = tmp_path / "c.pid"
    runtime.send(
        call(
            3,
            "warm_immune_fastgrace",
            task=3,
            input={"seconds": 0, "pidfile": str(p3)},
        )
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
    msgs = _collect(runtime, {2, 9})
    assert msgs[9]["result"] == {"cancelled": True}, (
        "queued-warm cancel must report the plain confirmed outcome, "
        f"got {msgs[9]['result']}"
    )
    assert msgs[2]["error"]["code"] == proto.TASK_CANCELLED
    assert runtime.recv_until(lambda m: m.get("id") == 1)["result"] == ["hold"]
    # Task 3 runs next in FIFO order; "dropped" must be absent forever.
    assert runtime.recv_until(lambda m: m.get("id") == 3)["result"] == [
        "hold",
        "after",
    ], "cancelled-while-queued call ran anyway"


def test_warm_death_mid_call_then_respawn(runtime):
    runtime.recv(timeout=_HANG_S)
    runtime.send(call(1, "warm_maybe_crash", task=1, input={"crash": False}))
    pid_a = runtime.recv(timeout=_HANG_S)["result"]

    runtime.send(call(2, "warm_maybe_crash", task=2, input={"crash": True}))
    assert runtime.recv(timeout=_HANG_S)["error"]["code"] == proto.WORKER_DIED
    _pid_dead(pid_a, timeout=_HANG_S)

    runtime.send(call(3, "warm_maybe_crash", task=3, input={"crash": False}))
    pid_b = runtime.recv(timeout=_HANG_S)["result"]
    assert pid_b != pid_a, "dead worker was reused"


def test_warm_crash_loop_stops_pool(runtime):
    """3 consecutive deaths with no success stop the pool; a stopped pool
    fails fast with a typed error and does not affect other pools."""
    runtime.recv(timeout=_HANG_S)
    for i in range(3):
        runtime.send(call(i + 1, "warm_maybe_crash", task=i + 1, input={"crash": True}))
        resp = runtime.recv_until(lambda m, i=i: m.get("id") == i + 1, timeout=_HANG_S)
        assert resp["error"]["code"] == proto.WORKER_DIED

    runtime.send(call(4, "warm_maybe_crash", task=4, input={"crash": False}))
    resp = runtime.recv_until(lambda m: m.get("id") == 4, timeout=_HANG_S)
    assert resp["error"]["code"] == proto.INTERNAL
    assert resp["error"]["data"]["type"] == "WorkerCrashLoop"

    # Per-function pools: warm_pid's pool is untouched.
    runtime.send(call(5, "warm_pid", task=5))
    assert isinstance(
        runtime.recv_until(lambda m: m.get("id") == 5, timeout=_HANG_S)["result"], int
    )


def test_warm_crash_loop_resets_on_success(runtime):
    """A successful call between deaths resets the consecutive counter."""
    runtime.recv(timeout=_HANG_S)
    for i in (1, 2):
        runtime.send(call(i, "warm_maybe_crash", task=i, input={"crash": True}))
        resp = runtime.recv_until(lambda m, i=i: m.get("id") == i, timeout=_HANG_S)
        assert resp["error"]["code"] == proto.WORKER_DIED
    # Success resets deaths to 0 — the pool must tolerate two more deaths.
    runtime.send(call(3, "warm_maybe_crash", task=3, input={"crash": False}))
    assert "result" in runtime.recv_until(lambda m: m.get("id") == 3, timeout=_HANG_S)
    for i in (4, 5):
        runtime.send(call(i, "warm_maybe_crash", task=i, input={"crash": True}))
        resp = runtime.recv_until(lambda m, i=i: m.get("id") == i, timeout=_HANG_S)
        assert resp["error"]["code"] == proto.WORKER_DIED
    # deaths is 2, not 4 — the pool still serves.
    runtime.send(call(6, "warm_maybe_crash", task=6, input={"crash": False}))
    assert "result" in runtime.recv_until(lambda m: m.get("id") == 6, timeout=_HANG_S)


# ---- cap accounting (0008 §3) ---------------------------------------------------


def test_warm_member_holds_cap_slot():
    """max_isolated_workers bounds live processes: one idle warm member
    leaves no room for a process-mode worker."""
    with Runtime(env={"PYLAIR_MAX_ISOLATED_WORKERS": "1"}) as rt:
        _guard(rt)
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
        _guard(rt)
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
        _guard(rt)
        rt.recv(timeout=_HANG_S)
        rt.send(
            call(1, "warm_sleep", task=1, input={"seconds": 0, "pidfile": str(pidfile)})
        )
        rt.recv(timeout=_HANG_S)
        wpid = _wait_pidfile(pidfile)
        rt.send(proto.request(2, "shutdown", None))
        rt.proc.wait(timeout=_HANG_S)
    _pid_dead(wpid)


def test_warm_eof_reaps_idle_member(tmp_path):
    """stdin EOF takes the same drain path — no orphans either way."""
    pidfile = tmp_path / "eof.pid"
    rt = Runtime()
    _guard(rt)
    try:
        rt.recv(timeout=_HANG_S)
        rt.send(
            call(1, "warm_sleep", task=1, input={"seconds": 0, "pidfile": str(pidfile)})
        )
        rt.recv(timeout=_HANG_S)
        wpid = _wait_pidfile(pidfile)
        rt.close_stdin()
        rt.proc.wait(timeout=_HANG_S)
    finally:
        rt.__exit__(None, None, None)
    _pid_dead(wpid)


# ---- interrupt-first cancel (0008 §2) -----------------------------------------


def test_warm_interruptible_call_survives_cancel(runtime, tmp_path):
    """POSIX — a sleep-bound call is interrupted inside the raise window:
    the member answers {"interrupted": true}, reports survived — plain
    cancelled, TASK_CANCELLED, same pid serving the next call.

    Windows — kill-only fallback (0008 §2): CTRL_BREAK needs a console
    the embedded worker does not have, so cancel takes the existing kill
    path — terminated ack and a new pid, exactly like process mode."""
    runtime.recv()
    p1, p2 = tmp_path / "a.pid", tmp_path / "b.pid"
    runtime.send(
        call(1, "warm_sleep", task=1, input={"seconds": 60, "pidfile": str(p1)})
    )
    pid = _wait_pidfile(p1, timeout=_HANG_S)
    runtime.send(cancel(9, 1))
    ack = terminal = None
    deadline = time.time() + _HANG_S
    while ack is None or terminal is None:
        m = runtime.recv(timeout=max(0.1, deadline - time.time()))
        if m.get("id") == 9:
            ack = m
        elif m.get("id") == 1:
            terminal = m
    assert terminal["error"]["code"] == proto.TASK_CANCELLED
    runtime.send(
        call(2, "warm_sleep", task=2, input={"seconds": 0, "pidfile": str(p2)})
    )
    assert runtime.recv(timeout=_HANG_S)["result"] == "slept"
    if sys.platform == "win32" or ack["result"] == {
        "cancelled": True,
        "status": "terminated",
    }:
        # Kill path: Windows interrupts nothing console-less; POSIX gets
        # here only when no response lands inside cancel_grace_ms — a
        # starved runner stalls the runtime's frame processing exactly
        # like a stuck call, and silence-past-grace IS the kill path by
        # design (0008 §2; the verdict is parent-side). Either way the
        # member died and the next call respawned.
        assert ack["result"] == {"cancelled": True, "status": "terminated"}
        assert _wait_pidfile(p2) != pid, "killed member was reused"
    else:
        # Survived: a response inside the grace — plain cancelled, the
        # member lived and serves the next call (0008 §2 ack rule).
        assert ack["result"] == {"cancelled": True}
        assert _wait_pidfile(p2) == pid, "survived member was not reused"


def test_warm_sigint_immune_is_killed_after_grace(runtime, tmp_path):
    """The stuck path: a signal-ignoring call answers nothing inside the
    grace — the existing kill path runs, ack is terminated, and the
    next call respawns."""
    runtime.recv()
    p1, p2 = tmp_path / "a.pid", tmp_path / "b.pid"
    runtime.send(
        call(
            1,
            "warm_interrupt_immune",
            task=1,
            input={"seconds": 60, "pidfile": str(p1)},
        )
    )
    pid = _wait_pidfile(p1)
    t0 = time.monotonic()
    runtime.send(cancel(9, 1))
    msgs = _collect(runtime, {1, 9})
    assert msgs[9]["result"] == {"cancelled": True, "status": "terminated"}
    assert msgs[1]["error"]["code"] == proto.TASK_CANCELLED
    # POSIX: the 3 s grace must have elapsed before the kill — the ack
    # cannot land instantly on the stuck path. Windows is kill-only:
    # no interrupt is attempted, so no grace elapses either.
    if sys.platform != "win32":
        assert time.monotonic() - t0 >= 2.5, "stuck path skipped the grace wait"
    _pid_dead(pid)
    runtime.send(
        call(
            2,
            "warm_interrupt_immune",
            task=2,
            input={"seconds": 0, "pidfile": str(p2)},
        )
    )
    assert runtime.recv()["result"] == "slept"
    assert _wait_pidfile(p2) != pid, "killed member was reused"


def test_warm_cancel_grace_ms_reaches_kill_faster(runtime, tmp_path):
    """A configured cancel_grace_ms=150 hits the stuck path at ~150 ms
    instead of the 3 s default — proof the decoration value is honored."""
    runtime.recv()
    pidfile = tmp_path / "f.pid"
    runtime.send(
        call(
            1,
            "warm_immune_fastgrace",
            task=1,
            input={"seconds": 60, "pidfile": str(pidfile)},
        )
    )
    _wait_pidfile(pidfile)
    t0 = time.monotonic()
    runtime.send(cancel(9, 1))
    ack = runtime.recv_until(lambda m: m.get("id") == 9, timeout=_HANG_S)
    elapsed = time.monotonic() - t0
    assert ack["result"] == {"cancelled": True, "status": "terminated"}
    assert elapsed < 3.0, f"grace bound ignored — ack took {elapsed:.2f}s"
    if sys.platform != "win32":
        # The configured ~150 ms grace actually elapsed — the stuck path
        # waited for the (ignored) interrupt before killing.
        assert elapsed >= 0.1, f"grace wait skipped — ack took {elapsed:.3f}s"


def test_warm_interrupt_outside_window_is_ignored(runtime):
    """An interrupt landing while the member idles in read_frame must not
    kill the loop — the handler records it and ignores it (0008 §2)."""
    runtime.recv()
    runtime.send(call(1, "warm_pid", task=1))
    pid = runtime.recv()["result"]
    # The member is alive and idle — deliver the raw signal directly.
    # Windows is kill-only: CTRL_BREAK delivers nothing console-less
    # (and firing console events at a shared test console is itself
    # risky), so only POSIX sends — the no-op invariant pins everywhere.
    if sys.platform != "win32":
        os.killpg(pid, signal.SIGINT)
    runtime.send(call(2, "warm_pid", task=2))
    assert runtime.recv()["result"] == pid, "idle interrupt broke the loop"


def test_warm_interrupt_reaches_worker_witness(runtime, tmp_path):
    """POSIX — the delivery proof: the member's own SIGINT handler writes
    a witness file that can only exist if the interrupt reached it.

    Windows — documents the kill-only fallback: CTRL_BREAK_EVENT cannot
    reach a worker spawned without a console (GenerateConsoleCtrlEvent
    reports success and delivers nothing — probed), so cancel takes the
    kill path: terminated ack, no witness, a new pid next call."""
    runtime.recv()
    witness = tmp_path / "break.witness"
    started = tmp_path / "started"
    runtime.send(
        call(
            1,
            "warm_break_witness",
            task=1,
            input={
                "witness": str(witness),
                "started": str(started),
                "seconds": 60,
            },
        )
    )
    # Running-gate: the marker can only exist once the fn is executing,
    # so the cancel provably lands on a live, started call — never a
    # pre-registration "pending" ack.
    _wait_file(started)
    runtime.send(cancel(9, 1))
    msgs = _collect(runtime, {1, 9})
    ack = msgs[9]
    assert msgs[1]["error"]["code"] == proto.TASK_CANCELLED
    if sys.platform == "win32":
        assert ack["result"] == {"cancelled": True, "status": "terminated"}
        # The handler never ran — there is nothing to wait for.
        assert not witness.exists(), "CTRL_BREAK was delivered after all"
        return
    # The user's handler raised KeyboardInterrupt -> interrupted response
    # -> survived -> plain cancelled. The legal twin on a starved runner
    # is silence past the grace -> kill path -> terminated; the witness
    # itself is then indeterminate (the handler can land in the same
    # scheduling gap that starved the parent's grace), so the aftermath
    # proof is member identity: the kill path means the next call runs
    # on a new member.
    pid1 = int(started.read_text().strip())
    if ack["result"] == {"cancelled": True, "status": "terminated"}:
        started2 = tmp_path / "started2"
        runtime.send(
            call(
                2,
                "warm_break_witness",
                task=2,
                input={"witness": str(witness), "seconds": 0, "started": str(started2)},
            )
        )
        _wait_file(started2, timeout=_HANG_S)
        assert int(started2.read_text().strip()) != pid1, "killed member was reused"
        return
    assert ack["result"] == {"cancelled": True}, _diagnose(runtime)
    deadline = time.time() + _HANG_S
    while not witness.exists():
        assert time.time() < deadline, "interrupt never reached the worker"
        time.sleep(0.05)
    started3 = tmp_path / "started3"
    runtime.send(
        call(
            3,
            "warm_break_witness",
            task=3,
            input={"witness": str(witness), "seconds": 0, "started": str(started3)},
        )
    )
    _wait_file(started3, timeout=_HANG_S)
    assert int(started3.read_text().strip()) == pid1, "survived member was not reused"


def test_warm_interrupt_as_call_returns(runtime, tmp_path):
    """The completion race (0008 §2): signals landing as the call returns
    — flag already cleared, frame being built and written — are recorded
    and ignored. The response must arrive as an intact frame and the
    same member must serve the next call.

    A hammer thread keeps signaling through the flag-clear/write/idle
    window (stretched by a ~5 MB payload) — dozens of signals, every
    boundary. On Windows the signals deliver nothing console-less; the
    intact-frame + member-survival invariants still pin."""
    runtime.recv()
    m1, m2 = tmp_path / "m1", tmp_path / "m2"
    size = 2_000_000
    runtime.send(
        call(
            1,
            "warm_gated_result",
            task=1,
            input={"marker": str(m1), "size": size},
        )
    )
    # The marker lands at fn entry — the hammer starts while the fn
    # builds its result and keeps firing through flag-clear/write/idle.
    pid = _wait_pidfile(m1)
    # Death-order witness (POSIX): records worker/reaper in the order
    # they die so a _WarmWorkerDead names its mechanism on failure.
    deaths: list = []
    wstop = threading.Event()
    watcher = None
    if sys.platform != "win32":
        group = [("worker", pid), *[("reaper", p) for p in _children(pid)]]
        watcher = threading.Thread(
            target=_watch_deaths, args=(group, wstop, deaths), daemon=True
        )
        watcher.start()
    stop = threading.Event()

    def hammer():
        # POSIX-only: Windows is kill-only — CTRL_BREAK cannot reach a
        # console-less worker, and firing console events at a shared
        # test console is itself unsafe. On Windows the test still pins
        # the intact-frame + member-survival invariants.
        if sys.platform == "win32":
            return
        while not stop.is_set():
            try:
                os.killpg(pid, signal.SIGINT)
            except (ProcessLookupError, OSError):
                pass
            time.sleep(0.002)

    t = threading.Thread(target=hammer, daemon=True)
    t.start()
    try:
        resp = runtime.recv(timeout=_HANG_S)
    except queue.Empty:
        raise AssertionError(
            f"no response under the SIGINT hammer\n{_diagnose(runtime, pid, deaths)}"
        )
    stop.set()
    t.join(timeout=_HANG_S)
    wstop.set()
    if watcher is not None:
        watcher.join(timeout=_HANG_S)
    if "error" in resp:
        # A signal slipped inside the raise window — the interrupted
        # payload, mapped for a call with no cancel in flight.
        assert resp["error"]["message"] == "worker call interrupted", _diagnose(
            runtime, pid, deaths
        )
    else:
        # Every signal landed post-window: recorded and ignored, and the
        # frame that was already writing completed intact.
        assert resp["result"]["pid"] == pid
        assert len(resp["result"]["items"]) == size
    runtime.send(
        call(
            2,
            "warm_gated_result",
            task=2,
            input={"marker": str(m2), "size": 10},
        )
    )
    assert runtime.recv()["result"]["pid"] == pid, (
        "interrupt at completion broke the member"
    )


def test_warm_interrupt_burst_at_idle_survives(runtime):
    """Back-to-back SIGINTs while the member sits in `read_frame`: the
    handler's "outside the raise window" diagnostic used to print() to
    buffered stderr — a second signal mid-write re-entered the writer
    and raised RuntimeError inside the serve loop's channel reads,
    killing the member (the CI flake this suite guards). Signals arrive
    here faster than one diagnostic write completes, so the collision
    — not just its rate — is what the burst pins. POSIX only."""
    if sys.platform == "win32":
        pytest.skip("process-group SIGINT is POSIX-only")
    runtime.recv()
    runtime.send(call(1, "warm_pid", task=1))
    pid = runtime.recv()["result"]
    deaths: list = []
    wstop = threading.Event()
    group = [("worker", pid), *[("reaper", p) for p in _children(pid)]]
    watcher = threading.Thread(
        target=_watch_deaths, args=(group, wstop, deaths), daemon=True
    )
    watcher.start()
    for _ in range(400):
        try:
            os.killpg(pid, signal.SIGINT)
        except (ProcessLookupError, OSError):
            break
    wstop.set()
    watcher.join(timeout=_HANG_S)
    runtime.send(call(2, "warm_pid", task=2))
    assert runtime.recv()["result"] == pid, (
        f"idle interrupt burst broke the member\n{_diagnose(runtime, pid, deaths)}"
    )


def test_warm_cancel_before_fn_work_survives(runtime, tmp_path):
    """The started-handshake path (0008 §2): a cancel landing while the
    call is anywhere short of real work — spawning, importing, or gated
    at fn top — waits for the worker's "started" frame inside the same
    grace, then interrupts. Without it, a signal arriving before the
    raise window opens would be ignored and the grace would kill a
    healthy member for nothing. Windows is kill-only as documented."""
    runtime.recv()
    gate = tmp_path / "gate"
    p1 = tmp_path / "a.pid"
    runtime.send(
        call(
            1,
            "warm_gated_start",
            task=1,
            input={"gate": str(gate), "pidfile": str(p1), "seconds": 60},
        )
    )
    # Order gate: proves task 1 registered and dispatched before the
    # cancel — never a pre-registration "pending".
    runtime.send(call(99, "add", task=99, input={"a": 1, "b": 1}))
    runtime.recv_until(lambda m: m.get("id") == 99)
    runtime.send(cancel(9, 1))
    msgs = _collect(runtime, {1, 9})
    assert msgs[1]["error"]["code"] == proto.TASK_CANCELLED
    gate.write_text("go")  # let a surviving member finish future calls
    runtime.send(
        call(
            2,
            "warm_gated_start",
            task=2,
            input={"gate": str(gate)},
        )
    )
    pid2 = runtime.recv()["result"]
    runtime.send(call(3, "warm_gated_start", task=3, input={"gate": str(gate)}))
    # Calls return the serving pid: 2==3 proves warm reuse with no
    # marker files at all — survival never depends on which side of an
    # interrupt the first call's writes landed.
    assert runtime.recv()["result"] == pid2, "pool did not reuse its member"
    if sys.platform == "win32":
        assert msgs[9]["result"] == {"cancelled": True, "status": "terminated"}
        if p1.exists():
            assert pid2 != int(p1.read_text().strip()), "killed member was reused"
    else:
        # Started-handshake survival is the design, but silence past the
        # grace is the kill path the contract also defines: a starved
        # runner stalls the runtime's frame processing exactly like a
        # stuck call (0008 §2 — the verdict is parent-side). Assert the
        # pairing the ack claims, not one outcome.
        if msgs[9]["result"] == {"cancelled": True, "status": "terminated"}:
            if p1.exists():
                assert pid2 != int(p1.read_text().strip()), "killed member was reused"
        else:
            assert msgs[9]["result"] == {"cancelled": True}, _diagnose(runtime)
            # a.pid lands in the fixture's finally — the only gap is an
            # interrupt firing during kwarg coercion, before the fn is even
            # entered, which is a legal landing point and leaves no marker.
            if p1.exists():
                assert pid2 == int(p1.read_text().strip()), (
                    "survived member was not reused"
                )


def test_warm_worker_died_includes_stderr_tail(runtime):
    """#140 review: a crash's last stderr lines ride WORKER_DIED data."""
    runtime.recv(timeout=_HANG_S)
    marker = "STDERR_TAIL_MARKER_9f3"
    runtime.send(call(1, "warm_crash_stderr", task=1, input={"marker": marker}))
    resp = runtime.recv_until(lambda m: m.get("id") == 1, timeout=_HANG_S)
    assert resp["error"]["code"] == proto.WORKER_DIED
    assert marker in resp["error"]["data"]["stderr"], (
        f"stderr tail missing the marker: {resp['error']['data']!r}"
    )


@pytest.mark.skipif(sys.platform == "win32", reason="killpg is POSIX-only")
def test_warm_death_with_killpg_eperm_still_reports_and_respawns(tmp_path):
    """The macOS wedge, reproduced: a member that crashes to a zombie
    leaves a process group on which killpg raises PermissionError, not
    ProcessLookupError. Before the fix that escaped _kill_proc and the
    except handler — the call task ended without a terminal and the
    caller waited forever. The terminal must now be unconditional."""
    with Runtime(env={"PYLAIR_KILLPG_FAULT": "eperm"}) as rt:
        _guard(rt)
        rt.recv(timeout=_HANG_S)
        rt.send(call(1, "warm_crash_stderr", task=1, input={"marker": "EPERM_FIRST"}))
        resp = rt.recv_until(lambda m: m.get("id") == 1, timeout=_HANG_S)
        assert resp["error"]["code"] == proto.WORKER_DIED, (
            f"call ended without its WORKER_DIED terminal: {resp!r}"
        )
        assert "EPERM_FIRST" in resp["error"]["data"]["stderr"], (
            f"stderr tail missing: {resp['error']['data']!r}"
        )
        # The pool respawns: a second call runs on a fresh member and its
        # own marker lands in the tail — proof the call executed.
        rt.send(call(2, "warm_crash_stderr", task=2, input={"marker": "EPERM_SECOND"}))
        resp = rt.recv_until(lambda m: m.get("id") == 2, timeout=_HANG_S)
        assert "EPERM_SECOND" in resp["error"]["data"]["stderr"], (
            f"no respawn — second marker absent: {resp['error']['data']!r}"
        )


@pytest.mark.skipif(sys.platform == "win32", reason="killpg is POSIX-only")
def test_warm_cancel_with_killpg_eperm_reports_terminated(tmp_path):
    """Same EPERM through the cancel path: the member is immune to the
    interrupt but dies on its own mid-grace, so the kill path hits a
    zombie's group — the chain that once surfaced as
    'termination_in_progress' with no call terminal. With signal errors
    contained the verdict stays 'terminated' and the call still ends."""
    pidfile = tmp_path / "sleeper.pid"
    with Runtime(env={"PYLAIR_KILLPG_FAULT": "eperm"}) as rt:
        _guard(rt)
        rt.recv(timeout=_HANG_S)
        rt.send(
            call(
                1,
                "warm_immune_then_crash",
                task=1,
                input={"die_after": 1.0, "pidfile": str(pidfile)},
            )
        )
        _wait_pidfile(pidfile)  # member is inside the call
        rt.send(cancel(9, 1))
        # The terminal (id=1) is emitted strictly BEFORE the verdict
        # (id=9): _finish runs inside the task the reporter shields, so
        # a recv_until(id==9) scan would eat it. Collect both by id.
        msgs = _collect(rt, {1, 9})
        ack = msgs[9]
        assert ack["result"] == {"cancelled": True, "status": "terminated"}, (
            f"cancel verdict degraded under EPERM: {ack!r}"
        )
        # The member self-crashed mid-cancel — WORKER_DIED is the honest
        # terminal; what matters is that a terminal ARRIVES (the wedge
        # was its absence).
        assert msgs[1]["error"]["code"] == proto.WORKER_DIED


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
    runtime.proc.wait(timeout=_HANG_S)

    deadline = time.time() + _HANG_S
    while time.time() < deadline:
        try:
            os.kill(wpid, 0)
        except ProcessLookupError:
            return
        time.sleep(0.2)
    os.kill(wpid, signal.SIGKILL)  # don't leak it from the test itself
    pytest.fail(f"warm worker {wpid} outlived a hard kill of the runtime")


# ---- prewarm + the worker:<fn> component (0008 §6) ----------------------------


def _components_until(
    rt: Runtime, name: str, phases: list[str], timeout: float = _HANG_S
):
    """Collect app.component params for `name` until its phase sequence
    ends with `phases` (or timeout)."""
    frames: list[dict] = []
    deadline = time.time() + timeout
    while True:
        got = [p["phase"] for p in frames if p["name"] == name]
        if got[-len(phases) :] == phases:
            return frames
        assert time.time() < deadline, f"component {name} never reached {phases}: {got}"
        m = rt.recv(timeout=max(0.1, deadline - time.time()))
        if m.get("method") == "app.component":
            frames.append(m["params"])


def test_prewarm_reports_ready_before_any_call():
    """The pool's first member spawns at startup beside the hooks and
    completes its import+resolve handshake — worker:<fn> goes
    loading -> ready without a single call being sent."""
    with Runtime("testapp_prewarm") as rt:
        _guard(rt)
        rt.recv()  # hello
        frames = _components_until(rt, "worker:prewarm_pid", ["loading", "ready"])
        got = [p["phase"] for p in frames if p["name"] == "worker:prewarm_pid"]
        assert got == ["loading", "ready"]


def test_prewarm_first_call_uses_prewarmed_member(tmp_path):
    """The member the component reported ready is the one that serves
    the first call: the worker wrote its pid at import (before the
    handshake) and the call must come back from that same pid."""
    pf = tmp_path / "prewarmed.pid"
    with Runtime("testapp_prewarm", env={"PYLAIR_PREWARM_PIDFILE": str(pf)}) as rt:
        _guard(rt)
        rt.recv()
        _components_until(rt, "worker:prewarm_pid", ["ready"])
        prewarmed = _wait_pidfile(pf)  # the member proved itself at import
        rt.send(call(1, "prewarm_pid", task=1))
        assert rt.recv()["result"] == prewarmed, (
            "first call did not reach the prewarmed member"
        )
        rt.send(call(2, "prewarm_pid", task=2))
        assert rt.recv()["result"] == prewarmed


def test_prewarm_import_failure_reports_failed():
    """A member whose app import explodes answers ready:false — the
    component reports failed with the import error as detail."""
    with Runtime("testapp_warmfail") as rt:
        _guard(rt)
        rt.recv()
        frames = _components_until(rt, "worker:warmfail_pid", ["failed"])
        got = [p for p in frames if p["name"] == "worker:warmfail_pid"]
        assert got[-1]["phase"] == "failed"
        assert "import exploded" in (got[-1].get("detail") or "")


def test_prewarm_requires_gate_waits_then_succeeds(tmp_path):
    """requires=("worker:<fn>",) works for free: the call waits on the
    still-importing pool while dispatch stays live, then succeeds when
    the handshake reports ready."""
    gate = tmp_path / "prewarm.gate"
    with Runtime("testapp_prewarm", env={"PYLAIR_PREWARM_GATE": str(gate)}) as rt:
        _guard(rt)
        rt.recv()
        # The component is visible while the member's import is gated.
        _components_until(rt, "worker:prewarm_pid", ["loading"])
        rt.send(call(1, "after_worker", task=1))
        # Dispatch is never blocked by prewarm — a plain call answers
        # while the gated call still waits on the component.
        rt.send(call(2, "add", task=2, input={"a": 1, "b": 1}))
        assert rt.recv_until(lambda m: m.get("id") == 2)["result"] == 2
        gate.write_text("go")
        resp1 = None
        saw_ready = False
        deadline = time.time() + _HANG_S
        while resp1 is None or not saw_ready:
            assert time.time() < deadline, "gated call or ready never arrived"
            m = rt.recv(timeout=max(0.1, deadline - time.time()))
            if m.get("id") == 1:
                resp1 = m
            elif (
                m.get("method") == "app.component"
                and m["params"].get("name") == "worker:prewarm_pid"
                and m["params"].get("phase") == "ready"
            ):
                saw_ready = True
        assert resp1["result"] == "answered"


def test_calls_queued_during_prewarm_served_by_member(tmp_path):
    """Calls landing while the prewarmed member is still importing wait
    on the pool queue — never leasing the mid-handshake member and
    never spawning a second one — then the handshake's wake serves
    them FIFO on that same member."""
    gate = tmp_path / "prewarm.gate"
    pf = tmp_path / "prewarmed.pid"
    env = {
        "PYLAIR_PREWARM_GATE": str(gate),
        "PYLAIR_PREWARM_PIDFILE": str(pf),
    }
    with Runtime("testapp_prewarm", env=env) as rt:
        _guard(rt)
        rt.recv()
        _components_until(rt, "worker:prewarm_pid", ["loading"])
        rt.send(call(1, "prewarm_pid", task=1))
        rt.send(call(2, "prewarm_pid", task=2))
        # Dispatch is never blocked by the queued calls — a plain call
        # answers while the member's import stays gated.
        rt.send(call(3, "add", task=3, input={"a": 1, "b": 1}))
        assert rt.recv_until(lambda m: m.get("id") == 3)["result"] == 2
        gate.write_text("go")
        prewarmed = _wait_pidfile(pf)
        got: dict[int, dict] = {}
        deadline = time.time() + _HANG_S
        while 1 not in got or 2 not in got:
            assert time.time() < deadline, "queued calls never answered"
            m = rt.recv(timeout=max(0.1, deadline - time.time()))
            if m.get("id") in (1, 2):
                got[m["id"]] = m
        assert got[1]["result"] == prewarmed
        assert got[2]["result"] == prewarmed


def test_prewarm_false_stays_lazy(tmp_path):
    """A plain warm pool keeps lazy spawning: no component and no
    member process until the first call — even in an app where another
    fn prewarms. A lazy_pid worker writes the lazy pidfile at import,
    so its absence proves no member ever spawned."""
    pf = tmp_path / "lazy.pid"
    with Runtime("testapp_prewarm", env={"PYLAIR_LAZY_PIDFILE": str(pf)}) as rt:
        _guard(rt)
        rt.recv()
        frames = _components_until(rt, "worker:prewarm_pid", ["ready"])
        assert not any(p["name"] == "worker:lazy_pid" for p in frames), (
            "lazy fn declared a component"
        )
        assert not pf.exists(), "a lazy_pid member spawned before the first call"
        rt.send(call(1, "lazy_pid", task=1))
        assert rt.recv()["result"] > 0
        assert _wait_pidfile(pf) > 0


# ---- idle_ttl_s (0008 §4) -------------------------------------------------------
# Fixture ttl is 0.3 s: long enough that a call or a sub-ttl gap always fits,
# short enough that "wait for the reap" is a fast poll. Reap witnesses are the
# member's own pid (gone on _pid_dead, changed on the next call) — no fixed
# sleeps as assertions; sleeps only place a call relative to the boundary.


def test_ttl_idle_member_reaped(runtime):
    """A member idle past idle_ttl_s is reaped through the kill path: the
    pid is gone (no orphan — the member's group, reaper child included,
    dies with it), and the quiet reap emits no frame at all."""
    runtime.recv()
    runtime.send(call(1, "warm_ttl_pid", task=1))
    resp = runtime.recv()
    assert "result" in resp, f"setup call failed: {resp!r}"
    pid = resp["result"]
    kids = _children(pid) if sys.platform != "win32" else []
    _pid_dead(pid)
    for k in kids:
        _pid_dead(k)
    # The reap is not a death: nothing may be emitted for it. A stray
    # frame would sit in the queue and surface here.
    assert runtime._q.empty(), f"idle reap emitted frames: {list(runtime._q.queue)}"
    assert not any("WORKER_DIED" in line for line in runtime.stderr_lines)


def test_ttl_call_within_ttl_reuses_and_resets(runtime):
    """Calls spaced under the ttl reuse the member, and each release
    restarts the clock: five calls over ~0.5 s (> ttl in total, < ttl
    per gap) all land on one pid — then the idle reap takes it."""
    runtime.recv()
    pids = []
    for i in range(5):
        if i:
            time.sleep(0.1)
        runtime.send(call(i + 1, "warm_ttl_pid", task=i + 1))
        pids.append(runtime.recv()["result"])
    assert len(set(pids)) == 1, (
        f"member reaped mid-sequence — idle clock did not reset: {pids}"
    )
    _pid_dead(pids[0])


def test_ttl_busy_member_is_not_reaped(runtime):
    """A call longer than the ttl runs to completion — the timer only
    runs while idle — and the member is reaped only after it idles."""
    runtime.recv()
    runtime.send(call(1, "warm_ttl_sleep", task=1, input={"seconds": 1.0}))
    t0 = time.monotonic()
    resp = runtime.recv()
    assert "result" in resp, f"long call did not complete: {resp!r}"
    assert time.monotonic() - t0 >= 0.9, "call returned early — was it cut?"
    _pid_dead(resp["result"])


def test_ttl_reap_then_respawn_deaths_unchanged(runtime):
    """A reap is not a death: after one, two REAL crashes leave the pool
    serving — had the reap counted, deaths would hit the limit early.
    The post-reap call also proves the pool shrank to zero and respawned
    lazily on a new pid."""
    runtime.recv()
    runtime.send(call(1, "warm_ttl_maybe_crash", task=1, input={"crash": False}))
    pid1 = runtime.recv()["result"]
    _pid_dead(pid1)  # the reap ran — and counted for nothing
    runtime.send(call(2, "warm_ttl_maybe_crash", task=2, input={"crash": True}))
    assert runtime.recv()["error"]["code"] == proto.WORKER_DIED
    runtime.send(call(3, "warm_ttl_maybe_crash", task=3, input={"crash": True}))
    assert runtime.recv()["error"]["code"] == proto.WORKER_DIED
    # deaths == 2, not 3 — the pool is still alive.
    runtime.send(call(4, "warm_ttl_maybe_crash", task=4, input={"crash": False}))
    resp = runtime.recv()
    assert "result" in resp, f"pool stopped — an idle reap counted as a death: {resp!r}"
    assert resp["result"] != pid1, "respawn after the reap reused a dead pid"


def test_ttl_reap_race_calls_succeed(runtime):
    """Drive calls around the reap moment: before it (reuse), at it
    (acquire races the mark), just after (kill in flight or pool already
    empty -> lazy spawn). The atomic mark means a member mid-reap is
    never handed out, so every call simply succeeds — 50 sweeps plus a
    queued pair every seventh to cover the wait path."""
    runtime.recv()
    rid = 100
    for i in range(50):
        time.sleep((0.15, 0.3, 0.3, 0.38, 0.45)[i % 5])
        n_calls = 2 if i % 7 == 6 else 1
        for _ in range(n_calls):
            runtime.send(call(rid, "warm_ttl_pid", task=rid))
            rid += 1
        want = set(range(rid - n_calls, rid))
        got = _collect(runtime, want)
        for r in sorted(got):
            assert "result" in got[r], (
                f"iteration {i}: call {r} failed mid-reap: {got[r]!r}"
            )


def test_prewarm_pool_still_reaps_and_stays_ready(tmp_path):
    """prewarm + ttl (0008 §4/§6): the startup member completes its
    handshake (component ready), idles, and is reaped — no re-prewarm.
    The worker:<fn> component stays ready throughout (the app is still
    ready; the next call just pays a cold start on a fresh member, whose
    own handshake re-reports ready)."""
    pf = tmp_path / "ttl.pid"
    with Runtime(
        "testapp_prewarm_ttl", env={"PYLAIR_PREWARM_TTL_PIDFILE": str(pf)}
    ) as rt:
        _guard(rt)
        rt.recv()
        _components_until(rt, "worker:prewarm_ttl_pid", ["loading", "ready"])
        prewarmed = _wait_pidfile(pf)
        _pid_dead(prewarmed)
        rt.send(call(1, "prewarm_ttl_pid", task=1))
        # Collect the cold-start response AND the component frames: the
        # respawned member's handshake re-reports ready (same phase, so
        # it rides the next flush interval); a failed phase would land
        # in `phases` here — after the reap nothing should ever report
        # one.
        resp = None
        phases: list[str] = []
        deadline = time.time() + _HANG_S
        while resp is None or not phases:
            assert time.time() < deadline, (
                f"cold-start respawn or re-reported ready never arrived: {phases}"
            )
            m = rt.recv(timeout=max(0.1, deadline - time.time()))
            if m.get("id") == 1:
                resp = m
            elif (
                m.get("method") == "app.component"
                and m["params"].get("name") == "worker:prewarm_ttl_pid"
            ):
                phases.append(m["params"]["phase"])
        assert resp["result"] > 0
        assert resp["result"] != prewarmed, (
            "post-reap call landed on the reaped member — not a cold start"
        )
        assert phases == ["ready"], f"component report across the reap: {phases}"


def test_ttl_reap_frees_cap_slot():
    """The reaped member releases its max_isolated_workers slot: with
    cap=1 a process-mode call is refused while the member lives and fits
    once the idle reap has run."""
    with Runtime(env={"PYLAIR_MAX_ISOLATED_WORKERS": "1"}) as rt:
        _guard(rt)
        rt.recv()
        rt.send(call(1, "warm_ttl_pid", task=1))
        pid = rt.recv()["result"]
        rt.send(call(2, "isolated_add", task=2, input={"a": 1, "b": 2}))
        resp = rt.recv()
        assert resp["error"]["code"] == proto.LIMIT_EXCEEDED
        # On POSIX _pid_dead is the reap event; on Windows it's a real
        # tasklist poll now, so this is the same witness on both.
        _pid_dead(pid)
        deadline = time.time() + _HANG_S
        for i in range(3, 200):
            rt.send(call(i, "isolated_add", task=i, input={"a": 1, "b": 2}))
            resp = rt.recv()
            if "result" in resp:
                assert resp["result"] == 3
                return
            assert resp["error"]["data"]["limit"] == "max_isolated_workers", (
                f"unexpected refusal after reap: {resp!r}"
            )
            assert time.time() < deadline, "cap slot never freed after idle reap"
            time.sleep(0.1)
