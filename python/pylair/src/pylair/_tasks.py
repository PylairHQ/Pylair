"""Task registry: in-flight calls, credit-gated stream pumps, isolated workers."""

from __future__ import annotations

import asyncio
import os
import signal
import sys
import time
from collections.abc import AsyncGenerator, Callable
from dataclasses import dataclass, field
from typing import Any

from pylair import _json
from pylair import _protocol as proto

ISOLATED_GRACE_S = 3.0
# taskkill is awaited with this bound — a hung taskkill or a nonzero
# exit falls back to proc.kill() rather than parking the loop forever.
TASKKILL_TIMEOUT_S = 5.0

# RC.8A ceilings — the host enforces these on the way in; the runtime
# clamps defensively so a host bug can't grow them without bound.
MAX_STREAM_CREDIT = int(os.environ.get("PYLAIR_MAX_STREAM_CREDIT", "4096"))
MAX_ISOLATED_WORKERS = int(os.environ.get("PYLAIR_MAX_ISOLATED_WORKERS", "8"))
# One chunk frame can never legitimately exceed the frame cap; a value
# this large ends the stream with a typed error instead (RC.8A).
from pylair._transport import MAX_DEPTH, MAX_FRAME


def _log(msg: str) -> None:
    print(f"[pylair] {msg}", file=sys.stderr, flush=True)


def _is_int_id(task_id: Any) -> bool:
    return isinstance(task_id, int) and not isinstance(task_id, bool)


@dataclass
class Task:
    id: int | str
    fn_name: str
    kind: str
    call_id: int | str  # the wire request id this task answers
    seq: int = 0
    credit: int = 0
    credit_event: asyncio.Event = field(default_factory=asyncio.Event)
    asyncio_task: asyncio.Task | None = None
    worker: asyncio.subprocess.Process | None = None
    done: bool = False


class TaskRegistry:
    def __init__(self, send: Callable[[dict], None]) -> None:
        self._send = send
        self._tasks: dict[int | str, Task] = {}
        # The host's control lane can outrun the call that registers a task:
        # hold cancels/credit for not-yet-known ids and apply them at start.
        self._pending_cancels: set[int | str] = set()
        self._pending_credit: dict[int | str, int] = {}
        # High-water mark of integer task ids whose call frame was
        # handled. Host ids only increase and frames arrive in order, so
        # an unknown id <= the mark is finished-or-rejected, never early.
        self._highest_task_id = 0
        self._isolated = 0
        # Set by the server on `shutdown`: worker kills then draw their
        # wait from the shared grace budget, never a fresh ISOLATED_GRACE_S.
        self.deadline: float | None = None

    def _budget(self, cap: float) -> float:
        """A phase's cap shrunk to whatever the shared deadline leaves."""
        if self.deadline is None:
            return cap
        return min(cap, max(self.deadline - time.monotonic(), 0.0))

    def note_call_id(self, task_id: int | str) -> None:
        """Advance the id high-water mark and drop dead early-entries.

        A handled call for T proves every earlier-issued call already
        arrived, so pendings below T can never be claimed — pruning them
        keeps the sets proportional to the in-flight window instead of
        everything the host ever cancelled.
        """
        if not _is_int_id(task_id) or task_id <= self._highest_task_id:
            return
        self._highest_task_id = task_id
        self._pending_cancels = {
            t for t in self._pending_cancels if not (_is_int_id(t) and t < task_id)
        }
        self._pending_credit = {
            t: n
            for t, n in self._pending_credit.items()
            if not (_is_int_id(t) and t < task_id)
        }

    def get(self, task_id: int | str) -> Task | None:
        return self._tasks.get(task_id)

    def all(self) -> list[Task]:
        return list(self._tasks.values())

    async def start(
        self,
        task_id: int | str,
        call_id: int | str,
        fn_name: str,
        kind: str,
        fn: Callable[..., Any],
        input: dict,
    ) -> None:
        if kind == "isolated" and self._isolated >= MAX_ISOLATED_WORKERS:
            # Worker processes are the heaviest resource — refuse past the
            # cap with the typed error, don't queue (RC.8A).
            self._send(
                proto.error_response(
                    call_id,
                    proto.LIMIT_EXCEEDED,
                    f"isolated worker cap {MAX_ISOLATED_WORKERS} reached",
                    {"limit": "max_isolated_workers", "max": MAX_ISOLATED_WORKERS},
                )
            )
            return
        task = Task(id=task_id, fn_name=fn_name, kind=kind, call_id=call_id)
        self._tasks[task_id] = task
        if kind == "isolated":
            self._isolated += 1

        if kind == "generator":
            # Credit is owned by the HOST: it grants the initial window right
            # after `call` (protocol §3.1). Starting at zero here avoids a
            # double grant (64 + host's 64).
            task.credit = 0
            task.asyncio_task = asyncio.create_task(self._pump_stream(task, fn, input))
        elif kind == "async":
            task.asyncio_task = asyncio.create_task(self._run_async(task, fn, input))
        elif kind == "isolated":
            task.asyncio_task = asyncio.create_task(
                self._run_isolated(task, fn_name, input)
            )
        else:  # sync inline — uncancellable per docs/cancellation.md
            task.asyncio_task = asyncio.create_task(self._run_sync(task, fn, input))

        # Control frames that beat the call here land now, not never.
        early_credit = self._pending_credit.pop(task_id, 0)
        if early_credit:
            task.credit += early_credit
            task.credit_event.set()
        if task_id in self._pending_cancels:
            self._pending_cancels.discard(task_id)
            if task.asyncio_task is not None and not task.asyncio_task.done():
                task.asyncio_task.cancel()
                # Same guarantee as cancel(): a task cancelled before its
                # first step never enters a handler, so nothing would emit
                # TASK_CANCELLED — the call's terminal would be lost.
                asyncio.create_task(self._ensure_terminal(task))

    # ---- execution modes -------------------------------------------------

    async def _run_async(self, task: Task, fn: Callable[..., Any], input: dict) -> None:
        try:
            result = await fn(**input)  # input coerced at dispatch (_call)
            self._finish(task, proto.response(task.call_id, result))
        except asyncio.CancelledError:
            self._finish(
                task,
                proto.error_response(
                    task.call_id, proto.TASK_CANCELLED, "task cancelled"
                ),
            )
        except Exception as e:  # noqa: BLE001 - user boundary
            self._finish(
                task,
                proto.error_response(
                    task.call_id, proto.INTERNAL, str(e), {"type": type(e).__name__}
                ),
            )

    async def _run_sync(self, task: Task, fn: Callable[..., Any], input: dict) -> None:
        loop = asyncio.get_running_loop()
        try:
            result = await loop.run_in_executor(None, lambda: fn(**input))
            self._finish(task, proto.response(task.call_id, result))
        except asyncio.CancelledError:
            # Inline sync can't be interrupted — the executor thread keeps
            # running and its result is discarded, but the caller still
            # observes TASK_CANCELLED now (docs/cancellation.md).
            self._finish(
                task,
                proto.error_response(
                    task.call_id, proto.TASK_CANCELLED, "task cancelled"
                ),
            )
        except Exception as e:  # noqa: BLE001 - user boundary
            self._finish(
                task,
                proto.error_response(
                    task.call_id, proto.INTERNAL, str(e), {"type": type(e).__name__}
                ),
            )

    async def _pump_stream(
        self, task: Task, fn: Callable[..., Any], input: dict
    ) -> None:
        agen: AsyncGenerator = fn(**input)
        try:
            while True:
                # Pull-based flow control: only advance while we hold credit.
                await self._wait_for_credit(task)
                value = await agen.__anext__()
                task.credit -= 1
                # A chunk deeper than the host's parser limit is a
                # malformed frame on arrival — end THIS stream with the
                # typed error instead of killing the runtime.
                if _json.exceeds_depth(value, MAX_DEPTH - 4):
                    self._finish(
                        task,
                        proto.error_response(
                            task.call_id,
                            proto.LIMIT_EXCEEDED,
                            "stream chunk exceeds the depth cap",
                            {"limit": "max_depth", "max": MAX_DEPTH},
                        ),
                    )
                    return
                # A chunk over the frame cap would be rejected by the host
                # reader and kill the runtime — end THIS stream with the
                # typed error instead (RC.8A).
                if len(_json.dumps(value).encode()) + 256 > MAX_FRAME:
                    self._finish(
                        task,
                        proto.error_response(
                            task.call_id,
                            proto.LIMIT_EXCEEDED,
                            "stream chunk exceeds the frame cap",
                            {"limit": "max_frame", "max": MAX_FRAME},
                        ),
                    )
                    return
                self._send(
                    proto.notification(
                        "stream.chunk",
                        {"task": task.id, "seq": task.seq, "value": value},
                    )
                )
                task.seq += 1
        except StopAsyncIteration:
            self._finish(task, proto.response(task.call_id, {"done": True}))
        except asyncio.CancelledError:
            await _aclose_quietly(agen)
            self._finish(
                task,
                proto.error_response(
                    task.call_id, proto.TASK_CANCELLED, "task cancelled"
                ),
            )
        except Exception as e:  # noqa: BLE001 - user boundary
            await _aclose_quietly(agen)
            self._finish(
                task,
                proto.error_response(
                    task.call_id, proto.INTERNAL, str(e), {"type": type(e).__name__}
                ),
            )

    async def _wait_for_credit(self, task: Task) -> None:
        while task.credit <= 0:
            task.credit_event.clear()
            await task.credit_event.wait()

    async def _run_isolated(self, task: Task, fn_name: str, input: dict) -> None:
        try:
            module = os.environ["PYLAIR_APP_MODULE"]
            kwargs: dict[str, Any] = {
                "stdin": asyncio.subprocess.PIPE,
                "stdout": asyncio.subprocess.PIPE,
                # The worker compares its ppid against this at startup —
                # a mismatch means the runtime died during the exec and
                # it is already orphaned (see _worker._exit_with_parent).
                "env": {**os.environ, "PYLAIR_WORKER_PARENT": str(os.getpid())},
            }
            if sys.platform == "win32":
                kwargs["creationflags"] = 0  # taskkill /T handles the tree
            else:
                kwargs["start_new_session"] = True  # own process group -> killpg
            task.worker = await asyncio.create_subprocess_exec(
                sys.executable,
                "-m",
                "pylair._worker",
                module,
                fn_name,
                **kwargs,
            )
            out, _ = await task.worker.communicate(_json.dumps(input).encode())
            if task.worker.returncode != 0:
                tail = out.decode(errors="replace")[-2000:]
                self._finish(
                    task,
                    proto.error_response(
                        task.call_id,
                        proto.WORKER_DIED,
                        f"worker exited {task.worker.returncode}",
                        {"output": tail} if tail else None,
                    ),
                )
                return
            payload = _json.loads(out)
            if "error" in payload:
                self._finish(
                    task,
                    proto.error_response(
                        task.call_id,
                        payload["error"].get("code", proto.INTERNAL),
                        payload["error"]["message"],
                        payload["error"].get("data"),
                    ),
                )
            else:
                self._finish(task, proto.response(task.call_id, payload["result"]))
        except asyncio.CancelledError:
            await self._kill_worker(task)
            self._finish(
                task,
                proto.error_response(
                    task.call_id, proto.TASK_CANCELLED, "task cancelled"
                ),
            )
        except BaseException as e:  # noqa: BLE001 - any failure gets a terminal
            self._finish(
                task,
                proto.error_response(
                    task.call_id,
                    proto.INTERNAL,
                    f"isolated call failed: {type(e).__name__}: {e}",
                    {"type": type(e).__name__},
                ),
            )

    async def _kill_worker(self, task: Task) -> None:
        proc = task.worker
        if proc is None or proc.returncode is not None:
            return
        if sys.platform == "win32":
            # taskkill /T kills the whole tree — already covers
            # grandchildren. Await it with a bound, check the exit code,
            # fall back to proc.kill() on failure or timeout, and bound
            # the wait for the worker to actually exit: the loop never
            # parks on a worker that refuses to die.
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
                except ProcessLookupError:
                    pass
            try:
                await asyncio.wait_for(
                    proc.wait(), timeout=self._budget(ISOLATED_GRACE_S)
                )
            except TimeoutError:
                _log(
                    f"worker {proc.pid} did not exit within "
                    f"{ISOLATED_GRACE_S}s of taskkill — last-resort kill"
                )
                try:
                    proc.kill()
                except ProcessLookupError:
                    pass
                try:
                    await asyncio.wait_for(proc.wait(), timeout=1.0)
                except TimeoutError:
                    _log(f"worker {proc.pid} survived kill — abandoning reap")
            return
        try:
            os.killpg(proc.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        try:
            await asyncio.wait_for(proc.wait(), timeout=self._budget(ISOLATED_GRACE_S))
        except TimeoutError:
            pass
        # No leaked grandchildren: SIGKILL the whole process group whether or
        # not the leader already exited on SIGTERM — a grandchild ignoring
        # SIGTERM must not survive.
        try:
            os.killpg(proc.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        try:
            proc.kill()
        except ProcessLookupError:
            pass
        await proc.wait()

    # ---- cancel / credit / finish -----------------------------------------

    def grant_credit(self, task_id: int | str, n: int) -> bool:
        task = self._tasks.get(task_id)
        if task is None or task.done:
            if _is_int_id(task_id) and task_id <= self._highest_task_id:
                return False  # finished or rejected — never registers
            # Credit that outran its call: hold it for registration instead
            # of losing it (control lane is independent of the data lane).
            if len(self._pending_credit) < 4096:
                self._pending_credit[task_id] = min(
                    self._pending_credit.get(task_id, 0) + n, MAX_STREAM_CREDIT
                )
            else:
                _log(
                    "pending-credit map hit its 4096 backstop — "
                    f"dropping early credit for task {task_id!r}"
                )
            return False
        # Defensive clamp (RC.8A): the host enforces the outstanding-credit
        # cap on grants; the runtime never lets it grow past the ceiling.
        task.credit = min(task.credit + n, MAX_STREAM_CREDIT)
        task.credit_event.set()
        return True

    def cancel(self, task_id: int | str, call_id: int | str | None = None) -> None:
        """Cancel a task — synchronous half, in dispatch order.

        Sends the cancel response for `call_id` (None = internal shutdown
        use, no response is owed). For isolated tasks the terminate/reap
        outcome is reported by a spawned waiter so a slow kill never
        occupies the dispatch lane.
        """
        task = self._tasks.get(task_id)
        if task is None:
            if _is_int_id(task_id) and task_id <= self._highest_task_id:
                # Below the high-water mark the call already arrived and
                # finished or was rejected — a late cancel, not early.
                result = {"cancelled": False, "reason": "task_not_found"}
            elif len(self._pending_cancels) < 4096:
                # A cancel can outrun the call that creates the task (the
                # host's control lane beats the data lane) — record it so
                # it lands at registration, never run untracked. An
                # unknown id above the mark means the call frame is still
                # in flight: the cancel is accepted pending arrival.
                self._pending_cancels.add(task_id)
                result = {"cancelled": True, "status": "pending"}
            else:
                _log(
                    "pending-cancel set hit its 4096 backstop — "
                    f"dropping early cancel for task {task_id!r}"
                )
                result = {"cancelled": False, "reason": "task_not_found"}
        elif task.done:
            result = {"cancelled": False, "reason": "task_not_found"}
        elif task.asyncio_task is not None and not task.asyncio_task.done():
            task.asyncio_task.cancel()
            asyncio.create_task(self._ensure_terminal(task))
            if task.kind == "isolated":
                if call_id is not None:
                    asyncio.create_task(self._report_isolated_cancel(task, call_id))
                return
            if task.kind == "sync":
                # Inline sync runs on an executor thread — it cannot be
                # interrupted. The caller sees TASK_CANCELLED; the Python
                # may run to completion. Say so — never claim it stopped.
                result = {"cancelled": True, "status": "may_continue"}
            else:
                # Ack immediately; the terminal response follows via the
                # task's own handler or the finisher — exactly once,
                # _finish dedupes.
                result = {"cancelled": True}
        else:
            result = {"cancelled": True}
        if call_id is not None:
            self._send(proto.response(call_id, result))

    async def _report_isolated_cancel(self, task: Task, call_id: int | str) -> None:
        """Awaiting half of an isolated cancel: report what ACTUALLY
        happened (RC.6B) — the terminate/force-kill/reap outcome within
        a bound strictly inside the host's cancel_timeout (5 s). The old
        bound (GRACE+2 s) equalled the host's — the response could lose
        the race and surface as RUNTIME_WEDGED."""
        try:
            await asyncio.wait_for(
                asyncio.shield(task.asyncio_task),
                timeout=ISOLATED_GRACE_S + 1.0,
            )
            result = {"cancelled": True, "status": "terminated"}
        except asyncio.CancelledError:
            result = {"cancelled": True, "status": "terminated"}
        # An outcome we can't classify means "may still be running".
        except BaseException:  # noqa: BLE001 - deliberate catch-all
            result = {"cancelled": True, "status": "termination_in_progress"}
        self._send(proto.response(call_id, result))

    async def _ensure_terminal(self, task: Task) -> None:
        """Guarantee a terminal response even if the handler never ran.

        A task cancelled before its first step never enters its handler's
        except block, so nothing would emit TASK_CANCELLED.
        """
        try:
            await asyncio.wait_for(asyncio.shield(task.asyncio_task), timeout=5.0)
        # CancelledError is BaseException, not Exception — catch broadly so the
        # task.done check below always runs.
        except BaseException as e:  # noqa: BLE001
            _log(f"finisher: task {task.id} ended with {e!r} before marking done")
        if not task.done:
            self._finish(
                task,
                proto.error_response(
                    task.call_id, proto.TASK_CANCELLED, "task cancelled"
                ),
            )

    async def cancel_all(self) -> None:
        for task in self._tasks.values():
            if not task.done:
                self.cancel(task.id)  # sync — all kills start together
        # cancel() only SCHEDULES the terminal-emitting finisher. On
        # shutdown the loop closes right after this returns, so wait for
        # the cancelled tasks to end — the finishers emit TASK_CANCELLED
        # while the writer thread is still alive to carry them.
        waiters = [
            t.asyncio_task
            for t in self._tasks.values()
            if t.asyncio_task is not None and not t.asyncio_task.done()
        ]
        try:
            if waiters:
                await asyncio.wait(waiters)
        finally:
            for task in self._tasks.values():
                if not task.done:
                    self._finish(
                        task,
                        proto.error_response(
                            task.call_id, proto.TASK_CANCELLED, "task cancelled"
                        ),
                    )

    def _finish(self, task: Task, terminal: dict) -> None:
        if task.done:
            return  # a cancelled task's late result is discarded
        task.done = True
        if task.kind == "isolated":
            self._isolated -= 1
        try:
            self._send(terminal)  # serializes on the event loop
        except (TypeError, ValueError) as e:
            # Result not wire-serializable (numpy/tensor object, NaN) —
            # fail this one call, never the runtime (protocol §2).
            self._send(
                proto.error_response(
                    task.call_id,
                    proto.INTERNAL,
                    f"result is not wire-serializable: {e}",
                    {"type": "SerializationError"},
                )
            )
        self._tasks.pop(task.id, None)


async def _aclose_quietly(agen: AsyncGenerator) -> None:
    try:
        await agen.aclose()
    except Exception as e:  # noqa: BLE001 - cleanup must not kill the loop
        _log(f"generator aclose raised: {e!r}")
