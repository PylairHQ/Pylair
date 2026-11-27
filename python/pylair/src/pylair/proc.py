"""External binaries: resolution (`pylair.binary`) and running them (`pylair.proc`).

Decision 0014 sections 3-4. The host resolves declared binaries and hands
the runtime a JSON map in PYLAIR_BINARIES (name -> absolute path); there
is no PATH fallback anywhere in the design. `spawn` is a thin wrapper over
asyncio subprocesses whose reason to exist is the cancellation tie-in:
children registered inside a task are tree-killed before TASK_CANCELLED
is reported, while children of a normally-completing task are left running
(servers outlive the call that starts them).
"""

from __future__ import annotations

import asyncio
import contextvars
import json
import os
import signal
import sys
from pathlib import Path
from typing import Any

# Grace for a sidecar tree on cancel: SIGTERM, then SIGKILL. Mirrors the
# worker-kill budget in _tasks (ISOLATED_GRACE_S).
KILL_GRACE_S = 3.0
# Same bound as the worker path: a hung taskkill falls back to proc.kill().
TASKKILL_TIMEOUT_S = 5.0

# Worker detection, same env channel models.py uses: workers inherit the
# runtime's environment, so binary() resolves there unchanged - and the
# process-group rules differ (see spawn).
_ENV_WORKER = "PYLAIR_WORKER_PARENT"


# ---- binary() -------------------------------------------------------------

_BINARIES_UNSET = object()
_binaries_cache: Any = _BINARIES_UNSET


def _binaries() -> dict[str, str] | None:
    """The parsed PYLAIR_BINARIES map, parsed once and cached. None when
    the variable is absent or malformed - binary() turns that into the
    RuntimeError every caller sees."""
    global _binaries_cache
    if _binaries_cache is not _BINARIES_UNSET:
        return _binaries_cache
    raw = os.environ.get("PYLAIR_BINARIES")
    parsed: dict[str, str] | None = None
    if raw:
        try:
            value = json.loads(raw)
            if isinstance(value, dict) and all(
                isinstance(k, str) and isinstance(v, str) for k, v in value.items()
            ):
                parsed = value
        except ValueError:
            parsed = None
    _binaries_cache = parsed
    return parsed


def _reset_cache() -> None:
    """Test hook: the env var is process state, so tests that monkeypatch
    it must also drop the parse-once cache. Production never resets - the
    map is fixed at spawn."""
    global _binaries_cache
    _binaries_cache = _BINARIES_UNSET


def binary(name: str) -> Path:
    """The host-resolved absolute path of a declared binary (0014 sec. 3).

    KeyError lists the available names; a missing or malformed
    PYLAIR_BINARIES is a RuntimeError - the binary was not resolved by
    the host (running outside a Pylair spawn, or no binaries declared).
    """
    resolved = _binaries()
    if resolved is None:
        raise RuntimeError(
            f"binary {name!r} was not resolved by the host: PYLAIR_BINARIES "
            "is not set or is malformed (running outside a Pylair spawn, or "
            "no binaries declared)"
        )
    try:
        return Path(resolved[name])
    except KeyError:
        available = ", ".join(sorted(resolved)) or "(none)"
        raise KeyError(f"unknown binary {name!r}; available: {available}") from None


# ---- task registry tie-in -------------------------------------------------

# The task id of the bridge task currently running user code, set by the
# task runner at creation (asyncio.create_task copies the context, so the
# whole runner - gated waits included - sees it). None outside a task.
_current_task_id: contextvars.ContextVar[int | str | None] = contextvars.ContextVar(
    "pylair_proc_task_id", default=None
)

# task id -> live Proc objects spawned inside it. A child is discarded
# when it exits; the whole entry is dropped when the task finishes -
# killed first when the finish is a cancellation (0014 sec. 4).
_task_children: dict[int | str, set[Proc]] = {}


def _register_child(task_id: int | str, proc: Proc) -> None:
    _task_children.setdefault(task_id, set()).add(proc)


def _discard_child(task_id: int | str, proc: Proc) -> None:
    children = _task_children.get(task_id)
    if children is not None:
        children.discard(proc)


def _pop_children(task_id: int | str) -> set[Proc]:
    """All still-registered children of a finishing task. Live ones are
    in the set; exited ones already discarded themselves."""
    return _task_children.pop(task_id, set())


# ---- kill tree ------------------------------------------------------------


def _killpg(pid: int, sig: int) -> None:
    os.killpg(pid, sig)


async def kill_tree(
    proc: asyncio.subprocess.Process, grace_s: float = KILL_GRACE_S
) -> None:
    """Kill proc's whole process tree, tolerating a dead or dying target.

    POSIX: the child is a session leader (spawn sets start_new_session),
    so killpg hits it and every grandchild in its group - SIGTERM, a
    short grace, then SIGKILL whether or not the leader exited, because
    a grandchild ignoring SIGTERM must not survive. ESRCH means the
    group is already gone; EPERM is what macOS raises on a zombie's
    group - either way the bounded reap below is still correct (the same
    tolerance _tasks._kill_proc documents). Windows: taskkill /T /F
    walks the parent-child chain - the documented equivalent of a Job
    Object terminate, already proven by the worker kill path - with a
    bounded wait and a proc.kill() fallback.
    """
    if proc.returncode is not None:
        return
    if sys.platform == "win32":
        try:
            tk = await asyncio.create_subprocess_exec(
                "taskkill",
                "/PID",
                str(proc.pid),
                "/T",
                "/F",
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.DEVNULL,
            )
            rc = await asyncio.wait_for(tk.wait(), timeout=TASKKILL_TIMEOUT_S)
        except (TimeoutError, OSError):
            rc = -1
        if rc != 0:
            try:
                proc.kill()
            except (ProcessLookupError, PermissionError):
                pass
        try:
            await asyncio.wait_for(proc.wait(), timeout=grace_s)
        except TimeoutError:
            try:
                proc.kill()
            except (ProcessLookupError, PermissionError):
                pass
            try:
                await asyncio.wait_for(proc.wait(), timeout=1.0)
            except TimeoutError:
                pass
        return
    try:
        _killpg(proc.pid, signal.SIGTERM)
    except (ProcessLookupError, PermissionError):
        pass
    try:
        await asyncio.wait_for(proc.wait(), timeout=grace_s)
    except TimeoutError:
        pass
    try:
        _killpg(proc.pid, signal.SIGKILL)
    except (ProcessLookupError, PermissionError):
        pass
    try:
        proc.kill()
    except (ProcessLookupError, PermissionError):
        pass
    try:
        await proc.wait()
    except (ProcessLookupError, PermissionError):
        pass


# ---- spawn / Proc ----------------------------------------------------------

PIPE = asyncio.subprocess.PIPE
DEVNULL = asyncio.subprocess.DEVNULL


class _Lines:
    """Async iterator over a stream's text lines (utf-8, errors=replace).
    `.reader` is the raw StreamReader for byte-level consumers."""

    def __init__(self, reader: asyncio.StreamReader) -> None:
        self.reader = reader

    def __aiter__(self) -> _Lines:
        return self

    async def __anext__(self) -> str:
        line = await self.reader.readline()
        if not line:
            raise StopAsyncIteration
        return line.decode("utf-8", errors="replace")


class Proc:
    """A running sidecar process (0014 sec. 4).

    stdout/stderr are async line iterators when piped (stderr defaults to
    inherit, so sidecar stderr lands in the dev inspector timeline with
    the runtime's own); wait() is the return code, kill() terminates the
    process (not the tree - tree kills are the cancel path's business),
    pid and the raw asyncio Process are exposed.
    """

    def __init__(self, process: asyncio.subprocess.Process) -> None:
        self.process = process
        self.pid: int = process.pid
        self.stdout: _Lines | None = (
            _Lines(process.stdout) if process.stdout is not None else None
        )
        self.stderr: _Lines | None = (
            _Lines(process.stderr) if process.stderr is not None else None
        )
        self._task_id = _current_task_id.get()
        if self._task_id is not None:
            _register_child(self._task_id, self)
            # Unregister on exit so an already-finished child is never
            # pointlessly signalled at cancel time. shielded from task
            # cancellation: the watcher must outlive the owning task.
            asyncio.get_running_loop().create_task(self._watch_exit())

    async def _watch_exit(self) -> None:
        try:
            await self.process.wait()
        except (ProcessLookupError, PermissionError):
            pass
        if self._task_id is not None:
            _discard_child(self._task_id, self)

    @property
    def returncode(self) -> int | None:
        return self.process.returncode

    async def wait(self) -> int:
        return await self.process.wait()

    def kill(self) -> None:
        self.process.kill()

    def terminate(self) -> None:
        self.process.terminate()

    def __hash__(self) -> int:  # registry membership is by identity
        return id(self)


def _resolve_exe(exe: str | os.PathLike[str]) -> str:
    """A binary name resolves through pylair.binary; anything path-shaped
    (a Path, or a string containing a separator) is used as given. Names
    cannot contain separators, so the two never collide."""
    if isinstance(exe, os.PathLike):
        return os.fspath(exe)
    if os.sep in exe or (os.altsep and os.altsep in exe) or "/" in exe:
        return exe
    return str(binary(exe))


async def spawn(
    exe: str | os.PathLike[str],
    *args: str,
    stdout: Any = PIPE,
    stderr: Any = None,
    stdin: Any = None,
    cwd: str | os.PathLike[str] | None = None,
    env: dict[str, str] | None = None,
) -> Proc:
    """Spawn an external binary (0014 sec. 4). Async because registration
    and process creation both belong to the running loop.

    `exe` is a declared binary name (resolved via pylair.binary) or an
    explicit path. stderr=None inherits, so sidecar stderr lands in the
    dev inspector timeline; PIPE captures for machine consumption.

    POSIX: the child is a session leader so the cancel path can killpg
    exactly its tree - except inside a worker, where it must stay in the
    worker's own group: the runtime cancels worker tasks by killing the
    worker's group, and a re-parented session would escape that kill
    (and the worker-exit group sweep). Windows needs neither - taskkill
    /T walks the tree either way.
    """
    path = _resolve_exe(exe)
    # stdin=None means DEVNULL, never inherit: the runtime's own stdin is
    # the host protocol frame channel, so an inheriting child would eat
    # frames the host sends - and on Windows an inherited stdin pipe also
    # keeps the child from being reaped (asyncio proactor never observes
    # the exit). Feeding input means passing PIPE explicitly.
    kwargs: dict[str, Any] = {
        "stdout": stdout,
        "stderr": stderr,
        "stdin": stdin if stdin is not None else asyncio.subprocess.DEVNULL,
        "cwd": cwd,
        "env": env,
    }
    if sys.platform != "win32" and not os.environ.get(_ENV_WORKER):
        kwargs["start_new_session"] = True
    process = await asyncio.create_subprocess_exec(path, *args, **kwargs)
    return Proc(process)
