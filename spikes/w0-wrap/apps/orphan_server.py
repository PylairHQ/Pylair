"""W0 fixture: a server that dies on SIGTERM but leaves a grandchild that
ignores it — the exact M1 grandchild-leak shape, for the kill-tree row."""

import os
import signal
import subprocess
import sys
import tempfile
from http.server import BaseHTTPRequestHandler, HTTPServer

MARKER = "W0-WRAP-MARKER"
ORPHAN_FILE = os.environ.get(
    "W0_ORPHAN_FILE", os.path.join(tempfile.gettempdir(), "w0-orphan.txt")
)


def spawn_orphan():
    # Grandchild: ignores SIGTERM (no-op on Windows — TerminateProcess
    # cannot be ignored, which is exactly what a Job Object uses), lives
    # forever, writes a heartbeat file whose first line names its pid so
    # the test can check the same process is gone, not just stopped.
    code = (
        "import signal,time,os\n"
        "signal.signal(signal.SIGTERM, signal.SIG_IGN)\n"
        f"f=open({ORPHAN_FILE!r},'w')\n"
        "f.write(f'pid={os.getpid()}\\n'); f.flush()\n"
        "while True: f.write('x'); f.flush(); time.sleep(0.5)\n"
    )
    subprocess.Popen([sys.executable, "-c", code])


class H(BaseHTTPRequestHandler):
    def do_GET(self):
        b = MARKER.encode()
        self.send_response(200)
        self.send_header("Content-Length", str(len(b)))
        self.end_headers()
        self.wfile.write(b)

    def log_message(self, *a):
        pass


if __name__ == "__main__":
    # Parent exits on SIGTERM (default disposition) — the orphan must not.
    signal.signal(signal.SIGTERM, signal.SIG_DFL)
    spawn_orphan()
    HTTPServer(("127.0.0.1", int(os.environ["PYLAIR_PORT"])), H).serve_forever()
