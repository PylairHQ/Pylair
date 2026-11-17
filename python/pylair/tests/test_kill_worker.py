"""Windows worker-kill path: taskkill is awaited with a bound, its exit
code is checked, and a failure or hang falls back to proc.kill() - the
loop never parks on a worker that refuses to die. The win32 branch is
exercised on every OS by stubbing sys.platform and the taskkill exec;
one real test runs only on Windows."""

from __future__ import annotations

import asyncio
import sys

import pytest

from pylair import _tasks
from pylair._tasks import Task, TaskRegistry


class _FakeProc:
    """Stands in for asyncio.subprocess.Process - the worker or the
    taskkill helper, depending on where it is plugged in."""

    def __init__(self, pid: int = 4321, rc: int = 0, delay: float = 0.0) -> None:
        self.pid = pid
        self.returncode: int | None = None
        self.rc = rc
        self.delay = delay
        self.kills = 0
        self.wait_calls = 0

    def kill(self) -> None:
        self.kills += 1

    async def wait(self) -> int:
        self.wait_calls += 1
        await asyncio.sleep(self.delay)
        self.returncode = self.rc
        return self.returncode


def _registry() -> TaskRegistry:
    return TaskRegistry(lambda _msg: None)


def _task_with(worker: _FakeProc) -> Task:
    task = Task(id=1, fn_name="f", kind="isolated", call_id=1)
    task.worker = worker  # type: ignore[assignment]
    return task


def _run_win32_kill(monkeypatch: pytest.MonkeyPatch, task: Task) -> None:
    """Run _kill_worker as-if on win32. The platform is faked INSIDE the
    coroutine: asyncio.run must create the loop under the real platform -
    newest CPython picks the event-loop class lazily, so a win32 stub at
    loop-creation time imports _overlapped and dies on non-Windows."""

    async def go() -> None:
        monkeypatch.setattr(sys, "platform", "win32")
        await _registry()._kill_worker(task)

    asyncio.run(go())


def _patch_taskkill(monkeypatch: pytest.MonkeyPatch, tk: _FakeProc) -> None:
    async def fake_exec(*_args, **_kwargs):
        return tk

    monkeypatch.setattr(asyncio, "create_subprocess_exec", fake_exec)


def test_taskkill_success_reaps_without_fallback(monkeypatch):
    worker, tk = _FakeProc(), _FakeProc(rc=0)
    _patch_taskkill(monkeypatch, tk)
    _run_win32_kill(monkeypatch, _task_with(worker))
    assert tk.wait_calls == 1, "taskkill was not awaited"
    assert worker.kills == 0, "fell back to proc.kill() on a clean taskkill"
    assert worker.wait_calls >= 1


def test_taskkill_nonzero_falls_back_to_proc_kill(monkeypatch):
    worker, tk = _FakeProc(), _FakeProc(rc=1)
    _patch_taskkill(monkeypatch, tk)
    _run_win32_kill(monkeypatch, _task_with(worker))
    assert worker.kills >= 1, "no fallback kill after nonzero taskkill"
    assert worker.returncode is not None, "worker wait was never awaited"


def test_taskkill_hang_times_out_then_falls_back(monkeypatch):
    worker = _FakeProc()
    tk = _FakeProc(delay=60.0)  # taskkill never answers in time
    _patch_taskkill(monkeypatch, tk)
    monkeypatch.setattr(_tasks, "TASKKILL_TIMEOUT_S", 0.05)
    _run_win32_kill(monkeypatch, _task_with(worker))
    assert worker.kills >= 1, "no fallback kill after taskkill timeout"


def test_taskkill_exec_failure_falls_back_to_proc_kill(monkeypatch):
    async def boom(*_args, **_kwargs):
        raise OSError("taskkill vanished")

    monkeypatch.setattr(asyncio, "create_subprocess_exec", boom)
    worker = _FakeProc()
    _run_win32_kill(monkeypatch, _task_with(worker))
    assert worker.kills >= 1


# ---- cancel-ack classification (the macOS flake) ----------------------
#
# _report_isolated_cancel answers "terminated" only when the task's
# asyncio task finishes inside a bound strictly under the host's 5 s
# cancel_timeout (ISOLATED_GRACE_S + 1.0). On a starved runner the kill
# can still be in flight at the bound; the honest, documented answer is
# "termination_in_progress" and the kill continues. These tests force
# the ordering deterministically: the 'reap' is a gate the test opens,
# never a sleep.


def _cancelling_task(reap_gate: asyncio.Event) -> asyncio.Task:
    """A stand-in for _run_isolated: on cancel it enters a 'reap' phase
    that completes only when the test opens the gate."""

    async def go() -> None:
        try:
            await asyncio.Event().wait()  # the work: never finishes
        except asyncio.CancelledError:
            await reap_gate.wait()  # the reap: finishes when the test says

    return asyncio.create_task(go())


async def _cancel_and_await_ack(task: Task) -> dict:
    """Register `task` on a fresh registry, cancel it through the real
    dispatch path, and await the ack frame."""
    ack: asyncio.Future = asyncio.get_running_loop().create_future()

    def send(m: dict) -> None:
        if m.get("id") == 2 and not ack.done():
            ack.set_result(m)

    reg = TaskRegistry(send)
    reg._tasks[task.id] = task
    # Let the task reach its suspension point first: cancelling a task
    # that never ran sets _must_cancel and ends it as CANCELLED no
    # matter what it catches, while a suspended task takes the
    # CancelledError through its handler and finishes normally — the
    # shape a real in-flight isolated call has.
    await asyncio.sleep(0)
    reg.cancel(task.id, call_id=2)
    return await asyncio.wait_for(ack, timeout=10)


def test_slow_reap_reports_termination_in_progress(monkeypatch):
    """Deterministic repro of the CI flake: the reap is still running at
    the ack bound, so the response is the documented unconfirmed outcome
    while the kill keeps running to completion."""
    monkeypatch.setattr(
        _tasks, "ISOLATED_GRACE_S", -0.95
    )  # bound 4.0 -> 0.05 s: the gate stays shut past it

    async def go() -> dict:
        task = Task(id=1, fn_name="f", kind="isolated", call_id=1)
        reap_gate = asyncio.Event()
        task.asyncio_task = _cancelling_task(reap_gate)
        ack = await _cancel_and_await_ack(task)
        # The kill is not lost: once the reap completes the task finishes
        # and _ensure_terminal delivers the call's terminal.
        reap_gate.set()
        await task.asyncio_task
        await asyncio.sleep(0)  # let the finisher run _finish
        return ack

    ack = asyncio.run(go())
    assert ack["result"] == {
        "cancelled": True,
        "status": "termination_in_progress",
    }


def test_fast_reap_reports_terminated():
    """Same path with the gate already open: the confirmed answer."""

    async def go() -> tuple[dict, asyncio.Task]:
        task = Task(id=1, fn_name="f", kind="isolated", call_id=1)
        reap_gate = asyncio.Event()
        reap_gate.set()  # 'reap' completes immediately
        task.asyncio_task = _cancelling_task(reap_gate)
        ack = await _cancel_and_await_ack(task)
        await asyncio.sleep(0)  # let the finisher run _finish
        return ack, task.asyncio_task

    ack, asyncio_task = asyncio.run(go())
    assert ack["result"] == {"cancelled": True, "status": "terminated"}
    assert asyncio_task.done()


@pytest.mark.skipif(sys.platform != "win32", reason="taskkill is Windows-only")
def test_real_taskkill_reaps_worker():
    """The actual win32 path against a real sleeping process."""

    async def go() -> None:
        proc = await asyncio.create_subprocess_exec(
            sys.executable, "-c", "import time; time.sleep(60)"
        )
        task = Task(id=1, fn_name="f", kind="isolated", call_id=1)
        task.worker = proc
        await asyncio.wait_for(_registry()._kill_worker(task), timeout=15)
        assert proc.returncode is not None, "worker still alive after taskkill"

    asyncio.run(go())
