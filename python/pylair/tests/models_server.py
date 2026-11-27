"""Loopback HTTP fixture for pylair.models tests — Range-capable,
fault-injecting, request-recording. No external network: every test
serves from 127.0.0.1 on an ephemeral port.

Faults are deterministic switches on the File spec, never sleeps:
`drop_after` lies about Content-Length then closes early, `gate` holds
a response until the test opens it, `ignore_range`/`if_range_stale`
force restart paths, `status_queue` replays 5xx/429 before serving.
"""

from __future__ import annotations

import hashlib
import http.server
import json
import socket
import threading
import time
import urllib.parse
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
    throttle: int = 0  # bytes per write burst (fragment the stream)
    burst_delay: float = 0.0  # seconds between bursts (actually slow)


@dataclass
class Repo:
    """A Hugging Face repo emulation for snapshot tests: the tree API
    (optionally paginated through the Link header) plus resolve files.
    `lfs` names the paths served as LFS kind — everything else is a
    small git-blob file. `dirs` adds directory entries to the tree,
    which clients must skip."""

    revision: str
    files: dict[str, bytes]
    lfs: set[str] = field(default_factory=set)
    page_size: int | None = None
    dirs: bool = True

    def entries(self) -> list[dict]:
        out = []
        for path in sorted(self.files):
            body = self.files[path]
            ent = {
                "type": "file",
                "oid": hashlib.sha1(b"blob %d\0" % len(body) + body).hexdigest(),
                "size": len(body),
                "path": path,
            }
            if path in self.lfs:
                ent["lfs"] = {
                    "oid": hashlib.sha256(body).hexdigest(),
                    "size": len(body),
                    "pointerSize": 131,
                }
            out.append(ent)
        if self.dirs:
            seen = set()
            for path in sorted(self.files):
                parts = path.split("/")[:-1]
                for i in range(1, len(parts) + 1):
                    d = "/".join(parts[:i])
                    if d not in seen:
                        seen.add(d)
                        out.append(
                            {"type": "directory", "oid": "0" * 40, "size": 0, "path": d}
                        )
        return sorted(out, key=lambda e: e["path"])


class _Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):  # quiet
        pass

    def _tree(self, srv, split):
        """Match /api/models/<repo>/tree/<revision> — repo may itself
        contain a slash (org/model). Returns (entries, page_size,
        base_qs) or None when the path is not a tree request. Unknown
        repo/revision pairs answer 404 like the real API."""
        parts = split.path.split("/")
        # ['', 'api', 'models', org..., name, 'tree', rev]
        if len(parts) < 6 or parts[1] != "api" or parts[2] != "models":
            return None
        if "tree" not in parts:
            return None
        i = parts.index("tree")
        repo = "/".join(parts[3:i])
        revision = parts[i + 1] if i + 1 < len(parts) else ""
        key = (repo, revision)
        if key not in srv.repos:
            return None  # falls through to the generic 404
        r = srv.repos[key]
        return r.entries(), r.page_size, "recursive=true"

    def do_GET(self):
        srv: ModelServer = self.server  # type: ignore[assignment]
        srv.requests.append({"path": self.path, "headers": dict(self.headers)})
        split = urllib.parse.urlsplit(self.path)
        tree = self._tree(srv, split)
        if tree is not None:
            entries, page_size, base_qs = tree
            qs = urllib.parse.parse_qs(split.query)
            cursor = int(qs.get("cursor", ["0"])[0])
            page = entries[cursor : cursor + page_size] if page_size else entries
            payload = json.dumps(page).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(payload)))
            if page_size and cursor + page_size < len(entries):
                nxt = f"{srv.base}{split.path}?{base_qs}&cursor={cursor + page_size}"
                self.send_header("Link", f'<{nxt}>; rel="next"')
            self.end_headers()
            self.wfile.write(payload)
            return
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
            # wfile is unbuffered (wbufsize=0): one write is one send()
            # which may return short — sendall can't truncate a burst.
            self.connection.sendall(body[i : i + step])
            if f.burst_delay:
                time.sleep(f.burst_delay)


class ModelServer(http.server.ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self) -> None:
        super().__init__(("127.0.0.1", 0), _Handler)
        self.files: dict[str, File] = {}
        self.redirects: dict[str, str] = {}
        self.repos: dict[tuple[str, str], Repo] = {}
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

    def add_repo(self, repo: str, r: Repo) -> None:
        """Register the tree plus one File per repo path at the resolve
        URL shape /<repo>/resolve/<revision>/<path>, so the Range /
        fault machinery applies to snapshot downloads unchanged."""
        self.repos[(repo, r.revision)] = r
        for path, body in r.files.items():
            self.files[f"/{repo}/resolve/{r.revision}/{path}"] = File(body)

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
