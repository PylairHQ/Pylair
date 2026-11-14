"""Thin async Ollama client for Pylair apps.

Streams plain content deltas — when an app wraps `chat` in an @expose'd
async generator, tokens flow through the normal bridge: pull-credit
backpressure, and cancellation aborts the HTTP request because task
cancellation propagates into httpx.

Host resolution: `host` argument, then $OLLAMA_HOST, then 127.0.0.1:11434.
"""

from __future__ import annotations

import json
import os
import re
import shutil
from collections.abc import AsyncIterator
from dataclasses import dataclass, field
from urllib.parse import urlsplit

import httpx

DEFAULT_HOST = "http://127.0.0.1:11434"
_DEFAULT_PORT = 11434
# Bind addresses, not destinations: a client must dial loopback instead.
_ANY_HOSTS = {"0.0.0.0", "::"}


def normalize_host(value: str | None) -> str:
    """Normalize an OLLAMA_HOST (or host= arg) to a base URL.

    Accepts "host", "host:port", "http(s)://host[:port]", a bare ":port"
    and bracketed IPv6. Bind-any addresses (0.0.0.0, ::) map to loopback;
    a missing port defaults to 11434 and the scheme to http; any path,
    query or trailing slash is dropped. Raises OllamaError naming the
    variable on garbage.
    """

    def bad(reason: str = "expected [http(s)://]host[:port]") -> OllamaError:
        return OllamaError(f"invalid OLLAMA_HOST {value!r}: {reason}")

    raw = (value or "").strip()
    if not raw:
        return DEFAULT_HOST
    if raw == "::":
        # Unbracketed IPv6 bind-any is unparseable as host:port.
        return DEFAULT_HOST
    if re.search(r"\s", raw):
        raise bad()
    candidate = raw if "://" in raw else f"http://{raw}"
    try:
        u = urlsplit(candidate)
        host, port = u.hostname, u.port
    except ValueError as e:
        raise bad() from e
    if not u.netloc:
        raise bad()
    if u.scheme not in ("http", "https"):
        raise bad("scheme must be http or https")
    if host is None or host in _ANY_HOSTS:
        host = "127.0.0.1"
    if ":" in host:
        host = f"[{host}]"
    return f"{u.scheme}://{host}:{port or _DEFAULT_PORT}"


def _host(explicit: str | None) -> str:
    """`host` arg, else OLLAMA_HOST (the same env var the Ollama CLI and
    its own SDK honor), else localhost."""
    return normalize_host(explicit or os.environ.get("OLLAMA_HOST"))


# Generation streams can run for minutes; only the connect is bounded.
_TIMEOUT = httpx.Timeout(None, connect=5.0)


class OllamaError(RuntimeError):
    """Raised for unreachable/denied/erroring Ollama — the message is
    written for app UIs, not logs."""


@dataclass
class Message:
    role: str  # "system" | "user" | "assistant" | "tool"
    content: str


@dataclass
class OllamaStatus:
    installed: bool  # `ollama` binary on PATH
    running: bool  # API responded
    version: str | None = None
    models: list[str] = field(default_factory=list)
    error: str | None = None


def _err(e: Exception) -> OllamaError:
    # A closed loopback port surfaces as ConnectError on Unix but can be a
    # ConnectTimeout on Windows (with an empty message) — same meaning.
    if isinstance(e, (httpx.ConnectError, httpx.ConnectTimeout)):
        return OllamaError(
            "Ollama is not running — start it with `ollama serve` "
            "(or install it from https://ollama.com)"
        )
    if isinstance(e, httpx.HTTPStatusError):
        try:
            detail = e.response.json().get("error", e.response.text)
        except (json.JSONDecodeError, ValueError, AttributeError):
            detail = e.response.text
        return OllamaError(f"Ollama error {e.response.status_code}: {detail}")
    return OllamaError(f"Ollama request failed: {e or type(e).__name__}")


async def status(host: str | None = None, timeout: float = 2.0) -> OllamaStatus:
    """Installed? Running? Version, models — for app UIs and `pylair doctor`
    parity. Never raises; failures land in `error`."""
    s = OllamaStatus(installed=shutil.which("ollama") is not None, running=False)
    try:
        async with httpx.AsyncClient(base_url=_host(host), timeout=timeout) as c:
            v = await c.get("/api/version")
            v.raise_for_status()
            s.running = True
            s.version = v.json().get("version")
            t = await c.get("/api/tags")
            t.raise_for_status()
            s.models = [m["name"] for m in t.json().get("models", [])]
    except (httpx.HTTPError, OllamaError) as e:
        s.error = str(_err(e)) if isinstance(e, httpx.HTTPError) else str(e)
        if not s.running and s.installed:
            s.error += " (binary found on PATH but API is down)"
    return s


async def chat(
    model: str,
    messages: list[Message],
    *,
    host: str | None = None,
    options: dict | None = None,
) -> AsyncIterator[str]:
    """Yield assistant content deltas for a chat completion. Cancelling the
    consuming task aborts the HTTP request mid-stream."""
    payload: dict = {
        "model": model,
        "messages": [{"role": m.role, "content": m.content} for m in messages],
        "stream": True,
    }
    if options:
        payload["options"] = options
    try:
        async with (
            httpx.AsyncClient(base_url=_host(host), timeout=_TIMEOUT) as c,
            c.stream("POST", "/api/chat", json=payload) as r,
        ):
            r.raise_for_status()
            async for line in r.aiter_lines():
                if not line:
                    continue
                chunk = json.loads(line)
                if err := chunk.get("error"):
                    raise OllamaError(err)
                if delta := chunk.get("message", {}).get("content"):
                    yield delta
                if chunk.get("done"):
                    return
    except OllamaError:
        raise
    except httpx.HTTPError as e:
        raise _err(e) from e


async def pull(model: str, *, host: str | None = None) -> AsyncIterator[dict]:
    """Yield Ollama pull progress events ({status, completed, total}).
    For 'download the model on first run' UX."""
    try:
        async with (
            httpx.AsyncClient(base_url=_host(host), timeout=_TIMEOUT) as c,
            c.stream("POST", "/api/pull", json={"model": model, "stream": True}) as r,
        ):
            r.raise_for_status()
            async for line in r.aiter_lines():
                if not line:
                    continue
                ev = json.loads(line)
                if err := ev.get("error"):
                    raise OllamaError(err)
                yield ev
    except OllamaError:
        raise
    except httpx.HTTPError as e:
        raise _err(e) from e
