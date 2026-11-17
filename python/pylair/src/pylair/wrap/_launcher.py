"""The wrap launcher: supervise one web server for the Rust supervisor.

Usage::

    python -m pylair.wrap [launcher options] --cmd <argv...> [launcher options]
    python -m pylair.wrap --framework {gradio|streamlit} --entry app.py [launcher options]

The command is everything after `--cmd`, except trailing launcher options
(`--ready-path`, `--probe-interval`, `--probe-timeout`, `--max-misses`),
which are peeled off the tail so both `--cmd app --ready-path /x` and
`--ready-path /x --cmd app` work. `{port}` in the command is replaced with
the chosen port and `PYLAIR_PORT` is exported to the server.

`--framework gradio|streamlit --entry` runs the matching adapter module
(`pylair.wrap._gradio` / `pylair.wrap._streamlit`) as the server instead
of a `--cmd`: the adapter pins the framework's host, port, share and
telemetry knobs from the app's env. `--framework command` (the default)
is `--cmd` mode, unchanged.

Lifecycle (docs/protocol-v1.md `ready`, docs/security.md section 9):

1. Stdout hygiene (the runtime's own `open_frame_channel`), then `hello`.
2. Pick a free loopback port, start the server, answer pings from the
   reader thread (pongs prove the frame pipe, never the server).
3. Ready: TCP connect, then GET the ready path until non-5xx. A slow
   answer is not a failure; readiness has no deadline here, the
   supervisor's `ready_timeout` owns it. Then `ready {"url": ...}`.
4. Health: one probe per interval, starting one interval after ready (the
   grace period). Only consecutive misses count; a success resets them.
5. Any server exit, and N consecutive misses, end the launcher with a
   non-zero exit code so the supervisor restarts the pair on a new port.

Every exit goes through `kill_tree` and then `os._exit`: a daemon thread
blocked on the stdin buffer can deadlock interpreter shutdown (the W0
spike hit this with `sys.exit`).
"""

from __future__ import annotations

import http.client
import os
import signal
import socket
import subprocess
import sys
import threading
import time
from collections.abc import Callable
from dataclasses import dataclass

from pylair import _json
from pylair import _protocol as proto
from pylair._transport import FrameChannel, FrameTooLarge, open_frame_channel

# Distinct exit codes, so a supervisor event names the cause.
EXIT_USAGE = 2  # bad command line (no frames were sent)
EXIT_START_FAILED = 3  # no free port, or the server could not be spawned
EXIT_SERVER_EXITED = 4  # the server process ended (any status)
EXIT_UNHEALTHY = 5  # consecutive health-probe misses: server is wedged

PORT_ATTEMPTS = 5
DEFAULT_READY_PATH = "/"
DEFAULT_PROBE_INTERVAL = 10.0
DEFAULT_PROBE_TIMEOUT = 5.0
DEFAULT_MAX_MISSES = 3
# How long a group member gets after SIGTERM before SIGKILL.
KILL_GRACE = 3.0
# Interpreter names that mean "the interpreter running this launcher": a
# packaged app has no `python` on PATH, only the bundled one.
_PYTHON_NAMES = frozenset({"python", "python3", "python.exe", "python3.exe"})


def _log(msg: str) -> None:
    print(f"[pylair.wrap] {msg}", file=sys.stderr, flush=True)


# ---- command line ---------------------------------------------------------


class UsageError(Exception):
    pass


@dataclass
class Options:
    cmd: list[str]
    ready_path: str = DEFAULT_READY_PATH
    probe_interval: float = DEFAULT_PROBE_INTERVAL
    probe_timeout: float = DEFAULT_PROBE_TIMEOUT
    max_misses: int = DEFAULT_MAX_MISSES
    framework: str = "command"
    entry: str | None = None


_FLAGS = (
    "--ready-path",
    "--probe-interval",
    "--probe-timeout",
    "--max-misses",
    "--framework",
    "--entry",
)

# Only the original four peel off a --cmd tail: `--cmd app --entry x`
# must keep passing --entry to the app exactly as W1 did. Framework and
# entry are launcher options recognised before --cmd only; a --cmd
# cannot combine with --framework anyway.
_TAIL_FLAGS = _FLAGS[:4]

FRAMEWORKS = ("command", "gradio", "streamlit")

# Cheap framework endpoints for readiness and health probes, chosen so a
# slow page hydration is never a probe miss (decision 0003). `/` is what
# `command` has always probed; it stays the default there.
READY_PATHS = {
    "command": "/",
    "gradio": "/config",
    "streamlit": "/_stcore/health",
}


def adapter_command(framework: str, entry: str) -> list[str]:
    """The server argv for a framework adapter: our own interpreter, the
    adapter module, the user's entry. The framework itself comes from the
    app's env, which for a packaged app is the bundled interpreter's."""
    return [sys.executable, "-m", f"pylair.wrap._{framework}", entry]


def _take_flag(tokens: list[str], i: int) -> tuple[str, str, int] | None:
    """Parse `--flag value` or `--flag=value` at tokens[i]."""
    tok = tokens[i]
    for flag in _FLAGS:
        if tok == flag:
            if i + 1 >= len(tokens):
                raise UsageError(f"{flag} needs a value")
            return flag, tokens[i + 1], 2
        if tok.startswith(flag + "="):
            return flag, tok[len(flag) + 1 :], 1
    return None


def parse_args(argv: list[str]) -> Options:
    if "--cmd" in argv:
        at = argv.index("--cmd")
        head, tail = list(argv[:at]), list(argv[at + 1 :])
    else:
        head, tail = list(argv), []
    raw: dict[str, str] = {}
    i = 0
    while i < len(head):
        taken = _take_flag(head, i)
        if taken is None:
            raise UsageError(f"unexpected argument: {head[i]!r}")
        raw[taken[0]] = taken[1]
        i += taken[2]
    # Peel launcher options off the tail. Walk from the end so the app's own
    # arguments are never reordered; only a recognised flag at the very end
    # is taken.
    while tail:
        if len(tail) >= 2 and tail[-2] in _TAIL_FLAGS:
            raw.setdefault(tail[-2], tail[-1])
            del tail[-2:]
        elif any(tail[-1].startswith(f + "=") for f in _TAIL_FLAGS):
            flag, _, value = tail[-1].partition("=")
            raw.setdefault(flag, value)
            del tail[-1]
        else:
            break

    framework = raw.get("--framework", "command")
    if framework not in FRAMEWORKS:
        raise UsageError(
            f"unknown --framework {framework!r} (one of {', '.join(FRAMEWORKS)})"
        )
    entry = raw.get("--entry")
    if framework == "command":
        if entry is not None:
            raise UsageError("--entry only applies to --framework gradio|streamlit")
        if "--cmd" not in argv:
            raise UsageError("missing --cmd <argv...>")
        if not tail:
            raise UsageError("--cmd needs a command")
        cmd = tail
    else:
        if "--cmd" in argv:
            raise UsageError(f"--cmd does not combine with --framework {framework}")
        if entry is None:
            raise UsageError(f"--framework {framework} needs --entry app.py")
        cmd = adapter_command(framework, entry)

    opts = Options(cmd=cmd, framework=framework, entry=entry)
    opts.ready_path = raw.get("--ready-path", READY_PATHS[framework])
    try:
        if "--probe-interval" in raw:
            opts.probe_interval = float(raw["--probe-interval"])
        if "--probe-timeout" in raw:
            opts.probe_timeout = float(raw["--probe-timeout"])
        if "--max-misses" in raw:
            opts.max_misses = int(raw["--max-misses"])
    except ValueError as e:
        raise UsageError(f"bad option value: {e}") from None
    if not opts.ready_path.startswith("/"):
        raise UsageError(f"--ready-path must start with '/': {opts.ready_path!r}")
    if opts.probe_interval <= 0 or opts.probe_timeout <= 0 or opts.max_misses < 1:
        raise UsageError("probe interval/timeout must be > 0 and max-misses >= 1")
    return opts


def build_command(argv: list[str], port: int) -> list[str]:
    """Substitute `{port}` and resolve a bare `python` to this interpreter."""
    cmd = [a.replace("{port}", str(port)) for a in argv]
    if cmd[0].lower() in _PYTHON_NAMES:
        cmd[0] = sys.executable
    return cmd


# ---- port -----------------------------------------------------------------


def pick_free_port(
    attempts: int = PORT_ATTEMPTS,
    make_socket: Callable[..., socket.socket] = socket.socket,
) -> int:
    """Bind 127.0.0.1:0, read the port, close it. Retries a failed bind.

    The port is only reserved until we close the socket: another process can
    take it before the server binds. That window is accepted. A server that
    loses the race exits, the launcher exits non-zero, and the supervisor's
    restart picks a new port.
    """
    last: OSError | None = None
    for _ in range(attempts):
        s = make_socket(socket.AF_INET, socket.SOCK_STREAM)
        try:
            s.bind(("127.0.0.1", 0))
            return s.getsockname()[1]
        except OSError as e:
            last = e
        finally:
            s.close()
    raise OSError(f"no free loopback port after {attempts} attempts: {last}")


# ---- probes ---------------------------------------------------------------


def probe_failure(port: int, path: str, timeout: float) -> str | None:
    """TCP connect, then GET `path`: None when the server is healthy (a
    non-5xx answer in time), else a short reason fit for the log."""
    try:
        socket.create_connection(("127.0.0.1", port), timeout=min(timeout, 2.0)).close()
    except OSError as e:
        return f"connect failed: {e!r}"
    conn = http.client.HTTPConnection("127.0.0.1", port, timeout=timeout)
    try:
        conn.request("GET", path, headers={"Connection": "close"})
        status = conn.getresponse().status
    except (OSError, http.client.HTTPException, ValueError) as e:
        return f"GET {path} failed: {e!r}"
    finally:
        conn.close()
    return f"GET {path} answered {status}" if status >= 500 else None


def probe(port: int, path: str, timeout: float) -> bool:
    return probe_failure(port, path, timeout) is None


# ---- process tree ---------------------------------------------------------


def _snapshot() -> list[tuple[int, int, int, str, str]]:
    """(pid, ppid, pgid, state, start) for every process we can see (POSIX).

    `start` is the process start time as a string (Linux: clock ticks since
    boot; elsewhere `ps lstart`). Paired with the pid it identifies a
    process, so a recycled pid is never mistaken for one we signalled.
    """
    rows: list[tuple[int, int, int, str, str]] = []
    if os.path.isdir("/proc/self"):
        for name in os.listdir("/proc"):
            if not name.isdigit():
                continue
            try:
                with open(f"/proc/{name}/stat", "rb") as f:
                    data = f.read()
                # comm may contain spaces and parens: fields start after
                # the LAST ')'. rest[0] is field 3 (state), so field 22
                # (starttime) is rest[19].
                rest = data[data.rindex(b")") + 2 :].split()
                rows.append(
                    (
                        int(name),
                        int(rest[1]),
                        int(rest[2]),
                        rest[0].decode(),
                        rest[19].decode(),
                    )
                )
            except (OSError, ValueError, IndexError):
                continue
        return rows
    try:
        out = subprocess.run(
            check=False,
            args=["ps", "-A", "-o", "pid=,ppid=,pgid=,stat=,lstart="],
            capture_output=True,
            text=True,
            timeout=5,
        ).stdout
    except (OSError, subprocess.SubprocessError):
        return rows
    for line in out.splitlines():
        parts = line.split()
        if len(parts) >= 5:
            try:
                rows.append(
                    (
                        int(parts[0]),
                        int(parts[1]),
                        int(parts[2]),
                        parts[3],
                        " ".join(parts[4:]),
                    )
                )
            except ValueError:
                continue
    return rows


def _scan(known: dict[int, str]) -> tuple[dict[int, str], list[int]]:
    """One snapshot, two answers: the tree's current members and which of
    `known` (pid -> start) are still alive.

    Members are the launcher's descendants and, when it leads its own
    process group (the supervisor spawns it that way and `killpg`s it),
    every group member too. A launcher that does not lead a group (run by
    hand) must not count the group: signalling a group it merely belongs to
    would hit the shell or test runner that started it.
    """
    me = os.getpid()
    rows = [r for r in _snapshot() if r[3][:1] not in ("Z", "X")]
    start_of = {pid: start for pid, _, _, _, start in rows}
    kids: dict[int, list[int]] = {}
    for pid, ppid, _, _, _ in rows:
        kids.setdefault(ppid, []).append(pid)
    found: set[int] = set()
    stack = list(kids.get(me, []))
    while stack:
        pid = stack.pop()
        if pid not in found:
            found.add(pid)
            stack.extend(kids.get(pid, []))
    if os.getpgrp() == me:
        found.update(pid for pid, _, pgid, _, _ in rows if pgid == me)
    found.discard(me)
    members = {pid: start_of[pid] for pid in found}
    live = [pid for pid, start in known.items() if start_of.get(pid) == start]
    return members, live


def tree_members() -> list[int]:
    """Live processes that belong to this launcher (POSIX), excluding it."""
    return sorted(_scan({})[0])


def kill_tree(proc: subprocess.Popen, grace: float = KILL_GRACE) -> None:
    """Stop the server and everything it started.

    POSIX: SIGTERM every member, wait `grace`, then SIGKILL survivors. The
    set of members is remembered from the first look and escalated on, not
    re-derived from the tree each time: a server that exits on SIGTERM
    leaves a SIGTERM-ignoring grandchild reparented to init (or a
    subreaper), out of every descendant walk, and only the remembered
    identity (pid plus start time) still names it. Members that appear
    later (a server that forks while shutting down) are added. Windows:
    `taskkill /T` for the tree; the supervisor's Job Object is the backstop
    for anything it cannot reach.
    """
    if os.name == "nt":
        subprocess.run(
            check=False,
            args=["taskkill", "/F", "/T", "/PID", str(proc.pid)],
            capture_output=True,
            creationflags=0x0800_0000,  # CREATE_NO_WINDOW
        )
        try:
            proc.wait(timeout=grace)
        except subprocess.TimeoutExpired:
            proc.kill()
        return
    # Our own group-wide SIGTERM must not take the launcher down.
    for sig in (signal.SIGTERM, signal.SIGINT):
        try:
            signal.signal(sig, signal.SIG_IGN)
        except ValueError:  # not the main thread
            pass
    known, _ = _scan({})
    for pid in known:
        _signal(pid, signal.SIGTERM)
    deadline = time.monotonic() + grace
    live: list[int] = list(known)
    while time.monotonic() < deadline:
        proc.poll()  # reap the leader so it never reads as a live member
        members, live = _scan(known)
        for pid, start in members.items():
            if pid not in known:  # a late fork: it gets the same treatment
                known[pid] = start
                live.append(pid)
                _signal(pid, signal.SIGTERM)
        if not live:
            return
        time.sleep(0.05)
    for pid in live:
        _signal(pid, signal.SIGKILL)
    proc.poll()


def _signal(pid: int, sig: int) -> None:
    try:
        os.kill(pid, sig)
    except (ProcessLookupError, PermissionError):
        pass


def spawn_server(cmd: list[str], env: dict[str, str]) -> subprocess.Popen:
    """Start the server in the launcher's process group (never `setsid`).

    A server in its own group would be out of reach of the supervisor's
    `killpg` of the launcher. stdin is closed off so the server cannot read
    host frames; stdout and stderr both go to the log stream (fd 2).
    """
    kwargs: dict = {
        "env": env,
        "stdin": subprocess.DEVNULL,
        "stdout": 2,
        "stderr": 2,
    }
    if os.name == "nt":
        kwargs["creationflags"] = subprocess.CREATE_NEW_PROCESS_GROUP
    return subprocess.Popen(cmd, **kwargs)


# ---- the launcher ---------------------------------------------------------


class Launcher:
    def __init__(self, opts: Options, channel: FrameChannel) -> None:
        self.opts = opts
        self._channel = channel
        self._wire_lock = threading.Lock()
        self._done = threading.Event()
        self._code: int | None = None
        self._code_lock = threading.Lock()
        self.port = 0
        self.proc: subprocess.Popen | None = None

    # -- frames --

    def send(self, msg: dict) -> None:
        body = _json.dumps(msg).encode()
        with self._wire_lock:
            try:
                self._channel.write_frame(body)
            except (OSError, ValueError):
                # Frame fd dead: the host is gone; stdin EOF will follow.
                pass

    def request_exit(self, code: int) -> None:
        """First caller wins: the exit code names the first thing that ended."""
        with self._code_lock:
            if self._code is None:
                self._code = code
        self._done.set()

    def _reader(self) -> None:
        # Pongs are answered here, not on the main thread (protocol section 5): they
        # prove the frame pipe, never the server. Health is probed over HTTP.
        try:
            while True:
                body = self._channel.read_frame()
                if body is None:
                    self.request_exit(0)  # stdin EOF: host is gone
                    return
                try:
                    msg = _json.loads(body)
                except Exception:  # noqa: BLE001 - a bad frame must not kill us
                    _log("dropping an unparseable frame")
                    continue
                self._dispatch(msg)
        except (FrameTooLarge, OSError):
            self.request_exit(0)

    def _dispatch(self, msg: object) -> None:
        if not isinstance(msg, dict):
            return
        method = msg.get("method")
        rid = msg.get("id")
        params = msg.get("params")
        params = params if isinstance(params, dict) else {}
        if method == "ping":
            self.send(
                proto.notification(
                    "pong", {"seq": params.get("seq"), "loop_stale_ms": 0}
                )
            )
        elif method == "shutdown" and isinstance(rid, (int, str)):
            self.send(proto.response(rid, {}))
            self.request_exit(0)
        elif method == "cancel" and isinstance(rid, (int, str)):
            self.send(
                proto.response(rid, {"cancelled": False, "reason": "task_not_found"})
            )
        elif isinstance(rid, (int, str)) and method is not None:
            # A wrap runtime exposes nothing: answer instead of hanging the
            # caller (protocol section 3.1: every call gets exactly one response).
            self.send(
                proto.error_response(
                    rid,
                    proto.FUNCTION_NOT_FOUND,
                    "wrap mode exposes no functions",
                )
            )
        # notifications from the host (stream.credit, ...) are ignored

    # -- probing --

    def _probe_loop(self) -> None:
        o, port = self.opts, self.port
        last_note = 0.0
        while not self._done.is_set():  # readiness: no deadline of our own
            why = probe_failure(port, o.ready_path, o.probe_timeout)
            if why is None:
                break
            # Say what the wait is stuck on, at most every 5s: this is the
            # startup log a window shows while the app loads.
            if time.monotonic() - last_note >= 5.0:
                last_note = time.monotonic()
                _log(f"not ready yet: {why}")
            if self._done.wait(0.2):
                return
        if self._done.is_set():
            return
        url = f"http://127.0.0.1:{port}/"
        _log(f"ready: {url}")
        self.send(proto.notification("ready", {"url": url}))
        misses = 0
        while not self._done.wait(o.probe_interval):
            why = probe_failure(port, o.ready_path, o.probe_timeout)
            if why is None:
                misses = 0
                continue
            misses += 1
            _log(f"health probe missed ({misses}/{o.max_misses}): {why}")
            if misses >= o.max_misses:
                _log("server is not answering: exiting so the supervisor restarts it")
                self.request_exit(EXIT_UNHEALTHY)
                return

    # -- main --

    def run(self) -> int:
        self.send(_hello())
        threading.Thread(target=self._reader, daemon=True, name="wrap-reader").start()
        try:
            self.port = pick_free_port()
        except OSError as e:
            _log(str(e))
            return EXIT_START_FAILED
        cmd = build_command(self.opts.cmd, self.port)
        env = {**os.environ, "PYLAIR_PORT": str(self.port)}
        env.setdefault("PYTHONUNBUFFERED", "1")  # live logs through the pipe
        try:
            self.proc = spawn_server(cmd, env)
        except OSError as e:
            _log(f"could not start the server {cmd[0]!r}: {e}")
            return EXIT_START_FAILED
        _log(f"started pid {self.proc.pid} on 127.0.0.1:{self.port}: {' '.join(cmd)}")
        for sig in _term_signals():
            try:
                signal.signal(sig, lambda s, f: self.request_exit(128 + s))
            except ValueError:
                pass
        threading.Thread(
            target=self._probe_loop, daemon=True, name="wrap-probe"
        ).start()
        while not self._done.wait(0.2):
            status = self.proc.poll()
            if status is not None:
                _log(f"server exited with status {status}")
                self.request_exit(EXIT_SERVER_EXITED)
        kill_tree(self.proc)
        return self._code if self._code is not None else 0


def _term_signals() -> list[int]:
    sigs = [signal.SIGTERM, signal.SIGINT]
    if hasattr(signal, "SIGBREAK"):
        sigs.append(signal.SIGBREAK)
    return sigs


def _hello() -> dict:
    msg = proto.hello([])
    msg["params"]["kind"] = "web"  # informational; the host's config decides
    return msg


def main(argv: list[str] | None = None) -> int:
    """Run the launcher. Returns the exit code; the caller must `os._exit`."""
    try:
        opts = parse_args(list(sys.argv[1:] if argv is None else argv))
    except UsageError as e:
        _log(
            "usage: python -m pylair.wrap [--framework {command|gradio|streamlit}] "
            f"[--entry app.py | --cmd <argv...>] [--ready-path /]  ({e})"
        )
        return EXIT_USAGE
    channel = open_frame_channel()  # hygiene first: fd 1 -> stderr
    return Launcher(opts, channel).run()
