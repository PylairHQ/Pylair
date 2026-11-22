"""Loopback HTTP fixture for pylair.models tests — Range-capable,
fault-injecting, request-recording. No external network: every test
serves from 127.0.0.1 on an ephemeral port.

Faults are deterministic switches on the File spec, never sleeps:
`drop_after` lies about Content-Length then closes early, `gate` holds
a response until the test opens it, `ignore_range`/`if_range_stale`
force restart paths, `status_queue` replays 5xx/429 before serving.
"""

from __future__ import annotations

import http.server
import socket
import threading
from dataclasses import dataclass, field


@dataclass
class File:
    body: bytes
    etag: str | None = '"v1"'
    lastmod: str | None = None
    # --- fault knobs -------------------------------------------------
    drop_after: int | None = None  # advertise full length, close early
    ignore_range: bool = False  # answer Range with 200 + whole body
    if_range_stale: bool = False  # treat any If-Range as stale -> 200
    status_queue: list[int] = field(default_factory=list)  # codes first
    gate: threading.Event | None = None  # hold the response until set
    mid_gate: threading.Event | None = None  # hold between write bursts
    throttle: int = 0  # bytes per write burst (slow the stream)


class _Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):  # quiet
        pass

    def do_GET(self):
        srv: ModelServer = self.server  # type: ignore[assignment]
        srv.requests.append({"path": self.path, "headers": dict(self.headers)})
        if self.path in srv.redirects:
            self.send_response(302)
            self.send_header("Location", srv.redirects[self.path])
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        f = srv.files.get(self.path)
        if f is None:
            self.send_error(404)
            self.end_headers()
            return
        with srv.lock:
            if f.status_queue:
                code = f.status_queue.pop(0)
            else:
                code = None
        if code is not None:
            self.send_response(code)
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        if f.gate is not None:
            f.gate.wait(timeout=30)
        body = f.body
        rng = self.headers.get("Range")
        start = 0
        if rng:
            try:
                start = int(rng.split("=")[1].split("-")[0])
            except (IndexError, ValueError):
                start = 0
            ir = self.headers.get("If-Range")
            if (
                ir is not None
                and (f.if_range_stale or ir not in (f.etag, f.lastmod))
                or f.ignore_range
            ):
                rng = None
                start = 0
            elif start >= len(body):
                self.send_response(416)
                self.send_header("Content-Length", "0")
                self.end_headers()
                return
        if rng and start > 0:
            self.send_response(206)
            self.send_header(
                "Content-Range", f"bytes {start}-{len(f.body) - 1}/{len(f.body)}"
            )
            body = body[start:]
        else:
            self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        if f.etag:
            self.send_header("ETag", f.etag)
        if f.lastmod:
            self.send_header("Last-Modified", f.lastmod)
        self.end_headers()
        if f.drop_after is not None:
            # Content-Length advertised the full body; the connection
            # dies after drop_after bytes — a truncated transfer. One
            # shot: the retried request serves clean.
            n = f.drop_after
            f.drop_after = None
            self.connection.sendall(body[:n])
            self.close_connection = True
            return
        step = f.throttle or max(len(body), 1)
        for i in range(0, len(body), step):
            if i and f.mid_gate is not None:
                f.mid_gate.wait(timeout=30)
            # wfile is unbuffered (wbufsize=0): one write is one send()
            # which may return short — sendall can't truncate a burst.
            self.connection.sendall(body[i : i + step])


class ModelServer(http.server.ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self) -> None:
        super().__init__(("127.0.0.1", 0), _Handler)
        self.files: dict[str, File] = {}
        self.redirects: dict[str, str] = {}
        self.requests: list[dict] = []
        self.lock = threading.Lock()
        self._t = threading.Thread(target=self.serve_forever, daemon=True)
        self._t.start()

    def probe(self) -> bool:
        """Self-connect once — Windows can hand a freshly-bound
        ephemeral port that RSTs new SYNs (lingering 4-tuple state
        under heavy loopback churn). A refused probe means rebind."""
        try:
            s = socket.create_connection(self.server_address, timeout=2)
        except OSError:
            return False
        s.close()
        return True

    @property
    def base(self) -> str:
        return f"http://127.0.0.1:{self.server_port}"

    def add(self, path: str, f: File) -> str:
        self.files[path] = f
        return self.base + path

    def hits(self, path: str) -> int:
        return sum(1 for r in self.requests if r["path"] == path)

    def last_headers(self, path: str) -> dict:
        for r in reversed(self.requests):
            if r["path"] == path:
                return {k.lower(): v for k, v in r["headers"].items()}
        return {}

    def close(self) -> None:
        self.shutdown()
        self.server_close()
