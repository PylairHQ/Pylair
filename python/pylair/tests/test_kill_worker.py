"""Windows worker-kill path: taskkill is awaited with a bound, its exit
code is checked, and a failure or hang falls back to proc.kill() — the
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
    """Stands in for asyncio.subprocess.Process — the worker or the
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
