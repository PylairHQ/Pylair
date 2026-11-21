"""App-defined readiness (decision 0007).

Unit tests drive the registry, flusher, hooks, and the requires= task
gate in-process; the e2e block drives spawned runtimes (the testapp_*
modules park their hooks on wire-reachable gates, so nothing asserts
on a fixed sleep).
"""

from __future__ import annotations

import asyncio
import threading
import time

import pytest

import pylair._expose as _ex
from conftest import Runtime
from pylair import _json, _readiness, expose
from pylair import _protocol as proto
from pylair._readiness import GateError, Readiness
from pylair._tasks import TaskRegistry
from test_protocol import call


@pytest.fixture(autouse=True)
def _isolated_state():
    """Keep the module-level registries private to each test."""
    saved = dict(_ex._registry)
    _ex._registry.clear()
    yield
    _ex._registry.clear()
    _ex._registry.update(saved)
    _readiness.reset()


class _Capture:
    """Outbound frame sink standing in for Server._send."""

    def __init__(self) -> None:
        self.frames: list[dict] = []

    def send(self, msg: dict) -> None:
        self.frames.append(msg)

    def component_frames(self, name: str | None = None) -> list[dict]:
        out = [f["params"] for f in self.frames if f.get("method") == "app.component"]
        return [p for p in out if name is None or p["name"] == name]


def _wait_for(pred, timeout: float = 10.0, what: str = "condition") -> None:
    deadline = time.monotonic() + timeout
    while not pred():
        if time.monotonic() > deadline:
            raise AssertionError(f"{what} never met")
        time.sleep(0.005)


async def _until_flag(ev: threading.Event, timeout: float = 10.0) -> None:
    loop = asyncio.get_running_loop()
    deadline = loop.time() + timeout
    while not ev.is_set():
        if loop.time() > deadline:
            raise AssertionError("flag never set")
        await asyncio.sleep(0.01)


# ---- registry ------------------------------------------------------------


def test_report_updates_registry_for_each_phase():
    r = Readiness()
    r.report("m", "loading", progress=0.4, detail="half")
    c = r._components["m"]
    assert (c.phase, c.progress, c.detail, c.reported) == (
        "loading",
        0.4,
        "half",
        True,
    )
    r.report("m", "ready")
    c = r._components["m"]
    assert (c.phase, c.progress, c.detail) == ("ready", None, None)
    r.report("m", "failed", detail="later regression")
    c = r._components["m"]
    assert (c.phase, c.detail) == ("failed", "later regression")


def test_reports_from_plain_thread_and_loop_both_land():
    r = Readiness(flush_s=3600.0)
    cap = _Capture()
    r.attach(cap.send)
    try:
        t = threading.Thread(target=lambda: r.report("t", "ready"))
        t.start()
        t.join(timeout=5)

        async def go() -> None:
            r.report("l", "ready")

        asyncio.run(go())
        _wait_for(lambda: {"t", "l"} <= {p["name"] for p in cap.component_frames()})
    finally:
        r.reset()


# ---- flusher: coalescing, immediate transitions, caps --------------------


def test_flusher_coalesces_latest_wins_and_transitions_immediately():
    r = Readiness(flush_s=3600.0)  # the interval can never tick in-test
    cap = _Capture()
    r.attach(cap.send)
    try:
        r.report("m", "loading", progress=0.4)
        _wait_for(lambda: len(cap.component_frames("m")) == 1)
        assert cap.component_frames("m")[0]["progress"] == 0.4
        # Same-phase updates mark dirty but post no wake — the next
        # reports can only ride a later flush.
        r.report("m", "loading", progress=0.6)
        r.report("m", "loading", progress=0.9)
        r.report("m", "ready")
        _wait_for(lambda: len(cap.component_frames("m")) == 2)
        # The 0.6/0.9 updates never got a frame of their own — the
        # transition emitted the latest state wholesale.
        assert cap.component_frames("m")[1] == {"name": "m", "phase": "ready"}
    finally:
        r.reset()


def test_10k_progress_reports_emit_only_transition_frames():
    """The _out tripwire can never be reached by reporting alone
    (0007 §4): one frame per component per interval — and here the
    interval is an hour, so only transitions may emit."""
    r = Readiness(flush_s=3600.0)
    cap = _Capture()
    r.attach(cap.send)
    try:
        for i in range(10_000):
            r.report("m", "loading", progress=i / 10_000)
        _wait_for(lambda: len(cap.component_frames("m")) == 1)
        # No second frame is possible without a transition or the tick.
        assert len(cap.frames) == 1
        r.report("m", "ready")
        _wait_for(lambda: len(cap.frames) == 2)
        r.reset()
        assert len(cap.frames) == 2  # 10_001 reports -> 2 frames
    finally:
        r.reset()


def test_progress_out_of_range_or_nonfinite_becomes_absent():
    r = Readiness()
    for bad in (1.5, -0.1, float("nan"), float("inf"), True):
        r.report("m", "loading", progress=bad)
        assert r._components["m"].progress is None, bad
    r.report("m", "loading", progress=0.5)
    assert r._components["m"].progress == 0.5


def test_reports_before_attach_ride_the_first_flush_once():
    """Pre-server reports are kept: loading()+ready() at import emit
    exactly one ready frame in the first post-hello flush; a declared
    component emits exactly one loading frame (0007 §4)."""
    r = Readiness(flush_s=3600.0)
    r.report("boot", "loading", progress=0.1)
    r.report("boot", "ready")
    r.declare("model")
    cap = _Capture()
    r.attach(cap.send)
    try:
        _wait_for(lambda: len(cap.frames) == 2)
        by_name = {p["name"]: p for p in cap.component_frames()}
        assert by_name["boot"]["phase"] == "ready"
        assert by_name["model"] == {"name": "model", "phase": "loading"}
        assert len(cap.component_frames("boot")) == 1
        assert len(cap.component_frames("model")) == 1
    finally:
        r.reset()


def test_declared_but_reported_ready_skips_the_loading_shot():
    """A declaration reported ready before the first flush emits
    `ready`, never `loading` (0007 §4 latest-wins rule)."""
    r = Readiness(flush_s=3600.0)
    r.declare("early")
    r.report("early", "ready")
    cap = _Capture()
    r.attach(cap.send)
    try:
        _wait_for(lambda: len(cap.frames) == 1)
        assert cap.component_frames("early") == [{"name": "early", "phase": "ready"}]
    finally:
        r.reset()


def test_reports_after_reset_are_silent():
    r = Readiness()
    r.attach(lambda m: None)
    r.reset()
    r.report("m", "ready")  # records quietly — never raises
    assert r._components["m"].phase == "ready"


# ---- startup hooks ---------------------------------------------------------


def test_async_hook_runs_on_the_loop():
    seen: dict[str, str] = {}

    @_readiness.on_startup
    async def h() -> None:
        seen["thread"] = threading.current_thread().name
        _readiness.loading("m")
        _readiness.ready("m")

    async def go() -> None:
        await _readiness.run_startup()
        assert seen["thread"] == threading.current_thread().name
        assert _readiness._default._components["m"].phase == "ready"

    asyncio.run(go())


def test_sync_hook_runs_on_an_executor():
    seen: dict[str, str] = {}

    @_readiness.on_startup
    def h() -> None:
        seen["thread"] = threading.current_thread().name
        _readiness.ready("m")

    async def go() -> None:
        await _readiness.run_startup()
        assert seen["thread"] != threading.current_thread().name
        assert _readiness._default._components["m"].phase == "ready"

    asyncio.run(go())


def test_hook_exception_fails_touched_loading_components():
    @_readiness.on_startup
    def boom() -> None:
        _readiness.loading("a")
        _readiness.loading("b")
        _readiness.ready("b")
        raise RuntimeError("checkpoint corrupt")

    async def go() -> None:
        await _readiness.run_startup()
        comps = _readiness._default._components
        assert comps["a"].phase == "failed"
        assert "RuntimeError" in comps["a"].detail
        assert "checkpoint corrupt" in comps["a"].detail
        # Terminal components are untouched by the failure.
        assert comps["b"].phase == "ready"

    asyncio.run(go())


def test_hook_exception_untouched_synthesizes_startup_name():
    @_readiness.on_startup
    def quiet_boom() -> None:
        raise KeyError("nothing reported")

    async def go() -> None:
        await _readiness.run_startup()
        c = _readiness._default._components["startup:quiet_boom"]
        assert c.phase == "failed"
        assert "KeyError" in c.detail

    asyncio.run(go())


def test_sync_hook_attribution_goes_through_copy_context():
    """Without contextvars.copy_context() the executor thread would see
    a default context and `loading()` would not attribute — the failure
    would synthesize startup:<name> instead of failing `s`."""

    @_readiness.on_startup
    def h() -> None:
        _readiness.loading("s")
        raise RuntimeError("boom")

    async def go() -> None:
        await _readiness.run_startup()
        comps = _readiness._default._components
        assert comps["s"].phase == "failed"
        assert "boom" in comps["s"].detail
        assert "startup:h" not in comps

    asyncio.run(go())


def test_later_hooks_still_run_after_earlier_failure():
    @_readiness.on_startup
    def bad() -> None:
        raise RuntimeError("first fails")

    @_readiness.on_startup
    def good() -> None:
        _readiness.ready("b")

    async def go() -> None:
        await _readiness.run_startup()
        assert _readiness._default._components["b"].phase == "ready"

    asyncio.run(go())


def test_on_startup_paren_form_registers():
    @_readiness.on_startup()
    def h() -> None:
        _readiness.ready("m")

    async def go() -> None:
        await _readiness.run_startup()
        assert _readiness._default._components["m"].phase == "ready"

    asyncio.run(go())


def test_shutdown_cancels_async_hooks():
    started = asyncio.Event()
    released = asyncio.Event()

    @_readiness.on_startup
    async def h() -> None:
        started.set()
        await released.wait()

    async def go() -> None:
        task = asyncio.create_task(_readiness.run_startup())
        await asyncio.wait_for(started.wait(), 5)
        task.cancel()
        with pytest.raises(asyncio.CancelledError):
            await task

    asyncio.run(go())


def test_shutdown_does_not_wait_on_sync_hooks():
    """A cancelled sync hook is abandoned like inline sync calls — the
    startup task is cancelled promptly, the executor work is discarded."""
    parked = threading.Event()
    release = threading.Event()

    @_readiness.on_startup
    def h() -> None:
        parked.set()
        release.wait(timeout=30)

    async def go() -> None:
        task = asyncio.create_task(_readiness.run_startup())
        await _until_flag(parked)
        t0 = asyncio.get_running_loop().time()
        task.cancel()
        with pytest.raises(asyncio.CancelledError):
            await task
        assert asyncio.get_running_loop().time() - t0 < 10, (
            "cancelling startup waited on the parked sync hook"
        )
        release.set()  # free the executor thread before asyncio.run exits

    asyncio.run(go())


# ---- the sweep -----------------------------------------------------------


def test_sweep_fails_never_reported_declarations():
    _readiness.declare("ghost")
    _readiness._default.sweep()
    c = _readiness._default._components["ghost"]
    assert c.phase == "failed"
    assert c.detail == "never reported by a startup hook"


def test_sweep_fails_reported_but_unfinished_components():
    _readiness._default.report("m", "loading")
    _readiness._default.sweep()
    c = _readiness._default._components["m"]
    assert c.phase == "failed"
    assert c.detail == "startup hook finished without reporting ready"


def test_sweep_leaves_terminal_components():
    _readiness._default.report("a", "ready")
    _readiness._default.report("b", "failed", detail="bad")
    _readiness._default.sweep()
    comps = _readiness._default._components
    assert comps["a"].phase == "ready"
    assert comps["b"].detail == "bad"  # sweep must not overwrite detail


def test_run_startup_with_zero_hooks_sweeps_immediately():
    _readiness.declare("ghost")

    async def go() -> None:
        await _readiness.run_startup()
        c = _readiness._default._components["ghost"]
        assert c.phase == "failed"
        assert c.detail == "never reported by a startup hook"

    asyncio.run(go())


# ---- wait_ready (the requires= wait) --------------------------------------


def test_wait_ready_returns_at_once_when_ready():
    async def go() -> None:
        _readiness.ready("m")
        await _readiness.wait_ready(("m",))

    asyncio.run(go())


def test_wait_ready_waits_for_the_transition():
    async def go() -> None:
        _readiness.declare("m")
        task = asyncio.create_task(_readiness.wait_ready(("m",)))
        await asyncio.sleep(0)
        await asyncio.sleep(0)
        assert not task.done(), "gate opened before the component was ready"
        _readiness.ready("m")
        await asyncio.wait_for(task, 5)

    asyncio.run(go())


def test_wait_ready_multiple_components():
    async def go() -> None:
        _readiness.declare("a")
        _readiness.declare("b")
        task = asyncio.create_task(_readiness.wait_ready(("a", "b")))
        await asyncio.sleep(0)
        _readiness.ready("a")
        await asyncio.sleep(0)
        assert not task.done(), "gate opened on a partial ready set"
        _readiness.ready("b")
        await asyncio.wait_for(task, 5)

    asyncio.run(go())


def test_wait_ready_failed_component_raises_gate_error():
    async def go() -> None:
        _readiness.failed("m", "checkpoint corrupt")
        with pytest.raises(GateError) as e:
            await _readiness.wait_ready(("m",))
        assert e.value.data == {"component": "m", "phase": "failed"}

    asyncio.run(go())


def test_wait_ready_undeclared_name_errors_loudly():
    async def go() -> None:
        with pytest.raises(GateError) as e:
            await _readiness.wait_ready(("phantom",))
        assert e.value.data["component"] == "phantom"

    asyncio.run(go())


def test_wait_ready_cancellation():
    async def go() -> None:
        _readiness.declare("m")
        task = asyncio.create_task(_readiness.wait_ready(("m",)))
        await asyncio.sleep(0)
        task.cancel()
        with pytest.raises(asyncio.CancelledError):
            await task

    asyncio.run(go())


# ---- expose(requires=) -----------------------------------------------------


def test_expose_requires_accepts_str_and_iterable():
    @expose(requires="model")
    def a() -> str:
        return "a"

    @expose(requires=("m1", "m2"))
    def b() -> str:
        return "b"

    assert _ex._registry["a"].requires == ("model",)
    assert _ex._registry["b"].requires == ("m1", "m2")


def test_expose_requires_declares_at_decoration():
    @expose(requires=("model", "index"))
    def f() -> str:
        return "f"

    comps = _readiness._default._components
    assert comps["model"].phase == "loading"
    assert comps["index"].phase == "loading"
    assert comps["model"].reported is False


def test_expose_requires_rejects_bad_values():
    for bad in ("", ("ok", ""), ("ok", 5), 5):
        with pytest.raises(ValueError):
            expose(requires=bad)(lambda: None)


# ---- the task gate (_run_gated) --------------------------------------------


async def _gated_result() -> str:
    return "done"


def test_gated_call_holds_task_slot_while_waiting():
    async def go() -> None:
        sent: list[dict] = []
        reg = TaskRegistry(sent.append)
        _readiness.declare("m")
        await reg.start(1, 1, "f", "async", _gated_result, {}, requires=("m",))
        task = reg.get(1)
        # The wait is inside the task: the slot is held for the whole
        # wait (0007 §2 slot accounting) and nothing has been sent.
        assert task is not None
        assert sent == []
        _readiness.ready("m")
        await asyncio.wait_for(task.asyncio_task, 5)
        assert sent[-1] == {"jsonrpc": "2.0", "id": 1, "result": "done"}

    asyncio.run(go())


def test_gated_call_failed_component_is_internal_with_component_data():
    async def go() -> None:
        sent: list[dict] = []
        reg = TaskRegistry(sent.append)
        _readiness.failed("m", "checkpoint corrupt")
        await reg.start(1, 1, "f", "async", _gated_result, {}, requires=("m",))
        task = reg.get(1)
        await asyncio.wait_for(task.asyncio_task, 5)
        err = sent[-1]["error"]
        assert err["code"] == proto.INTERNAL
        assert err["data"] == {"component": "m", "phase": "failed"}
        assert "checkpoint corrupt" in err["message"]

    asyncio.run(go())


async def _wait_sent(sent: list[dict], pred, timeout: float = 5.0) -> dict:
    """Wait for a frame matching pred to appear in `sent`."""
    loop = asyncio.get_running_loop()
    deadline = loop.time() + timeout
    while True:
        for m in sent:
            if pred(m):
                return m
        if loop.time() > deadline:
            raise AssertionError("frame never arrived")
        await asyncio.sleep(0.01)


def test_gated_call_cancelled_during_wait_is_task_cancelled():
    async def go() -> None:
        sent: list[dict] = []
        reg = TaskRegistry(sent.append)
        _readiness.declare("m")
        await reg.start(1, 1, "f", "async", _gated_result, {}, requires=("m",))
        reg.cancel(1, call_id=2)
        terminal = await _wait_sent(sent, lambda m: m.get("id") == 1)
        assert terminal["error"]["code"] == proto.TASK_CANCELLED
        ack = next(m for m in sent if m.get("id") == 2)
        assert ack["result"]["cancelled"] is True

    asyncio.run(go())


def test_isolated_gate_waits_before_worker_spawn(monkeypatch):
    """For isolated functions the wait happens before the worker spawn:
    the isolated share is held, but no worker is created while the
    component is loading (0007 §2)."""
    spawned: list[tuple] = []

    class _Proc:
        returncode = 0

        async def communicate(self, _data):
            return (_json.dumps({"result": "ok"}).encode(), b"")

        async def wait(self):
            return 0

    async def fake_exec(*args, **_kwargs):
        spawned.append(args)
        return _Proc()

    monkeypatch.setattr(asyncio, "create_subprocess_exec", fake_exec)
    monkeypatch.setenv("PYLAIR_APP_MODULE", "testapp")

    async def go() -> None:
        sent: list[dict] = []
        reg = TaskRegistry(sent.append)
        _readiness.declare("m")
        await reg.start(1, 1, "iso", "isolated", lambda: None, {}, requires=("m",))
        task = reg.get(1)
        await asyncio.sleep(0)
        await asyncio.sleep(0)
        assert reg._isolated == 1, "isolated share not held during wait"
        assert task.worker is None
        assert spawned == [], "worker spawned while component still loading"
        _readiness.ready("m")
        await asyncio.wait_for(task.asyncio_task, 5)
        assert spawned, "worker never spawned after ready"
        assert sent[-1]["result"] == "ok"

    asyncio.run(go())


# ---- end to end ------------------------------------------------------------


def _collect_until(rt: Runtime, pred, timeout: float = 20.0) -> list[dict]:
    """Collect app.component params until pred(frames) or timeout."""
    frames: list[dict] = []
    deadline = time.time() + timeout
    while not pred(frames):
        msg = rt.recv(timeout=max(0.1, deadline - time.time()))
        if msg.get("method") == "app.component":
            frames.append(msg["params"])
    return frames


def test_first_flush_carries_prehello_reports_and_declared_loading():
    """The first post-hello flush: import-time reports coalesce to one
    frame each, and a requires=-declared component emits exactly one
    loading frame — health().app shows it from the start."""
    with Runtime("testapp_readiness_prehello") as rt:
        rt.recv()  # hello
        frames: list[dict] = []
        deadline = time.time() + 20
        rt.send(call(1, "chat", task=1, input={"prompt": "hi"}))
        # The declared `model` loading one-shot must be observed before
        # release — otherwise the hook's ready could beat the first
        # flush and there would be no loading frame to see.
        while not any(p["name"] == "model" and p["phase"] == "loading" for p in frames):
            m = rt.recv(timeout=max(0.1, deadline - time.time()))
            if m.get("method") == "app.component":
                frames.append(m["params"])
        rt.send(call(2, "release", task=2))
        resp1 = resp2 = None
        order: list[int] = []
        while resp1 is None or resp2 is None:
            m = rt.recv(timeout=max(0.1, deadline - time.time()))
            if m.get("method") == "app.component":
                frames.append(m["params"])
            elif m.get("id") == 1:
                resp1, order = m, order + [1]
            elif m.get("id") == 2:
                resp2, order = m, order + [2]

        by_name: dict[str, list[dict]] = {}
        for p in frames:
            by_name.setdefault(p["name"], []).append(p)
        # loading()+ready() at import -> exactly one ready frame.
        assert by_name["boot"] == [{"name": "boot", "phase": "ready"}]
        # Declared AND already ready -> one ready frame, never loading.
        assert by_name["early"] == [{"name": "early", "phase": "ready"}]
        # Declared, unreported -> exactly one loading frame, then ready.
        assert [f["phase"] for f in by_name["model"]] == ["loading", "ready"]
        # The gated call waited for `model` and answered after release.
        assert order[0] == 2, "release should answer before the gated call"
        assert resp1["result"] == "answered:hi"


def test_call_dispatched_mid_hook_is_answered():
    """Hooks run concurrently with dispatch: a call sent while hooks are
    parked is answered, not queued behind them."""
    with Runtime("testapp_readiness") as rt:
        rt.recv()  # hello
        # The hook's own report (detail'd) proves it reached its gate.
        rt.recv_until(
            lambda m: (
                m.get("method") == "app.component"
                and m["params"].get("detail") == "checkpoint 1/10"
            )
        )
        rt.send(call(1, "add", task=1, input={"a": 2, "b": 3}))
        resp = rt.recv_until(lambda m: m.get("id") == 1)
        assert resp["result"] == 5
        # Unwedge the hooks so the runtime drains cleanly on exit.
        rt.send(call(2, "release_hooks", task=2))
        rt.recv_until(lambda m: m.get("id") == 2)


def test_first_call_race_gated_call_waits_then_succeeds():
    """A requires= call dispatched before the hook's first line waits in
    `loading` (declared at decoration) — never 'unknown component'."""
    with Runtime("testapp_readiness") as rt:
        rt.recv()  # hello — send the gated call immediately
        rt.send(call(1, "chat", task=1, input={"prompt": "hi"}))
        rt.send(call(2, "add", task=2, input={"a": 1, "b": 1}))
        # `add` answers while `chat` waits: the gate held.
        resp2 = rt.recv_until(lambda m: m.get("id") == 2)
        assert resp2["result"] == 2
        rt.send(call(3, "release_hooks", task=3))
        rt.recv_until(lambda m: m.get("id") == 3)
        resp1 = rt.recv_until(lambda m: m.get("id") == 1)
        assert resp1["result"] == "answered:hi"


def test_cancel_during_readiness_wait_is_task_cancelled():
    with Runtime("testapp_readiness") as rt:
        rt.recv()
        rt.send(call(1, "chat", task=7, input={"prompt": "x"}))
        rt.send(proto.request(2, "cancel", {"task": 7}))
        ack = rt.recv_until(lambda m: m.get("id") == 2)
        assert ack["result"]["cancelled"] is True
        terminal = rt.recv_until(lambda m: m.get("id") == 1)
        assert terminal["error"]["code"] == proto.TASK_CANCELLED
        # Unwedge for a clean teardown.
        rt.send(call(3, "release_hooks", task=3))
        rt.recv_until(lambda m: m.get("id") == 3)


def test_isolated_requires_wait_holds_share_without_spawning(tmp_path):
    """An isolated requires= call holds its share but spawns no worker
    until the component is ready."""
    with Runtime("testapp_readiness") as rt:
        rt.recv()
        pidfile = tmp_path / "w.pid"
        rt.send(
            call(
                1,
                "isolated_ready_work",
                task=1,
                input={"pidfile": str(pidfile)},
            )
        )
        rt.send(call(2, "add", task=2, input={"a": 1, "b": 1}))
        resp2 = rt.recv_until(lambda m: m.get("id") == 2)
        assert resp2["result"] == 2
        # `add` already answered and `model` is still loading — the
        # gated isolated call is waiting, so no worker can exist yet.
        assert not pidfile.exists(), "worker spawned during readiness wait"
        rt.send(call(3, "release_hooks", task=3))
        resp1 = rt.recv_until(lambda m: m.get("id") == 1)
        assert resp1["result"] == "worker-ran"


def test_hook_failures_sweep_and_gated_errors_end_to_end():
    with Runtime("testapp_readiness_fail") as rt:
        rt.recv()
        expected = {"model", "startup:quiet_hook", "sidecar", "ghost"}
        frames = _collect_until(
            rt,
            lambda f: expected
            <= {
                p["name"]
                for p in f
                if p["phase"] in ("ready", "failed")
            },
        )
        by_name = {p["name"]: p for p in frames}
        assert by_name["model"]["phase"] == "failed"
        assert "checkpoint corrupt" in by_name["model"]["detail"]
        assert by_name["startup:quiet_hook"]["phase"] == "failed"
        assert "KeyError" in by_name["startup:quiet_hook"]["detail"]
        assert by_name["sidecar"]["phase"] == "ready"
        assert by_name["ghost"]["phase"] == "failed"
        assert by_name["ghost"]["detail"] == "never reported by a startup hook"

        rt.send(call(1, "chat", task=1))
        err = rt.recv_until(lambda m: m.get("id") == 1)["error"]
        assert err["code"] == proto.INTERNAL
        assert err["data"] == {"component": "model", "phase": "failed"}

        rt.send(call(2, "ghost_call", task=2))
        err = rt.recv_until(lambda m: m.get("id") == 2)["error"]
        assert err["data"] == {"component": "ghost", "phase": "failed"}

        rt.send(call(3, "ping", task=3))
        assert rt.recv_until(lambda m: m.get("id") == 3)["result"] == "pong"


def test_zero_hooks_sweep_end_to_end():
    with Runtime("testapp_readiness_sweep") as rt:
        rt.recv()
        frames = _collect_until(
            rt,
            lambda f: any(p["name"] == "ghost" and p["phase"] == "failed" for p in f),
        )
        ghost = [p for p in frames if p["name"] == "ghost"]
        assert ghost[-1]["phase"] == "failed"
        assert ghost[-1]["detail"] == "never reported by a startup hook"
        rt.send(call(1, "ghost_call", task=1))
        err = rt.recv_until(lambda m: m.get("id") == 1)["error"]
        assert err["code"] == proto.INTERNAL
        assert err["data"] == {"component": "ghost", "phase": "failed"}
        rt.send(call(2, "ping", task=2))
        assert rt.recv_until(lambda m: m.get("id") == 2)["result"] == "pong"


def test_hook_kinds_record_loop_vs_executor_threads():
    """async hooks run on the event loop; sync hooks on an executor —
    the recorded thread names must differ (0007 §1)."""
    with Runtime("testapp_readiness") as rt:
        rt.recv()
        # Wait for both hooks' own reports — proves each ran to its
        # gate and wrote its thread record before we read them.
        for detail in ("checkpoint 1/10", "warming index"):
            rt.recv_until(
                lambda m, d=detail: (
                    m.get("method") == "app.component"
                    and m["params"].get("detail") == d
                )
            )
        rt.send(call(1, "hook_records", task=1))
        records = rt.recv_until(lambda m: m.get("id") == 1)["result"]
        assert records["async_thread"] == "MainThread"
        assert records["sync_thread"] != records["async_thread"]
        rt.send(call(2, "release_hooks", task=2))
        rt.recv_until(lambda m: m.get("id") == 2)


def test_app_component_wire_shape_exact():
    """The notification carries exactly {name, phase} plus optional
    progress/detail — the 0007 §3 shape."""
    with Runtime("testapp_readiness") as rt:
        rt.recv()
        frame = rt.recv_until(
            lambda m: (
                m.get("method") == "app.component"
                and m["params"].get("detail") == "checkpoint 1/10"
            )
        )
        assert frame["jsonrpc"] == "2.0"
        assert frame["method"] == "app.component"
        assert frame["params"] == {
            "name": "model",
            "phase": "loading",
            "progress": 0.1,
            "detail": "checkpoint 1/10",
        }
        rt.send(call(1, "release_hooks", task=1))
        rt.recv_until(lambda m: m.get("id") == 1)


def test_shutdown_proceeds_with_hooks_parked():
    """Hooks share the drain deadline: the shutdown response is sent
    even while an async hook is parked and a sync hook is parked on an
    executor thread that outlives the drain."""
    with Runtime("testapp_readiness") as rt:
        rt.recv()
        # Both hooks parked — async on the loop gate, sync on the
        # executor threading.Event.
        for detail in ("checkpoint 1/10", "warming index"):
            rt.recv_until(
                lambda m, d=detail: (
                    m.get("method") == "app.component"
                    and m["params"].get("detail") == d
                )
            )
        rt.send(proto.request(1, "shutdown", {"grace_ms": 5000}))
        resp = rt.recv_until(lambda m: m.get("id") == 1, timeout=15)
        assert resp["result"] == {}
