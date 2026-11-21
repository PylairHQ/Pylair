"""App-defined readiness (decision 0007).

The app reports per-component phases — loading / ready / failed — that
the host surfaces as health().app and the pylair://app event. Reports
are coalesced latest-wins and emitted by a daemon flusher thread at
PYLAIR_READINESS_FLUSH_MS (default 250 ms) — one app.component frame per
component per interval — with an immediate flush on every phase
transition. A chatty progress loop therefore costs at most one frame per
interval and can never reach the _out tripwire (_server.py).

The registry is module-level on purpose: reports made before the server
exists (import-time calls, `requires=` declarations at decoration) are
kept, and the first post-hello flush carries every accumulated state —
exactly one frame per component.
"""

from __future__ import annotations

import asyncio
import contextvars
import inspect
import math
import os
import sys
import threading
from collections.abc import Callable, Iterable
from typing import Any

from pylair import _protocol as proto

FLUSH_S = float(os.environ.get("PYLAIR_READINESS_FLUSH_MS", "250")) / 1000.0

# Per-hook attribution (0007 §1): while a startup hook runs, every
# report records its component name in this set. A hook that raises
# fails its touched-but-still-loading components; a hook that touched
# none fails a synthesized "startup:<fn.__name__>" component. Sync hooks
# run under contextvars.copy_context() — executor threads do not
# propagate context, so without the copy their reports would land
# unattributed.
_touched: contextvars.ContextVar[set[str] | None] = contextvars.ContextVar(
    "pylair_readiness_touched", default=None
)

_hooks: list[Callable[..., Any]] = []


def _log(msg: str) -> None:
    print(f"[pylair] {msg}", file=sys.stderr, flush=True)


def _check_name(name: object) -> None:
    if not isinstance(name, str) or not name:
        raise ValueError(f"component name must be a non-empty string, got {name!r}")


class GateError(Exception):
    """A `requires=` wait could not proceed: the component is `failed`
    or was never declared. The task gate maps it to INTERNAL; `data`
    carries {"component", "phase"} for the failed case (0007 §2)."""

    def __init__(self, message: str, data: dict | None = None) -> None:
        super().__init__(message)
        self.data: dict = data or {}


class _Component:
    __slots__ = ("detail", "phase", "progress", "reported")

    def __init__(self) -> None:
        self.phase = "loading"
        self.progress: float | None = None
        self.detail: str | None = None
        # True once a real report() landed — distinguishes a
        # requires=-declared component from a reported one (the sweep
        # details and the first-flush one-shot both depend on it).
        self.reported = False


class Readiness:
    """The component-state registry + coalescing flusher.

    All mutation goes through declare()/report()/sweep() under one lock;
    the flusher thread drains the dirty set at the configured interval,
    or immediately when a phase transition sets _wake.
    """

    def __init__(self, flush_s: float = FLUSH_S) -> None:
        self._flush_s = flush_s
        self._lock = threading.Lock()
        self._components: dict[str, _Component] = {}
        self._dirty: set[str] = set()
        self._wake = threading.Event()
        self._stopping = False
        self._send: Callable[[dict], None] | None = None
        self._flusher: threading.Thread | None = None
        # Futures created by wait_ready() waiters, on their own loops.
        self._waiters: list[asyncio.Future[None]] = []

    # ---- declarations and reports ------------------------------------

    def declare(self, name: str) -> None:
        """Enter a `requires=` component in phase `loading`.

        Called at decoration time (0007 §2) so a gated call can never
        see "unknown component". Not a transition: the declaration is
        sent exactly once by the first post-hello flush; if the server
        never attaches, it stays in-memory only.
        """
        _check_name(name)
        with self._lock:
            if name not in self._components:
                self._components[name] = _Component()
                self._dirty.add(name)

    def report(
        self,
        name: str,
        phase: str,
        progress: float | None = None,
        detail: str | None = None,
    ) -> None:
        """Record the latest state for a component (latest-wins).

        Callable from the loop or any thread. Never raises once the name
        is valid — a report the app cannot deliver is still recorded.
        A phase transition wakes the flusher immediately; a same-phase
        update (progress) waits for the next interval.
        """
        _check_name(name)
        if progress is not None and (
            not isinstance(progress, (int, float))
            or isinstance(progress, bool)
            or not math.isfinite(progress)
            or not 0 <= progress <= 1
        ):
            progress = None
        if detail is not None and not isinstance(detail, str):
            detail = str(detail)
        seen = _touched.get()
        if seen is not None:
            seen.add(name)
        with self._lock:
            c = self._components.get(name)
            prev = c.phase if c is not None else None
            if c is None:
                c = _Component()
                self._components[name] = c
            c.phase = phase
            c.progress = progress
            c.detail = detail
            c.reported = True
            self._dirty.add(name)
            transition = prev != phase
        if transition:
            self._wake.set()
            self._notify_waiters()

    # ---- attach + flusher ----------------------------------------------

    def attach(self, send: Callable[[dict], None]) -> None:
        """Bind the outbound frame sink and start the flusher.

        Called once in Server.run() right after hello: the first flush
        carries the whole pre-server buffer — one frame per component,
        latest-wins (0007 §4).
        """
        with self._lock:
            if self._flusher is not None:
                return
            self._send = send
            self._flusher = threading.Thread(
                target=self._flush_loop, name="tp-readiness", daemon=True
            )
            self._flusher.start()

    def _flush_loop(self) -> None:
        self._flush_once()  # the post-hello flush
        while not self._stopping:
            self._wake.wait(self._flush_s)
            self._wake.clear()
            if self._stopping:
                return
            self._flush_once()

    def _flush_once(self) -> None:
        with self._lock:
            names, self._dirty = self._dirty, set()
            send = self._send
            frames = [
                proto.notification(
                    "app.component", self._params(n, self._components[n])
                )
                for n in names
            ]
        if send is None:
            return
        for f in frames:
            try:
                send(f)
            except Exception as e:  # noqa: BLE001 - flusher must never die
                _log(f"readiness flush dropped a frame: {e!r}")

    @staticmethod
    def _params(name: str, c: _Component) -> dict:
        params: dict[str, Any] = {"name": name, "phase": c.phase}
        if c.progress is not None:
            params["progress"] = c.progress
        if c.detail is not None:
            params["detail"] = c.detail
        return params

    # ---- hook machinery -------------------------------------------------

    def sweep(self) -> None:
        """End-of-startup sweep (0007 §1): every component still
        `loading` becomes `failed`, so no gated wait can hang past the
        end of startup. A declaration that was never reported gets
        'never reported by a startup hook'; a reported-but-unfinished
        component gets the incomplete-hook detail."""
        with self._lock:
            changed = False
            for name, c in self._components.items():
                if c.phase != "loading":
                    continue
                c.phase = "failed"
                c.progress = None
                c.detail = (
                    "startup hook finished without reporting ready"
                    if c.reported
                    else "never reported by a startup hook"
                )
                self._dirty.add(name)
                changed = True
        if changed:
            self._wake.set()
            self._notify_waiters()

    def attribute_failure(
        self, fn: Callable[..., Any], names_touched: set[str], exc: BaseException
    ) -> None:
        """A startup hook raised: fail its touched-but-loading
        components with the exception as detail; a hook that touched
        none fails "startup:<fn.__name__>" (0007 §1)."""
        detail = f"{type(exc).__name__}: {exc}"
        hook = getattr(fn, "__name__", "hook")
        _log(f"startup hook {hook} failed: {detail}")
        with self._lock:
            victims = [
                n
                for n in names_touched
                if self._components.get(n) is not None
                and self._components[n].phase == "loading"
            ]
        if victims:
            for n in victims:
                self.report(n, "failed", detail=detail)
        elif not names_touched:
            self.report(f"startup:{hook}", "failed", detail=detail)

    # ---- requires= gate --------------------------------------------------

    async def wait_ready(self, names: Iterable[str]) -> None:
        """Wait until every named component is `ready` (0007 §2).

        Returns at once when all are ready; raises GateError when a
        component is `failed` (or has no declaration at all). The wait
        is cancellable through the ordinary task-cancel path — the
        end-of-startup sweep guarantees every loading component reaches
        a terminal phase, so the wait can never be permanent.
        """
        pending = set(names)
        while pending:
            with self._lock:
                for n in list(pending):
                    c = self._components.get(n)
                    if c is None:
                        raise GateError(
                            f"required component {n!r} is not a declared component",
                            {"component": n},
                        )
                    if c.phase == "failed":
                        raise GateError(
                            f"required component {n!r} failed"
                            + (f": {c.detail}" if c.detail else ""),
                            {"component": n, "phase": "failed"},
                        )
                    if c.phase == "ready":
                        pending.discard(n)
                if not pending:
                    return
                fut: asyncio.Future[None] = asyncio.get_running_loop().create_future()
                self._waiters.append(fut)
            try:
                await fut
            finally:
                if not fut.done():
                    fut.cancel()

    def _notify_waiters(self) -> None:
        with self._lock:
            waiters, self._waiters = self._waiters, []
        for f in waiters:
            if not f.done():
                try:
                    f.get_loop().call_soon_threadsafe(f.set_result, None)
                except RuntimeError:
                    pass  # the waiter's loop already closed

    # ---- lifecycle -------------------------------------------------------

    def reset(self) -> None:
        """Drop all state and stop the flusher (test isolation)."""
        self._stopping = True
        self._wake.set()
        flusher = self._flusher
        if flusher is not None and flusher is not threading.current_thread():
            flusher.join(timeout=5)
        with self._lock:
            self._components.clear()
            self._dirty.clear()
            self._waiters.clear()
            self._send = None
            self._flusher = None
            self._stopping = False


_default = Readiness()

# ---- module-level API ------------------------------------------------------


def on_startup(fn: Callable[..., Any] | None = None) -> Callable[..., Any]:
    """Register a post-hello startup hook (decision 0007 §1).

    Hooks run once per generation, started in registration order and
    concurrent with dispatch: `async def` hooks become loop tasks, `def`
    hooks run on the default executor under contextvars.copy_context().
    A hook exception fails its touched components, never the runtime.

    Usable bare (@on_startup) or with parens (@on_startup()).
    """

    def deco(f: Callable[..., Any]) -> Callable[..., Any]:
        _hooks.append(f)
        return f

    return deco if fn is None else deco(fn)


def loading(
    name: str, progress: float | None = None, detail: str | None = None
) -> None:
    """Report a component as loading (optional progress in [0, 1])."""
    _default.report(name, "loading", progress=progress, detail=detail)


def ready(name: str) -> None:
    """Report a component as ready."""
    _default.report(name, "ready")


def failed(name: str, detail: str | None = None) -> None:
    """Report a component as failed."""
    _default.report(name, "failed", detail=detail)


def declare(name: str) -> None:
    """Declare a `requires=` component at decoration (phase loading)."""
    _default.declare(name)


def startup_hooks() -> list[Callable[..., Any]]:
    return list(_hooks)


def attach(send: Callable[[dict], None]) -> None:
    _default.attach(send)


async def wait_ready(names: Iterable[str]) -> None:
    await _default.wait_ready(names)


def sweep() -> None:
    _default.sweep()


async def run_hook(fn: Callable[..., Any]) -> None:
    """Run one startup hook under the touched-set contextvar.

    Async hooks run directly; sync hooks run on the default executor
    under contextvars.copy_context() (executor threads do not propagate
    context). Exceptions become component failures, never crashes
    (0007 §1).
    """
    names_touched: set[str] = set()
    token = _touched.set(names_touched)
    try:
        if inspect.iscoroutinefunction(fn):
            await fn()
        else:
            ctx = contextvars.copy_context()
            await asyncio.get_running_loop().run_in_executor(None, ctx.run, fn)
    except asyncio.CancelledError:
        raise
    except BaseException as e:  # noqa: BLE001 - a hook failure is app state
        _default.attribute_failure(fn, names_touched, e)
    finally:
        _touched.reset(token)


async def run_startup() -> None:
    """Run every registered hook (started in registration order), then
    the end-of-startup sweep. Zero hooks still sweeps — a `requires=`
    name nothing will report fails immediately."""
    hooks = startup_hooks()
    if hooks:
        await asyncio.gather(*(run_hook(fn) for fn in hooks), return_exceptions=True)
    _default.sweep()


def reset() -> None:
    """Clear hooks and registry state (test isolation)."""
    _hooks.clear()
    _default.reset()
