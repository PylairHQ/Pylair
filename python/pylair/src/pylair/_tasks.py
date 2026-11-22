"""Task registry: in-flight calls, credit-gated stream pumps, isolated workers."""

from __future__ import annotations

import asyncio
import os
import signal
import struct
import subprocess
import sys
import time
from collections import deque
from collections.abc import AsyncGenerator, Callable
from dataclasses import dataclass, field
from typing import Any

from pylair import _json, _readiness
from pylair import _protocol as proto

ISOLATED_GRACE_S = 3.0
# taskkill is awaited with this bound - a hung taskkill or a nonzero
# exit falls back to proc.kill() rather than parking the loop forever.
TASKKILL_TIMEOUT_S = 5.0
# A warm pool whose workers die this many times in a row with no
# successful call in between is declared a crash loop and stops (0008 §1).
WARM_CRASH_LIMIT = 3
# Bounded tail kept per warm member for WORKER_DIED diagnostics.
WARM_STDERR_TAIL = 8192

# RC.8A ceilings — the host enforces these on the way in; the runtime
# clamps defensively so a host bug can't grow them without bound.
MAX_STREAM_CREDIT = int(os.environ.get("PYLAIR_MAX_STREAM_CREDIT", "4096"))
MAX_ISOLATED_WORKERS = int(os.environ.get("PYLAIR_MAX_ISOLATED_WORKERS", "8"))
# One chunk frame can never legitimately exceed the frame cap; a value
# this large ends the stream with a typed error instead (RC.8A).
from pylair._transport import MAX_DEPTH, MAX_FRAME

# The private worker channel uses the same 4-byte big-endian prefix as
# the host<->runtime framing (_transport._LEN) — internal protocol.
_LEN = struct.Struct(">I")


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
    # Warm mode (0008): the call is served by a pool member, not a
    # per-call worker. `warm` is set at registration; `warm_queued` only
    # while the call waits on the pool's FIFO queue — that's how cancel
    # tells "dropped from queue" apart from "kill the serving worker".
    warm: bool = False
    warm_queued: bool = False
    # Interrupt-first cancel (0008 §2): pending = the interrupt was sent
    # and the response is awaited; survived = the member answered inside
    # the grace window (the reporter maps it to the plain cancelled ack).
    # `warm_started` is set when the worker's "started" frame lands —
    # the raise window is open and an interrupt is worth sending.
    warm_cancel_pending: bool = False
    warm_survived: bool = False
    warm_grace_s: float = 3.0
    warm_started: asyncio.Event = field(default_factory=asyncio.Event)


class _WarmMember:
    """One live worker process in a warm pool (decision 0008)."""

    __slots__ = ("busy", "killed", "proc", "stderr_tail", "stderr_task")

    def __init__(self, proc: asyncio.subprocess.Process) -> None:
        self.proc = proc
        self.busy = False
        # Killed by us (cancel/drain), not a crash — a deliberate kill
        # must not count toward the pool's crash-loop limit.
        self.killed = False
        # Bounded tail of the member's stderr — a crash's last lines
        # ride the next WORKER_DIED (the #140 review fix).
        self.stderr_tail = b""
        self.stderr_task: asyncio.Task | None = None


class _WarmPool:
    """Per-function pool of warm workers: members, FIFO queue, crash count."""

    __slots__ = (
        "deaths",
        "fn_name",
        "members",
        "prewarm",
        "queue",
        "size",
        "spawning",
        "stopped",
    )

    def __init__(self, fn_name: str, size: int, prewarm: bool = False) -> None:
        self.fn_name = fn_name
        self.size = size
        self.members: list[_WarmMember] = []
        self.spawning = 0  # in-flight spawns — count toward size and cap
        # Queued calls wait here in call order; an entry is (task, future).
        self.queue: deque[tuple[Task, asyncio.Future]] = deque()
        self.deaths = 0  # consecutive member deaths with no success
        self.stopped = False  # crash-loop limit hit — calls fail fast
        # Prewarmed pools own the `worker:<fn>` readiness component —
        # every fresh member handshakes so ready/failed always means a
        # live member passed import+resolve (0008 §6).
        self.prewarm = prewarm


class _PoolStopped(Exception):
    """A queued call's acquire reached a crash-loop-stopped pool."""

    def __init__(self, pool: _WarmPool) -> None:
        super().__init__(
            f"warm pool for {pool.fn_name!r} stopped after "
            f"{pool.deaths} consecutive worker deaths"
        )


class _WarmCap(Exception):
    """The live-worker cap blocks a warm spawn at pool-acquire time."""


class _WarmWorkerDead(Exception):
    """The serving member died or broke its channel mid-call."""


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
        # Live worker PROCESS count, split by mode (0008 §3):
        # `self._isolated` counts in-flight process-mode calls (1:1 with
        # live workers); `self._warm_live` counts live warm pool members
        # (spawn to reap, including idle ones). The cap applies to the sum.
        self._isolated = 0
        self._warm_live = 0
        self._pools: dict[str, _WarmPool] = {}
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
        requires: tuple[str, ...] = (),
        worker: dict | None = None,
    ) -> None:
        warm = worker is not None and worker.get("mode") == "warm"
        # Worker processes are the heaviest resource — refuse past the
        # cap with the typed error, don't queue (RC.8A). The cap bounds
        # LIVE processes (0008 §3): in-flight process-mode calls plus
        # every warm member alive, idle ones included. Warm calls skip
        # this check — they queue and spawn-check at pool-acquire.
        if (
            kind == "isolated"
            and not warm
            and self._isolated + self._warm_live >= MAX_ISOLATED_WORKERS
        ):
            self._send(
                proto.error_response(
                    call_id,
                    proto.LIMIT_EXCEEDED,
                    f"isolated worker cap {MAX_ISOLATED_WORKERS} reached",
                    {
                        "limit": "max_isolated_workers",
                        "max": MAX_ISOLATED_WORKERS,
                    },
                )
            )
            return
        task = Task(id=task_id, fn_name=fn_name, kind=kind, call_id=call_id)
        task.warm = warm
        if warm:
            # Interrupt-first cancel's response window (0008 §2).
            task.warm_grace_s = (worker or {}).get("cancel_grace_ms", 3000) / 1000.0
        self._tasks[task_id] = task
        if kind == "isolated" and not warm:
            self._isolated += 1

        if kind == "generator":
            # Credit is owned by the HOST: it grants the initial window right
            # after `call` (protocol §3.1). Starting at zero here avoids a
            # double grant (64 + host's 64).
            task.credit = 0

        # A `requires=` wait lives inside the task coroutine (0007 §2):
        # the slot and window share are already charged here, cancel
        # lands through the normal path, and for isolated calls the
        # gate runs before the worker spawn inside _run_isolated. The
        # runner is built lazily so a gated-out or pre-step-cancelled
        # call never allocates a coroutine it must close.
        if requires:
            coro = self._run_gated(
                task,
                requires,
                lambda: self._runner(task, kind, fn_name, fn, input, worker),
            )
        else:
            coro = self._runner(task, kind, fn_name, fn, input, worker)
        task.asyncio_task = asyncio.create_task(coro)

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

    def _runner(
        self,
        task: Task,
        kind: str,
        fn_name: str,
        fn: Callable[..., Any],
        input: dict,
        worker: dict | None = None,
    ):
        """The kind's handler coroutine for this task."""
        if kind == "generator":
            return self._pump_stream(task, fn, input)
        if kind == "async":
            return self._run_async(task, fn, input)
        if kind == "isolated":
            if task.warm:
                return self._run_warm(task, fn_name, input, worker or {})
            return self._run_isolated(task, fn_name, input)
        return self._run_sync(task, fn, input)  # uncancellable inline sync

    async def _run_gated(
        self,
        task: Task,
        requires: tuple[str, ...],
        make_body: Callable[[], Any],
    ) -> None:
        """Wait for required components, then run the kind's handler.

        A component that is already `ready` returns immediately; a
        `failed` component ends the call as INTERNAL with
        data {"component", "phase": "failed"} (0007 §2 — the data
        channel is the extension point, no new error code). Cancellation
        is the ordinary path: the wait raises CancelledError like any
        other task suspension.
        """
        try:
            await _readiness.wait_ready(requires)
        except asyncio.CancelledError:
            self._finish(
                task,
                proto.error_response(
                    task.call_id, proto.TASK_CANCELLED, "task cancelled"
                ),
            )
            return
        except _readiness.GateError as e:
            self._finish(
                task,
                proto.error_response(task.call_id, proto.INTERNAL, str(e), e.data),
            )
            return
        await make_body()

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
                # typed error instead of killing the runtime. The check
                # rides inside dumps' single pre-walk (MAX_DEPTH - 4
                # leaves room for the notification envelope).
                try:
                    _body = _json.dumps(value, MAX_DEPTH - 4).encode()
                except _json.DepthExceeded:
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
                if len(_body) + 256 > MAX_FRAME:
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

    # ---- warm pools (decision 0008) ----------------------------------------
    #
    # A warm call is a normal isolated task until it reaches a pool member:
    # it registers, queues FIFO, is cancellable and fairness-counted
    # throughout. One call at a time per member — the response maps to the
    # serving task unambiguously, so the private channel needs no call ids.

    async def _run_warm(
        self, task: Task, fn_name: str, input: dict, worker: dict
    ) -> None:
        pool = self._pools.get(fn_name)
        if pool is None:
            pool = _WarmPool(
                fn_name, worker.get("pool", 1), prewarm=worker.get("prewarm", False)
            )
            self._pools[fn_name] = pool
        member: _WarmMember | None = None
        try:
            member = await self._acquire_warm(task, pool)
            # task.worker is set before the first await so cancel's kill
            # path reaches the serving member exactly like process mode.
            task.worker = member.proc
            body = _json.dumps({"op": "call", "input": input}).encode()
            member.proc.stdin.write(_LEN.pack(len(body)) + body)
            await member.proc.stdin.drain()
            # Frames until the terminal: {"started": true} marks the
            # raise window open on the worker — a pending interrupt is
            # now worth sending (0008 §2). Anything else that is not a
            # terminal payload is a broken channel.
            payload: dict | None = None
            while True:
                payload = await self._warm_response(member.proc)
                if payload is None:
                    raise _WarmWorkerDead
                if payload.get("started") is True:
                    task.warm_started.set()
                    continue
                if "ready" in payload:
                    # A handshake abandoned mid-cancel leaves its answer
                    # buffered — it is still the member's proof: report
                    # it and consume; the call's frames follow.
                    if pool.prewarm:
                        self._report_warm_ready(pool, payload)
                    continue
                break
            if not any(k in payload for k in ("result", "error", "interrupted")):
                raise _WarmWorkerDead
            # A framed response — any shape — proves the member healthy.
            pool.deaths = 0
            if task.warm_cancel_pending:
                # A response for the cancelled call = survived (0008 §2):
                # the interrupt either fired inside the raise window or
                # the call raced to completion — the member is alive and
                # returns to the pool either way.
                task.warm_survived = True
                self._finish(
                    task,
                    proto.error_response(
                        task.call_id, proto.TASK_CANCELLED, "task cancelled"
                    ),
                )
                return
            if "interrupted" in payload:
                # A KeyboardInterrupt with no cancel in flight — the call
                # died but the worker is fine and returns to the pool.
                self._finish(
                    task,
                    proto.error_response(
                        task.call_id, proto.INTERNAL, "worker call interrupted"
                    ),
                )
                return
        except asyncio.CancelledError:
            if member is not None:
                # Our cancel kill, not a crash — it must not count toward
                # the crash-loop limit. The kill path is the same one
                # process mode uses (SIGTERM->SIGKILL group / taskkill).
                member.killed = True
                await self._kill_worker(task)
                await self._drop_warm_member(pool, member)
            self._finish(
                task,
                proto.error_response(
                    task.call_id, proto.TASK_CANCELLED, "task cancelled"
                ),
            )
            return
        except _PoolStopped as e:
            self._finish(
                task,
                proto.error_response(
                    task.call_id,
                    proto.INTERNAL,
                    str(e),
                    {"type": "WorkerCrashLoop", "fn": fn_name},
                ),
            )
            return
        except _WarmCap:
            self._finish(
                task,
                proto.error_response(
                    task.call_id,
                    proto.LIMIT_EXCEEDED,
                    f"isolated worker cap {MAX_ISOLATED_WORKERS} reached",
                    {"limit": "max_isolated_workers", "max": MAX_ISOLATED_WORKERS},
                ),
            )
            return
        except BaseException as e:  # noqa: BLE001 - any failure gets a terminal
            # Channel broke, spawn failed, or the worker died mid-call —
            # drop the suspect member (counted) and respawn lazily.
            if member is not None:
                await self._drop_warm_member(pool, member)
            data: dict[str, Any] = {"type": type(e).__name__}
            if member is not None and member.stderr_tail:
                # The member's last stderr lines — a crash on import or
                # mid-call becomes visible to the caller (#140 review).
                data["stderr"] = member.stderr_tail.decode(errors="replace")[-2000:]
            self._finish(
                task,
                proto.error_response(
                    task.call_id,
                    proto.WORKER_DIED,
                    f"warm worker for {fn_name!r} died: {type(e).__name__}: {e}",
                    data,
                ),
            )
            return
        finally:
            if member is not None:
                task.worker = None
                if member in pool.members:
                    member.busy = False
                self._wake_next(pool)
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

    async def _acquire_warm(self, task: Task, pool: _WarmPool) -> _WarmMember:
        """A live member for this call: reuse idle, spawn under size+cap,
        else wait on the pool's FIFO queue (lazy spawn, 0008 §3).

        An entry stays at the head while its owner scans — a fresh caller
        always sees a nonempty queue and joins the tail, so a wake is
        never stolen mid-scan. A head that cannot claim rejoins the tail
        with a fresh future; every wake gives exactly one waiter a shot.
        """
        loop = asyncio.get_running_loop()
        entry: tuple[Task, asyncio.Future] | None = None
        try:
            while True:
                if pool.stopped:
                    raise _PoolStopped(pool)
                # Members that died between calls are discovered here.
                for m in list(pool.members):
                    if m.proc.returncode is not None:
                        await self._drop_warm_member(pool, m)
                # The dead-scan can stop the pool mid-iteration.
                if pool.stopped:
                    raise _PoolStopped(pool)
                if entry is None and pool.queue:
                    # Waiters ahead — join the tail before claiming.
                    entry = (task, loop.create_future())
                    pool.queue.append(entry)
                    task.warm_queued = True
                    continue
                if entry is None or (pool.queue and pool.queue[0] is entry):
                    for m in pool.members:
                        if not m.busy:
                            m.busy = True
                            return m
                    if len(pool.members) + pool.spawning < pool.size:
                        if self._isolated + self._warm_live >= MAX_ISOLATED_WORKERS:
                            raise _WarmCap
                        # Reserve size and cap slots BEFORE the spawn
                        # await — a second call arriving mid-spawn must
                        # not overshoot either bound by one exec's width.
                        pool.spawning += 1
                        self._warm_live += 1
                        try:
                            proc = await self._spawn_warm(pool)
                        except BaseException:
                            self._warm_live -= 1
                            raise
                        finally:
                            pool.spawning -= 1
                        m = _WarmMember(proc)
                        m.stderr_task = asyncio.create_task(
                            self._drain_member_stderr(m)
                        )
                        pool.members.append(m)
                        if pool.prewarm:
                            # A fresh member on a prewarmed pool proves
                            # itself before serving — the same handshake
                            # prewarm uses (0008 §6): a later successful
                            # respawn reports the component ready again.
                            try:
                                payload = await self._warm_handshake(m.proc)
                            except asyncio.CancelledError:
                                raise
                            except Exception:  # noqa: BLE001 - any channel error is a dead member
                                payload = None
                            if payload is None:
                                # Died before answering — count + drop,
                                # then rescan for a fresh member.
                                await self._drop_warm_member(pool, m)
                                continue
                            self._report_warm_ready(pool, payload)
                        m.busy = True
                        return m
                # No claim — wait for a wake.
                if entry is None:
                    entry = (task, loop.create_future())
                    pool.queue.append(entry)
                    task.warm_queued = True
                elif (
                    entry[1].done()
                    and not entry[1].cancelled()
                    and entry[1].exception() is None
                ):
                    # Spent wake, no claim — rejoin the tail fresh. A
                    # flushed (_PoolStopped) or cancelled fut is left in
                    # place: the bare await below re-raises it.
                    pool.queue.remove(entry)
                    entry = (task, loop.create_future())
                    pool.queue.append(entry)
                await entry[1]
        finally:
            if entry is not None:
                task.warm_queued = False
                if entry in pool.queue:
                    pool.queue.remove(entry)
                    # Our queue slot is vacated — the next waiter scans now.
                    self._wake_next(pool)

    async def _spawn_warm(self, pool: _WarmPool) -> asyncio.subprocess.Process:
        """One `--serve` worker for the pool — same containment flags and
        parent-death env as the one-shot path (0008 §1)."""
        try:
            module = os.environ["PYLAIR_APP_MODULE"]
            kwargs: dict[str, Any] = {
                "stdin": asyncio.subprocess.PIPE,
                "stdout": asyncio.subprocess.PIPE,
                # stderr is piped into a bounded per-member tail so a
                # crash's last lines ride WORKER_DIED — the drain task
                # starts the moment the member object exists.
                "stderr": asyncio.subprocess.PIPE,
                "env": {**os.environ, "PYLAIR_WORKER_PARENT": str(os.getpid())},
            }
            if sys.platform == "win32":
                # CREATE_NEW_PROCESS_GROUP costs nothing and keeps the
                # worker's group addressable + shields it from console
                # CTRL broadcasts (0008 §2 — Windows itself is kill-only,
                # but a console-attached embedding would deliver
                # CTRL_BREAK to exactly this group). taskkill /T and Job
                # containment cover the tree for the kill path.
                kwargs["creationflags"] = subprocess.CREATE_NEW_PROCESS_GROUP
            else:
                kwargs["start_new_session"] = True  # own process group -> killpg
            return await asyncio.create_subprocess_exec(
                sys.executable,
                "-m",
                "pylair._worker",
                "--serve",
                module,
                pool.fn_name,
                **kwargs,
            )
        except asyncio.CancelledError:
            raise  # our cancel, not a crash — burns no crash-loop token
        except BaseException:
            # A worker that never lived still burns a crash-loop token —
            # an always-failing spawn must not retry forever.
            pool.deaths += 1
            if pool.deaths >= WARM_CRASH_LIMIT:
                self._stop_pool(pool)
            raise

    async def _warm_response(self, proc) -> dict | None:
        """One framed reply from the serving member; None = dead channel."""
        try:
            hdr = await proc.stdout.readexactly(_LEN.size)
            (n,) = _LEN.unpack(hdr)
            if n > MAX_FRAME:
                return None
            body = await proc.stdout.readexactly(n)
            payload = _json.loads(body)
        except asyncio.IncompleteReadError as e:
            _log(
                f"warm member {proc.pid} channel closed mid-frame "
                f"(rc={proc.returncode}, {len(e.partial)} bytes held)"
            )
            return None
        except (OSError, ValueError):
            return None
        return payload if isinstance(payload, dict) else None

    async def _drain_member_stderr(self, m: _WarmMember) -> None:
        """Keep a bounded tail of a member's stderr for WORKER_DIED
        diagnostics, forwarding chunks to our own stderr so worker
        logs stay visible exactly like an inherited fd would."""
        try:
            while True:
                chunk = await m.proc.stderr.read(4096)
                if not chunk:
                    return
                m.stderr_tail = (m.stderr_tail + chunk)[-WARM_STDERR_TAIL:]
                try:
                    sys.stderr.write(chunk.decode(errors="replace"))
                    sys.stderr.flush()
                except (ValueError, OSError):
                    pass
        except (OSError, ValueError):
            return

    async def _drop_warm_member(self, pool: _WarmPool, m: _WarmMember) -> None:
        """Remove a member from the pool: count a crash death unless we
        killed it, reap the process, wake the queue to respawn lazily."""
        if m in pool.members:
            pool.members.remove(m)
            self._warm_live -= 1
            if not m.killed:
                pool.deaths += 1
                if pool.deaths >= WARM_CRASH_LIMIT:
                    self._stop_pool(pool)
        if m.proc.returncode is None:
            m.killed = True
            await self._kill_proc(m.proc)
        else:
            await m.proc.wait()
        # Let the drain finish reading what is already in the pipe so the
        # tail is complete for WORKER_DIED — bounded, because a grandchild
        # can hold the pipe open past the member's death.
        if m.stderr_task is not None:
            done, _pending = await asyncio.wait({m.stderr_task}, timeout=1.0)
            if not done:
                m.stderr_task.cancel()
            elif not m.stderr_task.cancelled():
                m.stderr_task.exception()  # retrieve — no "never retrieved" noise
            m.stderr_task = None
        self._wake_next(pool)

    def _wake_next(self, pool: _WarmPool) -> None:
        """Wake the first live waiter so it can re-scan the pool. Its
        entry stays at the head until it claims or rejoins the tail —
        a fresh caller arriving mid-wake never jumps the line. Dead
        entries are skipped; their owners remove them while unwinding."""
        for _task, fut in pool.queue:
            if not fut.done():
                fut.set_result(None)
                return

    def _flush_warm_queue(self, pool: _WarmPool) -> None:
        """Pool stopped: every queued call fails with the crash-loop error."""
        while pool.queue:
            _task, fut = pool.queue.popleft()
            if not fut.done():
                fut.set_exception(_PoolStopped(pool))

    async def _reap_warm_pools(self) -> None:
        """Drain phase: kill every live pool member (0008 §1). Idle
        members are not tasks, so cancel_all's per-task kills cannot
        reach them — this is the shutdown sweep for warm pools."""
        for pool in self._pools.values():
            for m in list(pool.members):
                m.killed = True
                await self._drop_warm_member(pool, m)

    def _stop_pool(self, pool: _WarmPool) -> None:
        """Crash-loop limit: fail-fast the pool (0008 §1). A prewarmed
        pool also fails its `worker:<fn>` component with the reason —
        it stays failed for as long as the pool is stopped (§6)."""
        pool.stopped = True
        self._flush_warm_queue(pool)
        if pool.prewarm:
            _readiness.failed(f"worker:{pool.fn_name}", detail=str(_PoolStopped(pool)))

    async def _warm_handshake(self, proc) -> dict | None:
        """The prewarm handshake (0008 §6): {"op": "ready"} is answered
        only after the member's import+resolve block ran, so the reply
        proves module+fn. Returns the payload; None on a dead channel."""
        body = _json.dumps({"op": "ready"}).encode()
        proc.stdin.write(_LEN.pack(len(body)) + body)
        await proc.stdin.drain()
        return await self._warm_response(proc)

    def _report_warm_ready(self, pool: _WarmPool, payload: dict) -> None:
        """Map one handshake answer onto the pool's component:
        {"ready": true} reports ready, anything else reports failed
        with the worker's error detail."""
        comp = f"worker:{pool.fn_name}"
        if payload.get("ready") is True:
            _readiness.ready(comp)
        else:
            err = payload.get("error") or {}
            _readiness.failed(comp, detail=err.get("message") or "member is not ready")

    async def _prewarm(self, fn_name: str, worker: dict) -> None:
        """Spawn the pool's first member at startup and drive its ready
        handshake (0008 §6): ready on a completed handshake, failed
        with detail on a spawn/import/death outcome. Reports onto the
        managed `worker:<fn>` component and never raises — a prewarm
        surprise is app-visible state, not a runtime failure."""
        comp = f"worker:{fn_name}"
        pool = self._pools.get(fn_name)
        if pool is None:
            pool = _WarmPool(fn_name, worker.get("pool", 1), prewarm=True)
            self._pools[fn_name] = pool
        else:
            pool.prewarm = True
        if pool.stopped:
            # Lazy calls already crashed the pool out — keep the
            # crash-loop reason, never spawn into a stopped pool.
            _readiness.failed(comp, detail=str(_PoolStopped(pool)))
            return
        if pool.members or pool.spawning:
            # A racing call already spawned (or is spawning) a member —
            # the lazy path's own post-spawn handshake reports the
            # component; a second spawn would overshoot pool.size.
            return
        if self._isolated + self._warm_live >= MAX_ISOLATED_WORKERS:
            _readiness.failed(
                comp, detail=f"isolated worker cap {MAX_ISOLATED_WORKERS} reached"
            )
            return
        # Reserve size and cap slots across the spawn await —
        # a call arriving mid-prewarm must not overshoot either.
        pool.spawning += 1
        self._warm_live += 1
        try:
            try:
                proc = await self._spawn_warm(pool)
            finally:
                pool.spawning -= 1
        except asyncio.CancelledError:
            self._warm_live -= 1
            raise  # shutting down — the reap owns the member
        except BaseException as e:  # noqa: BLE001 - spawn failure is app state
            self._warm_live -= 1
            _readiness.failed(comp, detail=f"{type(e).__name__}: {e}")
            return
        m = _WarmMember(proc)
        m.stderr_task = asyncio.create_task(self._drain_member_stderr(m))
        # In the pool before the handshake — a mid-handshake cancel or
        # drain still reaps the member like any other live member.
        # (_warm_live was already reserved across the spawn.)
        pool.members.append(m)
        try:
            payload = await self._warm_handshake(proc)
        except asyncio.CancelledError:
            raise
        except BaseException as e:  # noqa: BLE001 - report, never raise
            _readiness.failed(comp, detail=f"handshake failed: {type(e).__name__}: {e}")
            return
        if payload is None:
            await self._drop_warm_member(pool, m)
            _readiness.failed(
                comp,
                detail=(
                    f"member died during handshake (rc={proc.returncode})"
                    + (
                        ": " + m.stderr_tail.decode(errors="replace")[-500:]
                        if m.stderr_tail
                        else ""
                    )
                ),
            )
        else:
            self._report_warm_ready(pool, payload)

    def _interrupt_warm(self, task: Task) -> None:
        """Interrupt the serving member's call, worker-side (0008 §2):
        the signal raises KeyboardInterrupt only while the user function
        executes. POSIX-only — killpg(SIGINT) targets the worker's own
        session group. Windows is deliberately not attempted: a
        CTRL_BREAK_EVENT needs a shared console and embedded workers
        have none, so the documented fallback is the kill path. A member
        that already died needs no interrupt — its response EOF reads as
        dead anyway."""
        proc = task.worker
        if proc is None or proc.returncode is not None or sys.platform == "win32":
            return
        try:
            os.killpg(proc.pid, signal.SIGINT)
        except (ProcessLookupError, OSError) as e:
            _log(f"warm interrupt for task {task.id} failed: {e!r}")

    async def _kill_worker(self, task: Task) -> None:
        proc = task.worker
        if proc is None or proc.returncode is not None:
            return
        await self._kill_proc(proc)

    async def _kill_proc(self, proc: asyncio.subprocess.Process) -> None:
        if proc.returncode is not None:
            return
        if sys.platform == "win32":
            # taskkill /T kills the whole tree - already covers
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
                    f"{ISOLATED_GRACE_S}s of taskkill - last-resort kill"
                )
                try:
                    proc.kill()
                except ProcessLookupError:
                    pass
                try:
                    await asyncio.wait_for(proc.wait(), timeout=1.0)
                except TimeoutError:
                    _log(f"worker {proc.pid} survived kill - abandoning reap")
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
            if (
                task.kind == "isolated"
                and task.warm
                and not task.warm_queued
                # Interrupt-first is POSIX-only (0008 §2 fallback):
                # CTRL_BREAK reaches a console-attached target group,
                # but the embedded runtime spawns workers console-less —
                # GenerateConsoleCtrlEvent reports success and delivers
                # nothing (probed). Waiting the grace for a signal that
                # cannot land only delays the kill, so Windows cancels
                # go straight to the kill path below.
                and sys.platform != "win32"
            ):
                # Running warm call — interrupt first (0008 §2). The
                # task coroutine is NOT cancelled — it must keep waiting
                # on the response frame so a survived member can answer.
                # The reporter waits for the worker's "started" frame
                # inside the grace (the signal is only useful once the
                # raise window is open), interrupts, then classifies —
                # falling back to the kill path on silence. It also owns
                # the worker-None window: a cancel that lands mid-spawn
                # or mid-import gets the same started-wait instead of an
                # interrupt that would arrive before the window exists.
                task.warm_cancel_pending = True
                asyncio.create_task(self._ensure_terminal(task))
                if call_id is not None:
                    asyncio.create_task(self._report_warm_cancel(task, call_id))
                return
            task.asyncio_task.cancel()
            asyncio.create_task(self._ensure_terminal(task))
            if task.kind == "isolated":
                if task.warm_queued:
                    # Queued on a warm pool, never reached a worker —
                    # the drop IS the confirmed outcome; nothing ran and
                    # nothing needed terminating (0008 §3).
                    result = {"cancelled": True}
                else:
                    if call_id is not None:
                        asyncio.create_task(self._report_isolated_cancel(task, call_id))
                    return
            elif task.kind == "sync":
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

    async def _report_warm_cancel(self, task: Task, call_id: int | str) -> None:
        """Interrupt-first verdict (0008 §2). The signal is only useful
        inside the worker's raise window, so first wait for its
        "started" frame — inside the same cancel_grace_ms budget — then
        interrupt and wait for the response. A response inside the
        grace means survived: plain `cancelled`, the work confirmed
        stopped and the member returns to the pool. Silence means
        stuck: the existing kill path runs and reports `terminated`
        exactly like process mode. Started-wait, interrupt wait and the
        kill share one budget (ISOLATED_GRACE_S + 1.0) so the ack stays
        inside the host's 5 s cancel_timeout either way."""
        deadline = time.monotonic() + ISOLATED_GRACE_S + 1.0

        def budget() -> float:
            return max(0.0, min(task.warm_grace_s, deadline - time.monotonic()))

        try:
            if not task.warm_started.is_set() and not task.asyncio_task.done():
                # Wait for "started" OR the task itself ending (a fast
                # call's response, or a dead member's EOF) — whichever
                # first, bounded by the grace.
                started_wait = asyncio.ensure_future(task.warm_started.wait())
                try:
                    done, _pending = await asyncio.wait(
                        {started_wait, task.asyncio_task},
                        timeout=budget(),
                        return_when=asyncio.FIRST_COMPLETED,
                    )
                finally:
                    started_wait.cancel()
                if not done:
                    raise TimeoutError  # never started inside the grace
            if not task.asyncio_task.done():
                # Started -> the raise window is open, or the call just
                # finished and its frame is in flight — either way the
                # interrupt is safe to send.
                self._interrupt_warm(task)
            await asyncio.wait_for(asyncio.shield(task.asyncio_task), timeout=budget())
            result = (
                {"cancelled": True}
                if task.warm_survived
                else {"cancelled": True, "status": "terminated"}
            )
            self._send(proto.response(call_id, result))
            return
        except asyncio.CancelledError:
            self._send(
                proto.response(call_id, {"cancelled": True, "status": "terminated"})
            )
            return
        except BaseException:  # noqa: BLE001 - silence past the grace = stuck
            _log(
                f"warm task {task.id} unanswered past its interrupt grace — "
                "killing the member"
            )
        # Stuck: cancelling the task runs the same kill path as process
        # mode inside its CancelledError handler; what is left of the
        # shared budget bounds the reap wait.
        task.asyncio_task.cancel()
        try:
            await asyncio.wait_for(
                asyncio.shield(task.asyncio_task),
                timeout=max(0.0, deadline - time.monotonic()),
            )
            result = (
                {"cancelled": True}
                if task.warm_survived
                else {"cancelled": True, "status": "terminated"}
            )
        except asyncio.CancelledError:
            result = {"cancelled": True, "status": "terminated"}
        except BaseException:  # noqa: BLE001 - unclassifiable outcome
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
        if task.kind == "isolated" and not task.warm:
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
