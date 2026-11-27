"""pylair.binary() and pylair.proc (decision 0014 sections 3-4).

Unit tests monkeypatch PYLAIR_BINARIES and drop the parse-once cache;
the cancellation tie-in is proven end to end through the wire harness:
the terminal frame is only sent after the tree kill completes, so
asserting death right after the terminal arrives is deterministic.
"""

from __future__ import annotations

import asyncio
import json
import os
import sys
import time
from pathlib import Path

import pytest

import pylair
from conftest import Runtime
from pylair import _protocol as proto
from pylair import proc as proc_mod


def call(req_id, fn, task, input=None):
    return {
        "jsonrpc": "2.0",
        "id": req_id,
        "method": "call",
        "params": {"task": task, "fn": fn, "input": input or {}},
    }


@pytest.fixture(autouse=True)
def _reset_binary_cache():
    proc_mod._reset_cache()
    yield
    proc_mod._reset_cache()


def _pid_alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
        return True
    except OSError:
        return False


def _wait_file(path: Path, timeout: float = 15.0) -> str:
    deadline = time.time() + timeout
    while time.time() < deadline:
        if path.exists() and path.read_text().strip():
            return path.read_text().strip()
        time.sleep(0.05)
    raise AssertionError(f"{path} never appeared")


def _wait_dead(pid: int, timeout: float = 8.0) -> None:
    deadline = time.time() + timeout
    while time.time() < deadline:
        if not _pid_alive(pid):
            return
        time.sleep(0.1)
    raise AssertionError(f"pid {pid} still alive after {timeout}s")


# ---- binary() -------------------------------------------------------------


def test_binary_resolves_from_env(monkeypatch):
    monkeypatch.setenv(
        "PYLAIR_BINARIES",
        json.dumps({"ffmpeg": "/opt/app/ffmpeg", "llama": "/opt/app/llama"}),
    )
    assert pylair.binary("ffmpeg") == Path("/opt/app/ffmpeg")
    assert pylair.binary("llama") == Path("/opt/app/llama")


def test_binary_unknown_name_keyerror_lists_available(monkeypatch):
    monkeypatch.setenv("PYLAIR_BINARIES", json.dumps({"ffmpeg": "/x/ffmpeg"}))
    with pytest.raises(KeyError) as ei:
        pylair.binary("nope")
    msg = str(ei.value)
    assert "unknown binary 'nope'" in msg
    assert "ffmpeg" in msg, "available names must be listed"


def test_binary_absent_env_is_runtimeerror_naming_the_binary(monkeypatch):
    monkeypatch.delenv("PYLAIR_BINARIES", raising=False)
    with pytest.raises(RuntimeError) as ei:
        pylair.binary("ffmpeg")
    msg = str(ei.value)
    assert "ffmpeg" in msg
    assert "not resolved by the host" in msg


def test_binary_malformed_env_is_runtimeerror(monkeypatch):
    monkeypatch.setenv("PYLAIR_BINARIES", "{not json")
    with pytest.raises(RuntimeError, match="not resolved by the host"):
        pylair.binary("ffmpeg")
    monkeypatch.setenv("PYLAIR_BINARIES", json.dumps(["a", "b"]))  # not a dict
    proc_mod._reset_cache()
    with pytest.raises(RuntimeError, match="not resolved by the host"):
        pylair.binary("ffmpeg")


def test_binary_cache_is_parse_once(monkeypatch):
    monkeypatch.setenv("PYLAIR_BINARIES", json.dumps({"ffmpeg": "/first"}))
    assert pylair.binary("ffmpeg") == Path("/first")
    monkeypatch.setenv("PYLAIR_BINARIES", json.dumps({"ffmpeg": "/second"}))
    # No reset: the cached map is still in effect (fixed at spawn).
    assert pylair.binary("ffmpeg") == Path("/first")


# ---- spawn basics (in-process) --------------------------------------------


def test_spawn_by_path_lines_and_wait():
    async def go():
        proc = await pylair.proc.spawn(
            sys.executable, "-c", "print('alpha');print('beta')"
        )
        lines = []
        assert proc.stdout is not None
        async for line in proc.stdout:
            lines.append(line.rstrip())
        rc = await proc.wait()
        return lines, rc, proc

    lines, rc, proc = asyncio.run(go())
    assert lines == ["alpha", "beta"]
    assert rc == 0
    assert proc.returncode == 0
    assert proc.pid > 0


def test_spawn_by_name_resolves_through_binary(monkeypatch):
    monkeypatch.setenv("PYLAIR_BINARIES", json.dumps({"mypython": sys.executable}))

    async def go():
        proc = await pylair.proc.spawn("mypython", "-c", "print('named')")
        assert proc.stdout is not None
        first = await proc.stdout.__anext__()
        await proc.wait()
        return first

    assert asyncio.run(go()).rstrip() == "named"


def test_spawn_stderr_pipe_capture():
    async def go():
        proc = await pylair.proc.spawn(
            sys.executable,
            "-c",
            "import sys; sys.stderr.write('errline\\n')",
            stderr=pylair.proc.PIPE,
        )
        assert proc.stderr is not None
        lines = [line async for line in proc.stderr]
        await proc.wait()
        return lines

    lines = asyncio.run(go())
    assert [line.rstrip() for line in lines] == ["errline"]


def test_spawn_stderr_default_inherits_no_iterator():
    async def go():
        proc = await pylair.proc.spawn(sys.executable, "-c", "pass")
        await proc.wait()
        return proc

    proc = asyncio.run(go())
    assert proc.stderr is None, "inherited stderr must not expose an iterator"


def test_kill_terminates():
    async def go():
        proc = await pylair.proc.spawn(
            sys.executable, "-c", "import time; time.sleep(300)"
        )
        proc.kill()
        rc = await proc.wait()
        return rc

    rc = asyncio.run(go())
    assert rc != 0


def test_spawn_outside_task_does_not_register():
    async def go():
        before = sum(len(v) for v in proc_mod._task_children.values())
        proc = await pylair.proc.spawn(sys.executable, "-c", "pass")
        await proc.wait()
        during = sum(len(v) for v in proc_mod._task_children.values())
        return before, during

    before, during = asyncio.run(go())
    assert before == during == 0, "a spawn outside any task belongs to the runtime"


def test_raw_access_exposes_process_and_reader():
    async def go():
        proc = await pylair.proc.spawn(sys.executable, "-c", "print('raw')")
        assert isinstance(proc.process, asyncio.subprocess.Process)
        assert proc.stdout is not None
        assert isinstance(proc.stdout.reader, asyncio.StreamReader)
        await proc.wait()

    asyncio.run(go())


# ---- cancellation tie-in (wire harness) -------------------------------------


def test_cancel_kills_tree_including_grandchild(runtime, tmp_path):
    """The headlining guarantee (0014 sec. 4): a cancelled task's sidecar
    tree - grandchild included - dies BEFORE TASK_CANCELLED is reported,
    on every OS. The terminal frame is the proof the kill completed."""
    runtime.recv()
    child_pf = tmp_path / "child.pid"
    gc_pf = tmp_path / "grandchild.pid"
    runtime.send(
        call(
            1,
            "proc_tree_sleep",
            task=1,
            input={
                "child_pidfile": str(child_pf),
                "grandchild_pidfile": str(gc_pf),
                "workdir": str(tmp_path),
            },
        )
    )
    child_pid = int(_wait_file(child_pf))
    gc_pid = int(_wait_file(gc_pf))
    assert _pid_alive(child_pid) and _pid_alive(gc_pid)

    runtime.send({"jsonrpc": "2.0", "id": 2, "method": "cancel", "params": {"task": 1}})
    terminal = runtime.recv_until(lambda m: m.get("id") == 1)
    assert terminal["error"]["code"] == proto.TASK_CANCELLED
    # The terminal is only sent after kill_tree completes: both pids must
    # already be dead, not merely dying. A small window absorbs reaping lag
    # on starved machines, but on a healthy machine this is instant.
    _wait_dead(child_pid)
    _wait_dead(gc_pid)


def test_normal_completion_leaves_child_running(tmp_path):
    """Servers outlive the call that starts them: a task that finishes
    normally must not kill its registered child."""
    with Runtime() as runtime:
        runtime.recv()
        pidfile = tmp_path / "sleeper.pid"
        runtime.send(
            call(
                1,
                "proc_spawn_then_finish",
                task=1,
                input={"pidfile": str(pidfile), "workdir": str(tmp_path)},
            )
        )
        resp = runtime.recv_until(lambda m: m.get("id") == 1)
        pid = int(resp["result"])
        deadline = time.time() + 10
        while not pidfile.exists():
            assert time.time() < deadline, "sleeper never wrote its pid"
            time.sleep(0.05)
        assert _pid_alive(pid), "normal completion must not kill the child"
        # Cleanup: kill it ourselves so the test never leaks.
        if sys.platform == "win32":
            import subprocess as sp

            sp.run(
                ["taskkill", "/PID", str(pid), "/T", "/F"],
                capture_output=True,
                check=False,
            )
        else:
            try:
                os.kill(pid, 9)
            except OSError:
                pass


def test_line_iteration_through_the_wire(runtime):
    runtime.recv()
    runtime.send(call(1, "proc_cat_lines", task=1))
    resp = runtime.recv_until(lambda m: m.get("id") == 1)
    assert resp["result"] == ["one", "two", "three"]


def test_workers_resolve_binaries(tmp_path):
    """Warm and isolated workers inherit the runtime's env: the same map,
    no extra code (0014 sec. 3, same rule as models.path)."""
    env = {"PYLAIR_BINARIES": json.dumps({"ffmpeg": "/opt/app/ffmpeg"})}
    with Runtime(env=env) as runtime:
        runtime.recv()
        runtime.send(call(1, "proc_binary_path", task=1, input={"name": "ffmpeg"}))
        runtime.send(
            call(2, "proc_binary_path_isolated", task=2, input={"name": "ffmpeg"})
        )
        runtime.send(call(3, "proc_binary_path_warm", task=3, input={"name": "ffmpeg"}))
        results = {}
        deadline = time.time() + 30
        while len(results) < 3:
            m = runtime.recv(timeout=max(0.1, deadline - time.time()))
            if m.get("id") in (1, 2, 3) and "result" in m:
                results[m["id"]] = m["result"]
            elif m.get("id") in (1, 2, 3) and "error" in m:
                pytest.fail(f"call {m['id']} errored: {m['error']}")
            assert time.time() < deadline, "calls never finished"
        # Path normalizes separators per platform; compare normalized.
        expected = str(Path("/opt/app/ffmpeg"))
        for kind, rid in (("runtime", 1), ("isolated", 2), ("warm", 3)):
            assert results[rid] == expected, f"{kind} worker resolution"
