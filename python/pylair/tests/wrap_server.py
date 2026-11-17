"""A configurable web server for the wrap launcher tests (and the Rust
supervisor's wrap tests): one file, many ways to misbehave.

    python wrap_server.py PORT [options]

--first-n N --first-delay S   the first N requests take S seconds to answer
                              (a slow hydration)
--hang-after N                after N answered requests, accept and never
                              answer (a wedged server)
--noise                       write junk to fd 1 at start and on every request,
                              including bytes that look like a frame header
--grandchild PIDFILE          spawn a grandchild (SIGTERM-ignoring on POSIX);
                              it writes its pid to PIDFILE
--ignore-term                 ignore SIGTERM (needs the SIGKILL escalation)
--exit-after S --exit-code C  exit by itself after S seconds
--never-listen                never bind (a server that never becomes ready)
--pidfile PATH                write this server's own pid
--fail-starts N --state-file F  the first N starts exit 98 at once (a lost
                              bind race); F counts starts across restarts
"""

from __future__ import annotations

import argparse
import os
import signal
import socket
import socketserver
import subprocess
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

GRANDCHILD = """
import os, signal, sys, time
if os.name != "nt":
    signal.signal(signal.SIGTERM, signal.SIG_IGN)
with open(sys.argv[1], "w") as f:
    f.write(str(os.getpid()))
time.sleep(3600)
"""


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("port", type=int)
    ap.add_argument("--first-n", type=int, default=0)
    ap.add_argument("--first-delay", type=float, default=0.0)
    ap.add_argument("--hang-after", type=int, default=-1)
    ap.add_argument("--noise", action="store_true")
    ap.add_argument("--grandchild")
    ap.add_argument("--ignore-term", action="store_true")
    ap.add_argument("--exit-after", type=float, default=-1)
    ap.add_argument("--exit-code", type=int, default=7)
    ap.add_argument("--never-listen", action="store_true")
    ap.add_argument("--pidfile")
    ap.add_argument("--fail-starts", type=int, default=0)
    ap.add_argument("--state-file")
    args = ap.parse_args()

    if args.fail_starts:
        starts = 0
        try:
            with open(args.state_file) as f:
                starts = int(f.read() or 0)
        except (OSError, ValueError):
            pass
        with open(args.state_file, "w") as f:
            f.write(str(starts + 1))
        if starts < args.fail_starts:
            print(
                "server: address already in use (injected)", file=sys.stderr, flush=True
            )
            os._exit(98)
    if args.pidfile:
        with open(args.pidfile, "w") as f:
            f.write(str(os.getpid()))
    if args.ignore_term:
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
    if args.noise:
        print("server: stdout noise at startup", flush=True)
        os.write(1, b"\x00\x00\x00\x05hello")
    if args.grandchild:
        subprocess.Popen([sys.executable, "-c", GRANDCHILD, args.grandchild])
    if args.exit_after >= 0:

        def bye() -> None:
            time.sleep(args.exit_after)
            os._exit(args.exit_code)

        threading.Thread(target=bye, daemon=True).start()
    if args.never_listen:
        time.sleep(3600)
        return

    seen = 0
    lock = threading.Lock()

    class H(BaseHTTPRequestHandler):
        def do_GET(self) -> None:
            nonlocal seen
            with lock:
                seen += 1
                n = seen
            if args.noise:
                os.write(1, b"\x00\x00\x00\x02{}")
                print(f"server: request {n}", flush=True)
            if 0 <= args.hang_after < n:
                time.sleep(3600)
            if n <= args.first_n:
                time.sleep(args.first_delay)
            body = b"pylair-wrap-test-marker"
            self.send_response(200)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, *a: object) -> None:
            pass

    t0 = time.monotonic()

    def note(msg: str) -> None:
        print(
            f"server[{time.monotonic() - t0:6.2f}s]: {msg}", file=sys.stderr, flush=True
        )

    note(f"starting on {sys.executable} {sys.version.split()[0]} port {args.port}")

    # Evidence from CI (macOS, python.org 3.12): the server sat between bind()
    # and listen() (lsof: the port CLOSED, connects timing out) and never got
    # past the constructor. http.server.HTTPServer.server_bind() does a
    # socket.getfqdn() lookup there. This fixture does not need a server name,
    # so it skips that lookup, and measures the lookup on a side thread so a
    # slow resolver still shows up in the log without delaying listen().
    class Server(ThreadingHTTPServer):
        def server_bind(self) -> None:
            socketserver.TCPServer.server_bind(self)
            self.server_name = "127.0.0.1"
            self.server_port = self.server_address[1]

    def time_getfqdn() -> None:
        t = time.monotonic()
        socket.getfqdn("127.0.0.1")
        note(f"socket.getfqdn('127.0.0.1') took {time.monotonic() - t:.2f}s")

    threading.Thread(target=time_getfqdn, daemon=True).start()
    srv = Server(("127.0.0.1", args.port), H)
    srv.daemon_threads = True
    note(f"listening on {srv.server_address}")
    srv.serve_forever()


if __name__ == "__main__":
    main()
