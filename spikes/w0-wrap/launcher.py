#!/usr/bin/env python3
"""W0 hand-wired wrap launcher — stands in for `python -m pylair.wrap`.

Speaks the supervisor frame protocol over stdio (hello immediately, then
a tagged stderr line `READY <url>` once the HTTP probe passes — protocol
`ready` is W1; this spike needs no supervisor changes).

Usage: launcher.py -- cmd...   ({port} in cmd is substituted; PYLAIR_PORT
is also exported for the child)
"""

import json
import os
import queue
import signal
import socket
import struct
import subprocess
import sys
import threading
import time
import urllib.request

READY_TIMEOUT = 120.0

# Private frame fd: save the real stdout BEFORE redirecting fd 1 to
# stderr, exactly like the production runtime does.
FRAME_FD = os.dup(1)
_frames = os.fdopen(FRAME_FD, "wb")


def send(obj):
    b = json.dumps(obj).encode()
    _frames.write(struct.pack(">I", len(b)) + b)
    _frames.flush()


def note(line):  # stderr is drained and forwarded by the supervisor
    sys.stderr.write(line + "\n")
    sys.stderr.flush()


def append_file(env_var, line):
    """Automation hook: write `line` to the file named by env_var, if set.
    The headless driver can't see a GUI app's stderr on Windows, so the
    READY url and the child pids go to files instead."""
    path = os.environ.get(env_var)
    if path:
        with open(path, "a") as fh:
            fh.write(line + "\n")


def free_port():
    for _ in range(5):
        s = socket.socket()
        try:
            s.bind(("127.0.0.1", 0))
            return s.getsockname()[1]
        except OSError:
            continue
        finally:
            s.close()
    raise RuntimeError("no free loopback port")


def wait_ready(url, proc, timeout=READY_TIMEOUT):
    deadline = time.time() + timeout
    while time.time() < deadline:
        if proc.poll() is not None:
            return False
        try:
            code = urllib.request.urlopen(url, timeout=1).status
            if code < 500:
                return True
        except Exception:
            pass
        time.sleep(0.2)
    return False


HEALTH_EVERY = 5.0  # seconds between probes while serving
HEALTH_MISS_MAX = 3  # consecutive failures before we declare it stuck


def probe_health(url):
    """A wedged server still answers protocol pings (the LAUNCHER pongs,
    not the server) — only an HTTP probe catches a stuck-but-alive app.
    This is the W1 rationale, proven here in the spike."""
    try:
        return urllib.request.urlopen(url + "/", timeout=2).status < 500
    except Exception:
        return False


def main():
    argv = sys.argv[1:]
    if argv and argv[0] == "--":
        argv = argv[1:]
    if not argv:
        note("launcher: no command")
        sys.exit(2)

    # fd-1 hygiene: user/server stdout noise goes to stderr, never fd 1.
    # stdin reads keep using sys.stdin.buffer (fd 0 is untouched).
    os.dup2(2, 1)
    send({"jsonrpc": "2.0", "method": "hello", "params": {"protocol": 1, "exposes": [], "kind": "wrap"}})

    port = free_port()
    cmd = [a.replace("{port}", str(port)) for a in argv]
    env = {**os.environ, "PYLAIR_PORT": str(port)}
    if os.name == "posix":
        # No setsid: the server joins the LAUNCHER's process group — the one
        # the supervisor created and killpgs. setsid would give the server
        # its own group that a supervisor-side killpg can't reach.
        proc = subprocess.Popen(cmd, env=env, stderr=None)
    else:
        proc = subprocess.Popen(cmd, env=env, creationflags=subprocess.CREATE_NEW_PROCESS_GROUP)
    url = f"http://127.0.0.1:{port}"
    note(f"launcher: spawning {' '.join(cmd)} on {url}")
    append_file("W0_PID_FILE", f"launcher {os.getpid()}")
    append_file("W0_PID_FILE", f"server {proc.pid}")

    ready = wait_ready(url + "/", proc)
    note(("READY " if ready else "READY-FAIL ") + url)
    if ready:
        append_file("W0_READY_FILE", url)
    if not ready:
        kill_tree(proc)
        sys.exit(3)

    # stdin reader thread + 1s tick: lets us poll health and child liveness
    # while still responding to frames. (queue.Empty == tick.)
    msgs = queue.Queue()

    def reader():
        while True:
            h = sys.stdin.buffer.read(4)
            if len(h) < 4:
                msgs.put(None)
                return
            (n,) = struct.unpack(">I", h)
            msgs.put(json.loads(sys.stdin.buffer.read(n)))

    threading.Thread(target=reader, daemon=True).start()

    misses = 0
    last_probe = 0.0
    while True:
        try:
            m = msgs.get(timeout=1.0)
        except queue.Empty:
            if proc.poll() is not None:
                note("launcher: server exited")
                kill_tree(proc)
                os._exit(1)  # daemon reader is live — sys.exit can deadlock
            if time.time() - last_probe >= HEALTH_EVERY:
                last_probe = time.time()
                if probe_health(url):
                    misses = 0
                else:
                    misses += 1
                    note(f"launcher: health probe failed ({misses}/{HEALTH_MISS_MAX})")
                    if misses >= HEALTH_MISS_MAX:
                        note("launcher: server stuck — exiting for supervisor restart")
                        kill_tree(proc)
                        # os._exit: the daemon stdin reader holds the buffer
                        # lock at shutdown — sys.exit can deadlock there.
                        os._exit(5)
            continue
        if m is None:
            break  # stdin EOF — clean shutdown
        if m.get("method") == "ping":
            send({"jsonrpc": "2.0", "method": "pong", "params": {"seq": m["params"]["seq"]}})
        elif m.get("method") == "shutdown":
            send({"jsonrpc": "2.0", "id": m["id"], "result": None})
            break
    kill_tree(proc)
    os._exit(0)


def _group_members():
    """Pids in our own process group, excluding us — server + grandchildren
    all share the launcher pgid by construction."""
    try:
        out = subprocess.run(
            ["pgrep", "-g", str(os.getpgrp())], capture_output=True, text=True
        ).stdout
        return [int(p) for p in out.split() if int(p) != os.getpid()]
    except Exception:
        return []


def kill_tree(proc):
    """SIGTERM every group member (server + grandchildren), then SIGKILL any
    survivors after grace — a leader exiting on SIGTERM must not shield a
    grandchild that ignores it (the M1 bug shape)."""
    try:
        if os.name == "posix":
            for pid in _group_members():
                try:
                    os.kill(pid, signal.SIGTERM)
                except ProcessLookupError:
                    pass
            deadline = time.time() + 3
            while time.time() < deadline:
                if not _group_members():
                    return
                time.sleep(0.05)
            for pid in _group_members():
                try:
                    os.kill(pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
        else:
            # taskkill /T kills the tree; proc.terminate() is leader-only.
            subprocess.run(
                ["taskkill", "/F", "/T", "/PID", str(proc.pid)],
                capture_output=True,
            )
    except (ProcessLookupError, PermissionError):
        # Gone, or the pgid was recycled after the group died — either way
        # there is nothing left we can signal.
        pass


if __name__ == "__main__":
    main()
