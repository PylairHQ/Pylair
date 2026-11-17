"""Gradio adapter for wrap mode (W3): `python -m pylair.wrap._gradio app.py`.

Patches `gradio.Blocks.launch`, then runs the user's entry in this
process via `runpy.run_path(run_name="__main__")`. The patch forces
`server_name=127.0.0.1`, `server_port=$PYLAIR_PORT`, `inbrowser=False`,
`share=False` and `prevent_thread_lock=False`: the script can ask for a
public bind, a browser open, a tunnel or a non-blocking launch and it
loses every time. `Interface` and `ChatInterface` inherit launch() from
`Blocks`, so one patch covers all three.

Scripts that call `demo.launch()` never return from it (the forced block
holds the main thread, which is what the launcher wants). Scripts that
only define `demo` are launched by us after run_path returns.

`GRADIO_ANALYTICS_ENABLED=False` is set before gradio is imported; it
reads the env at import time. Supported major: gradio 5.x (CI runs
5.50). Other majors warn and still run.
"""

from __future__ import annotations

import importlib
import os
import runpy
import sys

from pylair.wrap._shared import check_version, log


def _usage() -> int:
    log("usage: python -m pylair.wrap._gradio ENTRY.py")
    return 2


def main(argv: list[str] | None = None) -> int:
    argv = sys.argv[1:] if argv is None else argv
    if len(argv) != 1:
        return _usage()
    entry = argv[0]
    if not os.path.isfile(entry):
        log(f"entry not found: {entry}")
        return 2
    port_s = os.environ.get("PYLAIR_PORT")
    if port_s is None:
        log("PYLAIR_PORT is not set: run under `python -m pylair.wrap`")
        return 2
    port = int(port_s)

    # Telemetry off before the import; gradio reads the env at import time.
    os.environ["GRADIO_ANALYTICS_ENABLED"] = "False"
    try:
        gr = importlib.import_module("gradio")
    except ImportError as e:
        log(
            f"gradio is not installed in this environment ({e}); the "
            "framework comes from the app's env, not from pylair"
        )
        return 3
    warning = check_version("gradio")
    if warning:
        log(warning)

    original = gr.Blocks.launch

    def launch(self, *args, **kwargs):
        # Force, not default: whatever the script passed loses.
        kwargs.update(
            server_name="127.0.0.1",
            server_port=port,
            inbrowser=False,
            share=False,
            prevent_thread_lock=False,
        )
        return original(self, *args, **kwargs)

    gr.Blocks.launch = launch
    globals_after = runpy.run_path(entry, run_name="__main__")

    # Reaching this line means the entry finished without a blocking
    # launch: the demo-only shape. `app`/`iface` names are deliberately
    # not guessed; the documented convention is `demo`.
    demo = globals_after.get("demo")
    if demo is None:
        log("entry never called launch() and defines no `demo`; nothing to serve")
        return 4
    if not isinstance(demo, gr.Blocks):
        log(f"`demo` is a {type(demo).__name__}, not a gradio.Blocks; cannot launch")
        return 4
    log("entry defined `demo` without launching; launching it now")
    demo.launch()
    return 0


if __name__ == "__main__":
    sys.exit(main())
