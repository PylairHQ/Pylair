"""Wrap launcher (`python -m pylair.wrap`): wire behaviour, fault injection.

Unit tests cover argument parsing, port selection and command building.
The rest drive a real launcher subprocess over frames, with
`wrap_server.py` as the misbehaving web server.
"""

from __future__ import annotations

import json
import os
import queue
import socket
import struct
import subprocess
import sys
import threading
import time
from pathlib import Path
from typing import Self

import pytest

from pylair import _protocol as proto
from pylair.wrap import _launcher as L

HERE = Path(__file__).parent
SERVER = str(HERE / "wrap_server.py")
_LEN = struct.Struct(">I")
WAIT = 30.0  # generous: CI runners and the Rust suite load the machine

# ---- unit: arguments, command, port -------------------------------------------


def test_parse_args_cmd_then_options_and_options_then_cmd():
    a = L.parse_args(["--cmd", "python", "-m", "app", "--port", "{port}"])
    assert a.cmd == ["python", "-m", "app", "--port", "{port}"]
    assert a.ready_path == "/"

    # The documented shape: launcher options trail the command.
    b = L.parse_args(["--cmd", "srv", "--flag", "x", "--ready-path", "/health"])
    assert b.cmd == ["srv", "--flag", "x"]
    assert b.ready_path == "/health"

    # ...and leading options work too, including `--opt=value`.
    c = L.parse_args(["--probe-interval=0.5", "--max-misses", "2", "--cmd", "srv"])
    assert (c.cmd, c.probe_interval, c.max_misses) == (["srv"], 0.5, 2)

    d = L.parse_args(["--cmd", "srv", "--probe-timeout=1.5"])
    assert (d.cmd, d.probe_timeout) == (["srv"], 1.5)


@pytest.mark.parametrize(
    "argv",
    [
        [],
        ["--ready-path", "/"],
        ["--cmd"],
        ["--cmd", "--ready-path", "/"],
        ["--bogus", "1", "--cmd", "srv"],
        ["--cmd", "srv", "--ready-path", "nope"],
        ["--cmd", "srv", "--max-misses", "0"],
        ["--cmd", "srv", "--probe-interval", "abc"],
    ],
)
def test_parse_args_rejects_bad_usage(argv):
    with pytest.raises(L.UsageError):
        L.parse_args(argv)


def test_build_command_substitutes_port_and_resolves_python():
    cmd = L.build_command(["python", "-m", "app", "--port", "{port}", "x{port}"], 4321)
    assert cmd == [sys.executable, "-m", "app", "--port", "4321", "x4321"]
    # Only an interpreter name is rewritten, never an arbitrary program.
    assert L.build_command(["uvicorn", "{port}"], 7)[0] == "uvicorn"


class _FlakySocket:
    """Bind fails for the first `fail` sockets, then behaves like a real one."""

    made = 0

    def __init__(self, fail: int):
        self._fail = fail
        self._real = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        _FlakySocket.made += 1
        self._n = _FlakySocket.made

    def bind(self, addr):
        if self._n <= self._fail:
            raise OSError("address in use (injected)")
        self._real.bind(addr)

    def getsockname(self):
        return self._real.getsockname()

    def close(self):
        self._real.close()


def test_pick_free_port_retries_failed_binds():
    _FlakySocket.made = 0
    port = L.pick_free_port(make_socket=lambda *a: _FlakySocket(4))
    assert 0 < port < 65536
    assert _FlakySocket.made == 5  # four failures, the fifth attempt wins


def test_pick_free_port_gives_up_after_five_attempts():
    _FlakySocket.made = 0
    with pytest.raises(OSError, match="5 attempts"):
        L.pick_free_port(make_socket=lambda *a: _FlakySocket(99))
    assert _FlakySocket.made == 5


def test_pick_free_port_is_loopback_and_free():
    port = L.pick_free_port()
    s = socket.socket()
    try:
        s.bind(("127.0.0.1", port))  # nothing holds it after the pick
    finally:
        s.close()


# ---- harness: a real launcher over frames -----------------------------------


def pid_alive(pid: int) -> bool:
    if sys.platform == "win32":
        import ctypes

        k32 = ctypes.windll.kernel32
        k32.OpenProcess.restype = ctypes.c_void_p
        h = k32.OpenProcess(0x1000, 0, pid)  # PROCESS_QUERY_LIMITED_INFORMATION
        if not h:
            return False
        code = ctypes.c_ulong()
        k32.GetExitCodeProcess(ctypes.c_void_p(h), ctypes.byref(code))
        k32.CloseHandle(ctypes.c_void_p(h))
        return code.value == 259  # STILL_ACTIVE
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    # A zombie is dead; it is only waiting to be reaped.
    out = subprocess.run(
        check=False,
        args=["ps", "-o", "stat=", "-p", str(pid)],
        capture_output=True,
        text=True,
    ).stdout.strip()
    return bool(out) and not out.startswith("Z")


class Wrap:
    """A spawned `python -m pylair.wrap` with frame-level access."""

    def __init__(self, server_args: list[str], *opts: str, leader: bool = True):
        env = dict(os.environ)
        env.pop("PYTHONPATH", None)
        cmd = [
            sys.executable,
            "-m",
            "pylair.wrap",
            *opts,
            "--cmd",
            "python",
            SERVER,
            "{port}",
            *server_args,
        ]
        kwargs: dict = {}
        if os.name == "posix" and leader:
            # What the supervisor does: the launcher leads its own group.
            kwargs["start_new_session"] = True
        elif os.name == "nt":
            kwargs["creationflags"] = subprocess.CREATE_NEW_PROCESS_GROUP
        self.proc = subprocess.Popen(
            cmd,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            cwd=HERE,
            env=env,
            **kwargs,
        )
        self.frames: queue.Queue = queue.Queue()
        self.stderr: list[str] = []
        self.stray_stdout = False
        threading.Thread(target=self._frames, daemon=True).start()
        threading.Thread(target=self._err, daemon=True).start()

    def _frames(self) -> None:
        try:
            while True:
                hdr = self.proc.stdout.read(4)
                if len(hdr) < 4:
                    return
                (n,) = _LEN.unpack(hdr)
                body = self.proc.stdout.read(n)
                try:
                    self.frames.put(json.loads(body))
                except ValueError:
                    self.stray_stdout = True  # a corrupted frame channel
                    return
        except (OSError, ValueError):
            return

    def _err(self) -> None:
        for line in self.proc.stderr:
            self.stderr.append(line.decode(errors="replace"))

    def send(self, msg: dict) -> None:
        body = json.dumps(msg).encode()
        self.proc.stdin.write(_LEN.pack(len(body)) + body)
        self.proc.stdin.flush()

    def diag(self) -> str:
        """What the launcher said: its status and stderr, for failure messages."""
        status = self.proc.poll()
        state = "still running" if status is None else f"exited {status}"
        time.sleep(0.2)
        text = "".join(self.stderr) or "(empty)"
        return "launcher " + state + "; stderr:" + chr(10) + text

    def recv(self, timeout: float = WAIT) -> dict:
        try:
            return self.frames.get(timeout=timeout)
        except queue.Empty:
            raise AssertionError(f"no frame within {timeout}s; {self.diag()}") from None

    def recv_method(self, method: str, timeout: float = WAIT) -> dict:
        deadline = time.monotonic() + timeout
        while True:
            m = self.recv(max(0.1, deadline - time.monotonic()))
            if m.get("method") == method:
                return m

    def wait(self, timeout: float = WAIT) -> int:
        try:
            return self.proc.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            raise AssertionError(
                f"launcher did not exit within {timeout}s; {self.diag()}"
            ) from None

    def err_text(self) -> str:
        time.sleep(0.2)
        return "".join(self.stderr)

    def __enter__(self) -> Self:
        return self

    def __exit__(self, *exc) -> bool:
        # End every test the way a dying host would: stdin EOF, and the
        # launcher must take its server tree down itself. Only a launcher
        # that fails to do so is killed, tree and all (a venv's python.exe
        # on Windows is a shim, so killing `proc` alone would orphan the
        # real launcher and its server).
        if self.proc.poll() is None:
            try:
                self.proc.stdin.close()
            except OSError:
                pass
            try:
                self.proc.wait(timeout=20)
            except subprocess.TimeoutExpired:
                self._kill_tree()
        return False

    def _kill_tree(self) -> None:
        if os.name == "nt":
            subprocess.run(
                ["taskkill", "/F", "/T", "/PID", str(self.proc.pid)],
                check=False,
                capture_output=True,
            )
        else:
            self.proc.kill()
        self.proc.wait(timeout=10)


def read_pid(path: Path, timeout: float = WAIT) -> int:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            text = path.read_text().strip()
            if text:
                return int(text)
        except (OSError, ValueError):
            pass
        time.sleep(0.05)
    raise TimeoutError(f"no pid in {path}")


def wait_dead(pid: int, timeout: float = 15.0) -> bool:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if not pid_alive(pid):
            return True
        time.sleep(0.1)
    return not pid_alive(pid)


def get(url: str) -> bytes:
    import urllib.request

    with urllib.request.urlopen(url, timeout=10) as r:
        return r.read()


# ---- wire behaviour ---------------------------------------------------------


def test_hello_first_then_ready_with_loopback_url():
    with Wrap([]) as w:
        hello = w.recv()
        assert hello["method"] == "hello"  # the first frame, immediately
        assert hello["params"]["protocol"] == proto.PROTOCOL_VERSION
        assert hello["params"]["exposes"] == []
        assert hello["params"]["kind"] == "web"
        ready = w.recv_method("ready")
        url = ready["params"]["url"]
        assert url.startswith("http://127.0.0.1:") and url.endswith("/")
        assert get(url) == b"pylair-wrap-test-marker"


def test_pongs_and_unknown_calls_get_answers():
    with Wrap([]) as w:
        w.recv()  # hello
        w.send(proto.request(1, "ping", {"seq": 7}))
        pong = w.recv_method("pong")
        assert pong["params"]["seq"] == 7
        w.send(proto.request(2, "call", {"fn": "x", "task": 1, "input": {}}))
        err = next(m for m in iter(w.recv, None) if m.get("id") == 2)
        assert err["error"]["code"] == proto.FUNCTION_NOT_FOUND
        w.send(proto.request(3, "cancel", {"task": 1}))
        ack = next(m for m in iter(w.recv, None) if m.get("id") == 3)
        assert ack["result"] == {"cancelled": False, "reason": "task_not_found"}


def test_pongs_answered_while_the_server_never_becomes_ready():
    with Wrap(["--never-listen"]) as w:
        w.recv()  # hello
        for seq in (1, 2):
            w.send(proto.request(seq, "ping", {"seq": seq}))
            assert w.recv_method("pong", timeout=10)["params"]["seq"] == seq
        assert not any(m.get("method") == "ready" for m in list(w.frames.queue))
        assert w.proc.poll() is None  # no readiness deadline of its own


def test_stray_stdout_never_reaches_the_frame_channel():
    with Wrap(["--noise"]) as w:
        assert w.recv()["method"] == "hello"
        url = w.recv_method("ready")["params"]["url"]
        for _ in range(3):  # every request writes junk to the server's fd 1
            get(url)
        w.send(proto.request(1, "ping", {"seq": 1}))
        w.recv_method("pong")
        assert not w.stray_stdout, "a non-frame byte reached the frame fd"
        text = w.err_text()
        assert "server: stdout noise at startup" in text  # went to the log
        assert "server: request" in text


def test_a_print_inside_the_launcher_process_goes_to_stderr():
    # Hygiene is the runtime's own: fd 1 points at stderr once hello is out.
    code = (
        "import os,sys;from pylair._transport import open_frame_channel as o;"
        "o();print('stray');os.write(1,b'raw-fd1\\n');sys.stderr.flush()"
    )
    r = subprocess.run(
        check=False,
        args=[sys.executable, "-c", code],
        capture_output=True,
        cwd=HERE,
        timeout=30,
    )
    assert r.stdout == b""
    assert b"stray" in r.stderr and b"raw-fd1" in r.stderr


# ---- readiness and health ----------------------------------------------------


def test_slow_get_during_hydration_is_not_failure_before_ready():
    # First 3 GETs take 1.2s; each probe gives up at 0.4s. The launcher keeps
    # trying (no readiness deadline of its own) and goes ready afterwards.
    with Wrap(
        ["--first-n", "3", "--first-delay", "1.2"],
        "--probe-timeout",
        "0.4",
    ) as w:
        w.recv()
        ready = w.recv_method("ready")
        assert ready["params"]["url"].startswith("http://127.0.0.1:")
        assert w.proc.poll() is None


def test_slow_probes_below_the_miss_limit_do_not_kill_the_server():
    # Readiness is the first GET. Probes 2 and 3 (health) are slow and time
    # out: two consecutive misses with max-misses 3. The fourth answers.
    with Wrap(
        ["--first-n", "3", "--first-delay", "1.0"],
        "--probe-interval",
        "0.2",
        "--probe-timeout",
        "0.3",
        "--max-misses",
        "3",
    ) as w:
        w.recv()
        w.recv_method("ready")
        time.sleep(4.0)
        assert w.proc.poll() is None, w.err_text()


def test_hung_server_exits_with_the_unhealthy_code_after_three_misses():
    with Wrap(
        ["--hang-after", "1"],
        "--probe-interval",
        "0.3",
        "--probe-timeout",
        "0.3",
        "--max-misses",
        "3",
    ) as w:
        w.recv()
        w.recv_method("ready")
        # The launcher still pongs while the server hangs: only the HTTP
        # probe can see this wedge.
        w.send(proto.request(1, "ping", {"seq": 1}))
        w.recv_method("pong")
        assert w.wait() == L.EXIT_UNHEALTHY
        text = w.err_text()
        assert "(3/3)" in text and "(1/3)" in text
        assert "(4/3)" not in text


def test_server_exit_after_ready_makes_the_launcher_exit_nonzero():
    with Wrap(["--exit-after", "0.8", "--exit-code", "0"]) as w:
        w.recv()
        w.recv_method("ready")
        assert w.wait() == L.EXIT_SERVER_EXITED  # even a clean server exit


def test_server_exit_before_ready_makes_the_launcher_exit_nonzero():
    with Wrap(["--never-listen", "--exit-after", "0.2", "--exit-code", "9"]) as w:
        w.recv()
        assert w.wait() == L.EXIT_SERVER_EXITED
        assert not any(m.get("method") == "ready" for m in list(w.frames.queue))


def test_bad_command_exits_with_the_start_failed_code():
    env = dict(os.environ)
    env.pop("PYTHONPATH", None)
    p = subprocess.Popen(
        [sys.executable, "-m", "pylair.wrap", "--cmd", "no-such-program-xyz"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        cwd=HERE,
        env=env,
    )
    try:
        assert p.wait(timeout=WAIT) == L.EXIT_START_FAILED
        assert b"could not start the server" in p.stderr.read()
    finally:
        p.kill()
        p.wait()


def test_usage_error_exits_2_without_frames():
    r = subprocess.run(
        check=False,
        args=[sys.executable, "-m", "pylair.wrap"],
        capture_output=True,
        cwd=HERE,
        timeout=30,
    )
    assert r.returncode == L.EXIT_USAGE
    assert r.stdout == b""
    assert b"usage" in r.stderr


# ---- shutdown and the process tree -------------------------------------------


def test_shutdown_acks_and_stops_the_server(tmp_path):
    pidfile = tmp_path / "srv.pid"
    with Wrap(["--pidfile", str(pidfile)]) as w:
        w.recv()
        w.recv_method("ready")
        pid = read_pid(pidfile)
        w.send(proto.request(5, "shutdown", {"grace_ms": 1000}))
        ack = next(m for m in iter(w.recv, None) if m.get("id") == 5)
        assert ack["result"] == {}
        assert w.wait() == 0
        assert wait_dead(pid)


def test_shutdown_while_never_ready_still_exits_cleanly():
    with Wrap(["--never-listen"]) as w:
        w.recv()
        w.send(proto.request(5, "shutdown", {}))
        assert w.wait() == 0


def test_stdin_eof_stops_the_server_and_exits_zero(tmp_path):
    pidfile = tmp_path / "srv.pid"
    with Wrap(["--pidfile", str(pidfile)]) as w:
        w.recv()
        w.recv_method("ready")
        pid = read_pid(pidfile)
        w.proc.stdin.close()  # host died
        assert w.wait() == 0
        assert wait_dead(pid)


@pytest.mark.parametrize("leader", [True, False], ids=["group-leader", "plain-child"])
def test_grandchild_does_not_survive_shutdown(tmp_path, leader):
    """A SIGTERM-ignoring grandchild must die with the tree (M3's leak shape).

    `group-leader` is how the supervisor starts us (we own the group, the
    server shares it); `plain-child` is a launcher run by hand, which must
    still reach its descendants without signalling the caller's group.
    """
    if sys.platform == "win32" and not leader:
        pytest.skip("one process model on Windows")
    gpid_file, spid_file = tmp_path / "g.pid", tmp_path / "s.pid"
    with Wrap(
        ["--grandchild", str(gpid_file), "--pidfile", str(spid_file)],
        leader=leader,
    ) as w:
        w.recv()
        w.recv_method("ready")
        gpid, spid = read_pid(gpid_file), read_pid(spid_file)
        assert pid_alive(gpid)
        w.send(proto.request(1, "shutdown", {}))
        assert w.wait() == 0
        assert wait_dead(spid)
        assert wait_dead(gpid), "grandchild leaked past the launcher"


@pytest.mark.skipif(sys.platform == "win32", reason="SIGTERM escalation is POSIX")
def test_sigterm_ignoring_server_is_escalated_to_sigkill(tmp_path):
    spid_file = tmp_path / "s.pid"
    with Wrap(["--ignore-term", "--pidfile", str(spid_file)]) as w:
        w.recv()
        w.recv_method("ready")
        spid = read_pid(spid_file)
        w.send(proto.request(1, "shutdown", {}))
        assert w.wait() == 0
        assert wait_dead(spid)


def test_orphaned_grandchild_dies_when_the_server_exits(tmp_path):
    """The server spawns a grandchild and then exits: the grandchild is
    reparented, so a walk of the server's children cannot find it. The
    group (POSIX) still holds it; on Windows the supervisor's Job Object
    does (covered by the Rust suite), so this runs on POSIX only here."""
    if sys.platform == "win32":
        pytest.skip("Windows containment of orphans is the Job Object (Rust suite)")
    gpid_file = tmp_path / "g.pid"
    with Wrap(["--grandchild", str(gpid_file), "--exit-after", "1.0"]) as w:
        w.recv()
        w.recv_method("ready")
        gpid = read_pid(gpid_file)
        assert w.wait() == L.EXIT_SERVER_EXITED
        assert wait_dead(gpid), "reparented grandchild leaked"
