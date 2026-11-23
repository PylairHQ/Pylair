"""Harness bootstrap: run `python -m pylair` with a SIGUSR1 task dump.

conftest's Runtime spawns this instead of `-m pylair` so a wedged
runtime can be asked for every asyncio task's parked coroutine stack —
faulthandler only shows thread frames, and a live loop parked in
select() hides which await never resolved. Behaviour is identical to
-m (runpy with alter_sys); the handler is POSIX-only.
"""

import runpy
import signal
import sys


def _dump_tasks(_sig, _frame):
    import asyncio

    out = sys.stderr
    try:
        # asyncio.run() sets the policy loop; get_event_loop returns it
        # even called mid-run_forever from a signal handler.
        loop = asyncio.get_event_loop()
        tasks = asyncio.all_tasks(loop) if loop is not None else ()
    except Exception as e:  # noqa: BLE001 - never let the probe kill the probe
        print(f"diag: could not enumerate tasks: {e!r}", file=out, flush=True)
        return
    print(f"=== diag: {len(tasks)} asyncio task(s) ===", file=out, flush=True)
    for t in sorted(tasks, key=lambda t: t.get_name()):
        try:
            print(f"--- {t.get_name()} coro={t.get_coro()!r}", file=out, flush=True)
            t.print_stack(file=out)
        except Exception as e:  # noqa: BLE001
            print(f"    print_stack failed: {e!r}", file=out, flush=True)
    out.flush()


if sys.platform != "win32":
    signal.signal(signal.SIGUSR1, _dump_tasks)

runpy.run_module("pylair", run_name="__main__", alter_sys=True)
