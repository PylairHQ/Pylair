"""Shared harness: drive a real `python -m pylair` subprocess over frames."""

from __future__ import annotations

import json
import os
import queue
import struct
import subprocess
import sys
import threading
from pathlib import Path

import pytest

HERE = Path(__file__).parent
_LEN = struct.Struct(">I")


class Runtime:
    """A spawned pylair runtime with frame-level access."""

    def __init__(self, module: str = "testapp", env: dict | None = None):
        env = dict(os.environ) | (env or {})
        env.pop("PYTHONPATH", None)
        # A SIGABRT to a wedged runtime dumps every thread's stack to its
        # stderr — the flake-hunt diagnostics in test_warm._hang_dump rely
        # on it. No effect on normal operation.
        env["PYTHONFAULTHANDLER"] = "1"
        self.proc = subprocess.Popen(
            # _diag_run.py wraps `python -m pylair` (runpy, alter_sys —
            # identical argv/env semantics) to add a SIGUSR1 coroutine-task
            # dump for the warm-suite flake hunt.
            [sys.executable, str(HERE / "_diag_run.py"), module],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            cwd=HERE,
            env=env,
        )
        self._q: queue.Queue = queue.Queue()
        self.stderr_lines: list[str] = []
        self.io_error: str | None = None
        self._stdout_t = threading.Thread(target=self._drain_stdout, daemon=True)
        self._stdout_t.start()
        self._stderr_t = threading.Thread(target=self._drain_stderr, daemon=True)
        self._stderr_t.start()

    def _drain_stdout(self) -> None:
        try:
            while True:
                hdr = self.proc.stdout.read(4)
                if len(hdr) < 4:
                    return
                (n,) = _LEN.unpack(hdr)
                body = self.proc.stdout.read(n)
                self._q.put(json.loads(body))
        except (OSError, ValueError) as e:
            # The reader thread is now dead — every later frame is lost.
            # Surface it on the next recv instead of starving the wait.
            self.io_error = str(e)
            self._q.put({"__io_error__": str(e)})

    def _drain_stderr(self) -> None:
        for line in self.proc.stderr:
            self.stderr_lines.append(line.decode(errors="replace"))

    def send(self, msg: dict) -> None:
        body = json.dumps(msg).encode()
        self.proc.stdin.write(_LEN.pack(len(body)) + body)
        self.proc.stdin.flush()

    def send_raw(self, data: bytes) -> None:
        self.proc.stdin.write(data)
        self.proc.stdin.flush()

    def recv(self, timeout: float = 10.0) -> dict:
        msg = self._q.get(timeout=timeout)
        if "__io_error__" in msg:
            raise RuntimeError(f"runtime stdout reader died: {msg['__io_error__']}")
        return msg

    def recv_until(self, pred, timeout: float = 10.0) -> dict:
        import time

        deadline = time.time() + timeout
        while time.time() < deadline:
            msg = self.recv(timeout=max(0.1, deadline - time.time()))
            if pred(msg):
                return msg
        raise TimeoutError("predicate not satisfied")

    def close_stdin(self) -> None:
        self.proc.stdin.close()

    def kill(self) -> None:
        self.proc.kill()

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        if self.proc.poll() is None:
            self.proc.kill()
            self.proc.wait(timeout=5)
        return False


@pytest.fixture
def runtime():
    with Runtime() as r:
        yield r
