"""Warm-pool health reporting (decision 0008 §health, docs/inspector.md):
the stats piggybacked on each pong reflect the pool's real state through
spawn, busy, idle reap and crash. Driven against a real runtime — the
pong is read straight off the wire after a host ping.
"""

from __future__ import annotations

import pytest

from conftest import Runtime
from pylair import _protocol as proto


def call(rid: int, fn: str, task: int, input: dict | None = None) -> dict:
    return proto.request(rid, "call", {"fn": fn, "task": task, "input": input or {}})


def ping(rid: int, seq: int) -> dict:
    return proto.request(rid, "ping", {"seq": seq})


def pools_of(runtime: Runtime, rid: int, seq: int) -> dict:
    """Send a ping, return the pong's pools dict (empty if absent)."""
    runtime.send(ping(rid, seq))
    pong = runtime.recv_until(lambda m: m.get("method") == "pong", timeout=120)
    return pong["params"].get("pools", {})


@pytest.fixture
def runtime():
    rt = Runtime("testapp")
    try:
        yield rt
    finally:
        rt.kill()


def test_stats_absent_before_first_warm_call(runtime):
    """No warm fn ever called -> no pools, no entries."""
    runtime.recv(timeout=120)  # hello
    assert pools_of(runtime, 900, 1) == {}


def test_stats_spawn_idle_then_busy_then_idle(runtime):
    """A warm call spawns the pool's first member: after it, the pong
    reports live=1 idle=1; during a slow call busy=1 queued reflects
    the FIFO; after the terminal, idle again."""
    runtime.recv(timeout=120)
    runtime.send(call(1, "warm_pid", task=1))
    runtime.recv_until(lambda m: m.get("id") == 1, timeout=120)

    pools = pools_of(runtime, 900, 1)
    assert "warm_pid" in pools
    st = pools["warm_pid"]
    assert st["size"] == 1
    assert st["live"] == 1
    assert st["busy"] == 0
    assert st["idle"] == 1
    assert st["queued"] == 0
    assert st["deaths"] == 0
    assert st["crash_loop"] is False
    assert st["reaps"] == 0

    # Busy: a 1.5s sleep occupies the member; a second call queues.
    runtime.send(call(2, "warm_sleep", task=2, input={"seconds": 1.5}))
    runtime.send(call(3, "warm_sleep", task=3, input={"seconds": 0.01}))
    import time

    time.sleep(0.5)  # let task 2 reach the member
    pools = pools_of(runtime, 901, 2)
    st = pools["warm_sleep"]
    assert st["busy"] == 1
    assert st["idle"] == 0
    assert st["queued"] == 1

    runtime.recv_until(lambda m: m.get("id") == 2, timeout=120)
    runtime.recv_until(lambda m: m.get("id") == 3, timeout=120)
    pools = pools_of(runtime, 902, 3)
    st = pools["warm_sleep"]
    assert st["busy"] == 0
    assert st["idle"] == 1
    assert st["queued"] == 0


def test_stats_deaths_and_crash_loop(runtime):
    """Three consecutive member deaths surface in deaths and flip the
    crash_loop flag once the pool stops."""
    runtime.recv(timeout=120)
    for i in range(3):
        runtime.send(call(i + 1, "warm_maybe_crash", task=i + 1, input={"crash": True}))
        resp = runtime.recv_until(lambda m, i=i: m.get("id") == i + 1, timeout=120)
        assert resp["error"]["code"] == proto.WORKER_DIED
    pools = pools_of(runtime, 900, 1)
    st = pools["warm_maybe_crash"]
    assert st["deaths"] >= 3
    assert st["crash_loop"] is True


def test_stats_reap_counts_idle_ttl(tmp_path):
    """An idle member past its TTL is reaped: live drops and reaps
    increments — the two fields an operator watches for churn."""
    rt = Runtime("testapp_prewarm_ttl")
    try:
        rt.recv(timeout=120)  # hello
        import time

        # Prewarm spawns the member; wait for it, then for the TTL
        # (the fixture uses a short one) to fire the reap.
        deadline = time.time() + 60
        pools = {}
        while time.time() < deadline:
            pools = pools_of(rt, 900, 1)
            if pools and any(st["live"] >= 1 for st in pools.values()):
                break
            time.sleep(0.2)
        assert pools, "pool never reported"
        assert any(st["live"] >= 1 for st in pools.values())

        deadline = time.time() + 60
        reaped = False
        while time.time() < deadline:
            pools = pools_of(rt, 901, 2)
            if any(st["reaps"] >= 1 and st["live"] == 0 for st in pools.values()):
                reaped = True
                break
            time.sleep(0.3)
        assert reaped, f"idle member never reaped: {pools}"
    finally:
        rt.kill()
