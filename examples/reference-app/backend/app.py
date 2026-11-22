"""Reference app backend — a local-AI workflow over the Pylair bridge.

Covers the four M6 acceptance criteria: token-by-token streaming from a
real model (Ollama), safe cancellation, deliberate crash + supervised
recovery, and packaged installs on machines with no Python.

`add` and `count` stay exposed: the webview self-test
(PYLAIR_SELF_TEST_WEBVIEW, run by the acceptance job) drives them.
"""

import asyncio
import os
import sys
from collections.abc import AsyncIterator

from pylair import expose
from pylair_ollama import Message, chat as ollama_chat, status as ollama_status

DEFAULT_MODEL = os.environ.get("REFERENCE_MODEL", "qwen2.5:0.5b")


@expose
async def status() -> dict:
    """Ollama reachability + model list — drives the UI status badge.
    Never raises: the app must still work when Ollama is absent (clean
    install machines have no Ollama either)."""
    s = await ollama_status()
    return {
        "installed": s.installed,
        "running": s.running,
        "version": s.version,
        "models": s.models,
        "default_model": DEFAULT_MODEL,
        "error": s.error,
    }


@expose
async def chat(prompt: str, model: str | None = None) -> AsyncIterator[str]:
    """Token-by-token stream from a local Ollama model. Cancelling the
    consuming task aborts the HTTP request mid-stream (pylair_ollama owns
    the httpx lifecycle), so a cancelled chat releases its connection."""
    async for delta in ollama_chat(
        model or DEFAULT_MODEL, [Message(role="user", content=prompt)]
    ):
        yield delta


@expose
async def demo_stream(tokens: int = 80, delay_ms: int = 60) -> AsyncIterator[str]:
    """Deterministic token-ish stream when no Ollama is present — same
    streaming and cancellation path as `chat`, exercisable on clean CI
    machines that have no model server."""
    for i in range(tokens):
        yield f"token-{i} "
        await asyncio.sleep(delay_ms / 1000)


@expose
def crash() -> None:
    """Kill the runtime process on purpose. The supervisor must detect the
    exit, restart the runtime, and the UI watches the state transitions."""
    os._exit(3)


# ---- exercised by the packaged self-tests (acceptance job) ----
# `hello` is the plugin's headless/GUI self-test call; `add`/`count` are
# driven through the webview by PYLAIR_SELF_TEST_WEBVIEW.


@expose
def hello(name: str = "world") -> str:
    return f"hello, {name} — from python"


@expose
async def add(a: int, b: int) -> int:
    return a + b


@expose
async def count(n: int = 5) -> AsyncIterator[int]:
    for i in range(n):
        yield i
        await asyncio.sleep(0.2)


@expose
def log_stderr(msg: str) -> None:
    # The two-window self-test writes a marker line and asserts it reaches
    # only the owner window's pylair://stderr subscription.
    print(msg, file=sys.stderr)


@expose
def fn_permissions_fixture() -> None:
    # Presence tells the two-window self-test that this build enforces
    # per-function permissions, so selftest-winb.json's deny entry for win-b
    # must be honoured (CI appends the same function to scaffolded apps).
    return None


# Per-window fairness self-test (docs/decisions/0005): `hold` occupies a
# task slot until cancelled (or the timeout passes); `holds_active` is the
# rendezvous win-b polls to learn that main is actually holding before it
# probes its own share. Presence arms the self-test's window-fairness check
# (CI appends the same functions to scaffolded apps).
_holds_active = 0


@expose
async def hold(seconds: float = 30.0) -> str:
    global _holds_active
    _holds_active += 1
    try:
        await asyncio.sleep(seconds)
        return "held"
    finally:
        _holds_active -= 1


@expose
def holds_active() -> int:
    return _holds_active


# `b_ping` is win-b's liveness mark: the call itself is the bridge command
# that subscribes win-b to the fairness live set, so `b_seen` >= 1 on the
# main window is deterministic proof win-b holds a live entry before the
# flood — no event, no timing (CI appends the same functions).
_b_seen = 0


@expose
def b_ping() -> int:
    global _b_seen
    _b_seen += 1
    return _b_seen


@expose
def b_seen() -> int:
    return _b_seen


# App-readiness self-test (decisions 0007): the startup hook reports the
# "selftest" component loading then ready, and `selftest_ping` is gated
# on it via requires=. The ping/done pair re-reports the cycle as two
# REAL transitions — a single loading+ready pair inside one call would
# coalesce to a no-op on the wire, so a webview that subscribed to
# pylair://app after startup observes a phase event deterministically
# (CI appends the same functions to the scaffolded app).
from pylair import loading, on_startup, ready


@on_startup
def _selftest_component() -> None:
    loading("selftest", 0.5)
    ready("selftest")


@expose(requires=("selftest",))
def selftest_ping() -> str:
    loading("selftest", 0.9)
    return "pong"


@expose
def selftest_done() -> str:
    ready("selftest")
    return "done"
