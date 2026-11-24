#!/usr/bin/env python3
"""U0 spike update server.

Serves dist/ over http://127.0.0.1:8377 and generates Tauri updater
manifests on the fly:

  GET /latest.json          manifest for the version named in dist/CHANNEL
  GET /rollback-<ver>.json  manifest for dist/<ver>/ (the "old" artifact)
  GET /<ver>/<file>         the signed artifacts themselves

Manifests carry the stock fields (version/notes/pub_date/platforms
{target: {signature, url}}) plus a `sha256` per platform — the artifact
identity the skip list keys on (protocol rule: unknown fields ignored).
"""

import hashlib
import json
import sys
import time
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path
from urllib.parse import urlparse

ROOT = Path(__file__).parent / "dist"
PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 8377

PLATFORM_PATTERNS = [
    (".AppImage", "linux-x86_64"),
    (".app.tar.gz", "darwin"),
    ("-setup.exe", "windows-x86_64"),
]


def sha256(p: Path) -> str:
    h = hashlib.sha256()
    h.update(p.read_bytes())
    return h.hexdigest()


def platforms_for(ver: str, host: str) -> dict:
    out = {}
    d = ROOT / ver
    if not d.is_dir():
        return out
    for f in sorted(d.iterdir()):
        if f.suffix == ".sig":
            continue
        sig = f.with_name(f.name + ".sig")
        for pat, target in PLATFORM_PATTERNS:
            if f.name.endswith(pat) and sig.exists():
                if target == "darwin":
                    arch = (ROOT / "ARCH").read_text().strip() if (ROOT / "ARCH").exists() else "aarch64"
                    key = f"darwin-{arch}"
                else:
                    key = target
                out[key] = {
                    "signature": sig.read_text().strip(),
                    "sha256": sha256(f),
                    "url": f"http://{host}/{ver}/{f.name}",
                }
                break
    return out


def manifest(ver: str, host: str) -> dict:
    return {
        "version": ver,
        "notes": f"u0 spike {ver}",
        "pub_date": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "platforms": platforms_for(ver, host),
    }


class H(BaseHTTPRequestHandler):
    def log_message(self, fmt, *a):
        sys.stderr.write("[serve] " + fmt % a + "\n")

    def _json(self, obj):
        body = json.dumps(obj).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _file(self, p: Path):
        self.send_response(200)
        self.send_header("Content-Length", str(p.stat().st_size))
        self.end_headers()
        self.wfile.write(p.read_bytes())

    def do_GET(self):
        host = self.headers.get("Host", f"127.0.0.1:{PORT}")
        path = urlparse(self.path).path.lstrip("/")
        if path == "latest.json":
            ver = (ROOT / "CHANNEL").read_text().strip()
            return self._json(manifest(ver, host))
        if path.startswith("rollback-") and path.endswith(".json"):
            return self._json(manifest(path[len("rollback-"): -len(".json")], host))
        f = ROOT / path
        if f.is_file() and f.parent != ROOT.parent:  # stay inside dist/
            return self._file(f)
        self.send_response(404)
        self.end_headers()


if __name__ == "__main__":
    HTTPServer(("127.0.0.1", PORT), H).serve_forever()
