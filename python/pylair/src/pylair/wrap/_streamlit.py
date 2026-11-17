"""Streamlit adapter for wrap mode (W3): `python -m pylair.wrap._streamlit app.py`.

Runs `streamlit run ENTRY` in-process through the framework's own CLI
entry (`streamlit.web.cli.main`), with the loopback bind, headless mode,
usage stats off, the file watcher off and the script health check
endpoint on. Multipage apps work as usual: Streamlit discovers `pages/`
next to the entry.

Telemetry is off twice: `STREAMLIT_BROWSER_GATHER_USAGE_STATS=false`
(read by streamlit's config at import) and the explicit
`--browser.gatherUsageStats false` flag. Supported major: streamlit 1.x
(CI runs 1.67). Other majors warn and still run.
"""

from __future__ import annotations

import importlib
import os
import sys

from pylair.wrap._shared import check_version, log


def _usage() -> int:
    log("usage: python -m pylair.wrap._streamlit ENTRY.py")
    return 2


def main(argv: list[str] | None = None) -> int:
    argv = sys.argv[1:] if argv is None else argv
    if len(argv) != 1:
        return _usage()
    entry = argv[0]
    if not os.path.isfile(entry):
        log(f"entry not found: {entry}")
        return 2
    port = os.environ.get("PYLAIR_PORT")
    if port is None:
        log("PYLAIR_PORT is not set: run under `python -m pylair.wrap`")
        return 2

    os.environ["STREAMLIT_BROWSER_GATHER_USAGE_STATS"] = "false"
    try:
        cli = importlib.import_module("streamlit.web.cli")
    except ImportError as e:
        log(
            f"streamlit is not installed in this environment ({e}); the "
            "framework comes from the app's env, not from pylair"
        )
        return 3
    warning = check_version("streamlit")
    if warning:
        log(warning)

    # The console_script entry is streamlit.web.cli:main; feeding it an
    # argv is the most stable call across 1.x versions (the bootstrap
    # signature has moved around).
    sys.argv = [
        "streamlit",
        "run",
        entry,
        "--server.address",
        "127.0.0.1",
        "--server.port",
        port,
        "--server.headless",
        "true",
        "--browser.gatherUsageStats",
        "false",
        "--server.fileWatcherType",
        "none",
        # Exposes /_stcore/script-health-check: a plain GET that executes
        # the entry in a throwaway session. Lets tests (and later doctor)
        # prove the app itself runs, not just the server shell.
        "--server.scriptHealthCheckEnabled",
        "true",
    ]
    cli.main()  # click group: serves until the process is killed
    return 0


if __name__ == "__main__":
    sys.exit(main())
