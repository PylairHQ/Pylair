"""Wrap framework adapters: real Gradio and Streamlit apps through the
launcher (`--framework gradio|streamlit --entry app.py`).

The framework tests need the `wrap` dependency group
(`uv sync --group wrap`); without it they skip, and the parse-level
tests still run. Every wrapped server answers on a loopback port the
launcher chose and is killed with the launcher on shutdown.
"""

from __future__ import annotations

import os
import re
import socket
import subprocess
import sys
import time
from pathlib import Path

import pytest

from pylair import _protocol as proto
from pylair.wrap import _launcher as L
from test_wrap import Wrap, get, wait_dead

HERE = Path(__file__).parent
APPS = HERE / "wrap_apps"
WAIT = 90.0  # real frameworks are slow to import; CI machines are slower

# A server pid the launcher logged ("started pid N on 127.0.0.1:P").
_STARTED = re.compile(r"started pid (\d+) on 127\.0\.0\.1:(\d+)")


@pytest.fixture
def gradio():
    return pytest.importorskip("gradio")


@pytest.fixture
def streamlit():
    return pytest.importorskip("streamlit")


class AdapterWrap(Wrap):
    """A spawned `python -m pylair.wrap --framework F --entry E`."""

    def __init__(self, framework: str, entry: Path, *opts: str):
        argv = [
            sys.executable,
            "-m",
            "pylair.wrap",
            "--framework",
            framework,
            "--entry",
            str(entry),
            *opts,
        ]
        self._spawn(argv)

    def ready_url(self) -> str:
        self.recv()  # hello
        return self.recv_method("ready", timeout=WAIT)["params"]["url"]

    def server_pid_port(self) -> tuple[int, int]:
        """(pid, port) of the server process the launcher reported."""
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            m = _STARTED.search("".join(self.stderr))
            if m:
                return int(m.group(1)), int(m.group(2))
            time.sleep(0.1)
        raise AssertionError(f"launcher never logged a server pid; {self.diag()}")


def external_addr() -> str | None:
    """A non-loopback IPv4 of this machine, for a refused-bind check."""
    try:
        s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        try:
            s.connect(("192.0.2.1", 9))  # TEST-NET: routed, never answered
            addr = s.getsockname()[0]
            return addr if not addr.startswith("127.") else None
        finally:
            s.close()
    except OSError:
        return None


def assert_loopback_only(port: int) -> None:
    """The server must not answer on a real interface: it proves the
    adapter's forced bind, not just a config value."""
    addr = external_addr()
    if addr is None:
        return  # nowhere to test from
    with pytest.raises((ConnectionRefusedError, TimeoutError, OSError)):
        socket.create_connection((addr, port), timeout=3)


# ---- argument parsing (no frameworks needed) ----------------------------------


def test_parse_framework_gradio_builds_the_adapter_command():
    o = L.parse_args(["--framework", "gradio", "--entry", "app.py"])
    assert o.cmd == [sys.executable, "-m", "pylair.wrap._gradio", "app.py"]
    assert o.ready_path == "/config"  # cheap route, not the heavy page
    assert o.entry == "app.py"


def test_parse_framework_streamlit():
    o = L.parse_args(["--framework", "streamlit", "--entry", "app.py"])
    assert o.cmd == [sys.executable, "-m", "pylair.wrap._streamlit", "app.py"]
    assert o.ready_path == "/_stcore/health"


def test_parse_framework_command_behaves_like_today():
    o = L.parse_args(["--framework", "command", "--cmd", "srv", "--port", "{port}"])
    assert o.cmd == ["srv", "--port", "{port}"]
    assert o.ready_path == "/"
    # And a bare --cmd with no --framework is still command mode.
    assert L.parse_args(["--cmd", "srv"]).framework == "command"
    # W1 tail rule preserved: --framework/--entry after --cmd stay app
    # arguments, they are never peeled as launcher options.
    o = L.parse_args(["--cmd", "srv", "--entry", "x", "--framework", "y"])
    assert o.cmd == ["srv", "--entry", "x", "--framework", "y"]
    assert o.framework == "command" and o.entry is None


def test_framework_ready_path_can_be_overridden():
    o = L.parse_args(
        ["--framework", "gradio", "--entry", "a.py", "--ready-path", "/healthz"]
    )
    assert o.ready_path == "/healthz"


@pytest.mark.parametrize(
    "argv",
    [
        ["--entry", "a.py"],  # entry needs a framework
        ["--framework", "gradio"],  # entry missing
        ["--framework", "streamlit"],  # entry missing
        ["--framework", "bogus", "--entry", "a.py"],
        ["--framework", "gradio", "--entry", "a.py", "--cmd", "srv"],
        ["--framework", "command", "--entry", "a.py", "--cmd", "srv"],
    ],
)
def test_parse_framework_rejects_bad_combos(argv):
    with pytest.raises(L.UsageError):
        L.parse_args(argv)


def test_version_check_warns_off_pin_and_stays_quiet_on_it():
    from pylair.wrap import _shared

    assert _shared.check_version("gradio", "5.50.0") is None
    assert _shared.check_version("gradio", "4.44.0") is not None
    assert _shared.check_version("gradio", "6.0") is not None
    assert _shared.check_version("gradio", "not-a-version") is not None
    assert _shared.check_version("streamlit", "1.67.0") is None
    assert _shared.check_version("streamlit", "2.0.0") is not None
    # A package that is not installed gets no warning either: its import
    # error later names the problem and the check must not double-report.
    # Whatever is installed in this env, the lookup must not raise.
    _shared.check_version("gradio")
    _shared.check_version("streamlit")


def test_missing_framework_is_a_clean_error_not_a_crash(tmp_path):
    """The adapter names the missing framework and exits non-zero; the
    supervisor sees a server exit, not a dead launcher."""
    entry = tmp_path / "app.py"
    entry.write_text("print('never runs')")
    # gradio may not even be installed here; force the missing-module path
    # either way so the test means the same thing in both envs.
    code = (
        "import sys, runpy; sys.modules['gradio'] = None; sys.argv = "
        f"['x', {str(entry)!r}]; runpy.run_module('pylair.wrap._gradio', "
        "run_name='__main__')"
    )
    r = subprocess.run(
        [sys.executable, "-c", code],
        check=False,
        capture_output=True,
        text=True,
        timeout=60,
        env={**os.environ, "PYLAIR_PORT": "1"},
    )
    assert r.returncode != 0
    assert "gradio" in r.stderr and "not installed" in r.stderr


# ---- gradio --------------------------------------------------------------------


def test_gradio_launch_entry_ready_marker_and_forced_bind(gradio):
    with AdapterWrap("gradio", APPS / "gradio_launch.py") as w:
        url = w.ready_url()
        _, port = w.server_pid_port()
        body = get(url + "config")
        assert b"pylair-wrap-gradio-marker" in body  # served config blob
        text = w.err_text()
        # The entry ran under us and saw the telemetry kill switch.
        assert "w3-entry-ran analytics=False" in text
        # launch() was called with hostile values; ours won. Gradio logs
        # its own bind line: loopback, and the launcher's port (the entry
        # asked for 0.0.0.0:9999).
        m = re.search(r"Running on local URL:\s+http://127\.0\.0\.1:(\d+)", text)
        assert m and int(m.group(1)) == port
        # No tunnel: gradio would log a public gradio.live URL if share won.
        assert "gradio.live" not in text
        assert "Running on public URL" not in text
        assert_loopback_only(port)


def test_gradio_demo_only_script_is_launched_by_the_adapter(gradio):
    with AdapterWrap("gradio", APPS / "gradio_demo_only.py") as w:
        url = w.ready_url()
        body = get(url + "config")
        assert b"pylair-wrap-gradio-marker" in body
        assert "w3-entry-ran demo-only" in w.err_text()


def test_gradio_server_tree_reaped_on_shutdown(gradio):
    with AdapterWrap("gradio", APPS / "gradio_demo_only.py") as w:
        w.ready_url()
        pid, _ = w.server_pid_port()
        w.send(proto.request(5, "shutdown", {"grace_ms": 1000}))
        assert w.wait(timeout=WAIT) == 0
        assert wait_dead(pid)


# ---- streamlit ------------------------------------------------------------------


def test_streamlit_single_page_ready_health_and_telemetry(streamlit):
    with AdapterWrap("streamlit", APPS / "streamlit_app.py") as w:
        url = w.ready_url()
        # Cheap liveness (the launcher's probe) and the app shell.
        assert get(url + "_stcore/health") == b"ok"
        assert b"streamlit" in get(url).lower()
        # script-health-check executes the entry in a throwaway session:
        # 200 proves OUR app runs, and its print then lands in the log.
        assert get(url + "_stcore/script-health-check") == b"ok"
        deadline = time.monotonic() + 20
        while "w3-entry-ran" not in w.err_text():
            assert time.monotonic() < deadline, (
                f"entry never ran after script-health-check; {w.diag()}"
            )
            time.sleep(0.5)
        assert "w3-entry-ran usageStats=false" in w.err_text()


def test_streamlit_multipage_serves_extra_pages(streamlit):
    with AdapterWrap("streamlit", APPS / "streamlit_multi" / "streamlit_app.py") as w:
        url = w.ready_url()
        assert get(url + "_stcore/health") == b"ok"
        # Multipage URLs serve the app shell; routing is client-side.
        r = get(url + "second_page")
        assert b"streamlit" in r.lower()


def test_streamlit_server_tree_reaped_on_shutdown(streamlit):
    with AdapterWrap("streamlit", APPS / "streamlit_app.py") as w:
        w.ready_url()
        pid, _ = w.server_pid_port()
        w.send(proto.request(5, "shutdown", {"grace_ms": 1000}))
        assert w.wait(timeout=WAIT) == 0
        assert wait_dead(pid)


# ---- both: loopback proof --------------------------------------------------------


def test_streamlit_binds_loopback_only(streamlit):
    with AdapterWrap("streamlit", APPS / "streamlit_app.py") as w:
        w.ready_url()
        _, port = w.server_pid_port()
        assert_loopback_only(port)
