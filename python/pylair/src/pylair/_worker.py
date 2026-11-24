"""Isolated-worker entry point: `python -m pylair._worker [--serve] <module> <fn>`.

Internal protocol (not the wire protocol). One-shot mode (default): reads
the call input as one JSON document on stdin, runs the named @expose'd
function from the given user module, writes {"result": ...} or {"error":
...} as one JSON document on a PRIVATE dup of stdout, exits 0. Non-zero
exit = WORKER_DIED to the parent.

Serve mode (`--serve`, warm workers, decision 0008): imports the module
once, then serves length-prefixed request frames one at a time over the
same private channel — {"op": "call", "input": {...}} -> one framed
{"result"|"error"|"interrupted"} body each — until stdin EOF or
{"op": "shutdown"}.

The result channel gets the same stdout hygiene as the runtime's frame
channel (protocol §1): fd 1 is redirected to stderr before user code runs,
so a `print()` inside an isolated function can never corrupt the result.
"""

from __future__ import annotations

import importlib
import os
import signal
import sys
import threading
import time
import traceback

from pylair import _json, _transport
from pylair import _protocol as proto
from pylair._expose import ExposedFn, exposed_functions


def _exit_with_parent() -> None:
    """Reap this worker's whole process group if the runtime disappears.

    Workers run in their own session (so a single task can be cancelled by
    killing exactly its group), which means a hard kill of the runtime's own
    process group cannot reach them. When the parent is gone the worker is
    re-parented; SIGKILL the group (worker + any grandchildren, SIGTERM
    ignorers included) rather than run on unsupervised. On Windows the host's
    Job Object already contains workers.
    """
    if sys.platform == "win32":
        return
    # The runtime tells us the ppid to expect — a worker still in
    # interpreter startup when the runtime died is already reparented,
    # and getppid alone would anchor every watcher below on init. Die
    # now rather than run unsupervised.
    parent = int(os.environ.get("PYLAIR_WORKER_PARENT") or os.getppid())
    if os.getppid() != parent:
        os.kill(os.getpid(), signal.SIGKILL)

    # The group-reap itself must live in a separate PROCESS. A watcher thread
    # in the worker can be starved by a native call holding the GIL — and on
    # Linux pdeathsig kills the worker the instant the runtime dies, long
    # before such a thread could fire, leaving the group behind. The reaper
    # is forked here, while the worker is still single-threaded and before
    # user imports; it sits in this process group watching the worker (its
    # own ppid) and the runtime (a kill-0 probe). Either dying ends the whole
    # group — worker exit also sweeps grandchildren a finished call spawned.
    try:
        worker = os.getpid()
        if os.fork() == 0:
            try:
                # The reaper shares the worker's process group, so a warm
                # cancel's killpg(SIGINT) would land here too — ignore it:
                # the reaper answers only to parent loss or worker exit.
                signal.signal(signal.SIGINT, signal.SIG_IGN)
                # Inherited fds must not pin the pipes: this copy of fd 1 is
                # the result channel — holding it would hide EOF forever.
                os.closerange(0, 3)
                while True:
                    time.sleep(0.5)
                    if os.getppid() != worker:
                        break
                    try:
                        os.kill(parent, 0)
                    except ProcessLookupError:
                        break
                    except OSError:
                        pass  # EPERM: process exists
                os.killpg(0, signal.SIGKILL)
            finally:
                os._exit(1)
    except OSError:
        pass  # fork failed — the watchers below remain the fallback

    # Linux: kernel-level backstop for the watcher below — PR_SET_PDEATHSIG
    # fires even if a native call is holding the GIL (which would stall the
    # polling thread). If the runtime died before prctl ran, getppid already
    # changed: kill ourselves now — the race the ppid check exists for. The
    # reaper above reaps the group this leaves behind.
    if sys.platform.startswith("linux"):
        try:
            import ctypes

            ctypes.CDLL("libc.so.6", use_errno=True).prctl(1, signal.SIGKILL)
            if os.getppid() != parent:
                os.kill(os.getpid(), signal.SIGKILL)
        except (ImportError, OSError) as e:
            # No ctypes or a libc without prctl — the ppid watcher below
            # remains the fallback, so a setup failure is not fatal.
            print(f"[pylair] worker: pdeathsig setup failed: {e!r}", file=sys.stderr)

    def watch() -> None:
        while True:
            time.sleep(0.5)
            if os.getppid() != parent:
                try:
                    os.killpg(0, signal.SIGKILL)
                finally:
                    os._exit(1)

    threading.Thread(target=watch, daemon=True, name="pylair-parent-watch").start()


# Interrupt machinery (0008 §2, serve mode only): the parent sends
# SIGINT (POSIX, killpg) or CTRL_BREAK (Windows, SIGBREAK) to interrupt
# a running warm call. The handler raises KeyboardInterrupt ONLY while
# the user function executes — `_raise_active` bounds the window. An
# interrupt landing while the worker reads a request, builds a payload
# or writes a frame is recorded and ignored: a raise in `read_frame`
# would kill the loop; a raise mid-`write_frame` would corrupt the
# channel, so a response that started writing always completes.
_raise_active = False
_interrupt_seen = 0
_interrupt_installed = False
# Tell the parent the raise window is about to open (0008 §2): the
# cancel path interrupts only after this frame lands, so a signal can
# never be wasted on a worker that has not reached the user function.
_STARTED = b'{"started": true}'
_IGNORED_LINE = (
    "[pylair] worker: interrupt outside the raise window — ignored\n".encode()
)


def _install_interrupt() -> None:
    """Map the interrupt signal to a windowed KeyboardInterrupt.

    POSIX: SIGINT. Windows: SIGBREAK (the worker is spawned with
    CREATE_NEW_PROCESS_GROUP so CTRL_BREAK_EVENT targets exactly its
    group) — SIGBREAK's default disposition is terminate, so this must
    run before any user import.
    """

    def _on_interrupt(_signum, _frame) -> None:
        global _interrupt_seen
        _interrupt_seen += 1
        if _raise_active:
            raise KeyboardInterrupt
        # Not a buffered print: a second signal landing while the writer
        # is mid-write re-enters it and raises RuntimeError, escaping
        # into whatever the main thread was running — inside the serve
        # loop's channel reads/writes that exit reads as worker death.
        # os.write has no Python-level buffer state to re-enter; nested
        # calls merely interleave the line.
        try:
            os.write(sys.stderr.fileno(), _IGNORED_LINE)
        except (OSError, ValueError, AttributeError):
            # Closed fd, a rebound/None sys.stderr — a diagnostic never
            # gets to kill the worker.
            pass

    global _interrupt_installed
    _interrupt_installed = True
    if sys.platform == "win32":
        signal.signal(signal.SIGBREAK, _on_interrupt)
    else:
        signal.signal(signal.SIGINT, _on_interrupt)


def _exception_payload(e: BaseException) -> dict:
    """The generic error body for failures outside a mapped type."""
    return {
        "error": {
            "message": f"{type(e).__name__}: {e}",
            "data": {
                "type": type(e).__name__,
                "traceback": traceback.format_exc(limit=20),
            },
        }
    }


def _invoke(
    entry: ExposedFn | None,
    fn_name: str,
    module_name: str,
    input: dict,
    on_start=None,
    regs: dict | None = None,
) -> dict:
    """Run one call against the resolved registry entry; build its payload.

    Shared by one-shot and serve mode — every boundary error is a payload,
    never a worker exit (a nonzero exit reads as WORKER_DIED upstream).
    `on_start` fires once, right before the raise window opens — serve
    mode uses it for the "started" frame. `regs` is the caller's blob
    handle map: serialize `payload` with the same map so a Blob's tag is
    the handle its `blobs` entry registers.
    """
    global _raise_active
    try:
        if entry is None:
            return {
                "error": {
                    "code": proto.FUNCTION_NOT_FOUND,
                    "message": f"{fn_name!r} is not @expose'd in {module_name}",
                    "data": {"type": "NotExposed"},
                }
            }
        fn = entry.fn
        if on_start is not None:
            on_start()
        _raise_active = True
        try:
            result = fn(**_json.coerce_kwargs(fn, input))
            # The declared return type decides tagging — a BigInt-
            # declared fn doing int arithmetic returns plain ints.
            result = _json.coerce_result(entry.signature.get("returns"), result)
        finally:
            _raise_active = False
        try:
            # dumps_collect runs check_wire inside AND materializes any
            # Blob: the file lands in the inherited PYLAIR_BLOB_DIR and
            # its registration rides home in `blobs` — the parent
            # forwards it as blob.register before the response, bound to
            # the task's owner (decision 0010 §4; a worker has no host
            # channel of its own). The caller serializes `payload` with
            # the SAME regs so the tag in `result` is the minted handle's.
            regs = regs if regs is not None else {}
            _json.dumps_collect(result, regs, _transport.MAX_DEPTH - 2)
        except _json.DepthExceeded:
            # A result deeper than the host's parser limit would arrive
            # as a malformed frame and kill the runtime — the parent maps
            # `code` through, so fail this call with the typed error.
            return {
                "error": {
                    "code": proto.LIMIT_EXCEEDED,
                    "message": "result exceeds the depth cap",
                    "data": {
                        "limit": "max_depth",
                        "max": _transport.MAX_DEPTH,
                    },
                }
            }
        except _json.BlobFallbackExceeded as e:
            return {
                "error": {
                    "code": proto.LIMIT_EXCEEDED,
                    "message": str(e),
                    "data": {"limit": "max_frame", "feature": "blob"},
                }
            }
        except (TypeError, ValueError) as e:
            # Same boundary the parent's _finish draws: a value the codec
            # can't hold is this call's error, never the worker's death.
            return {
                "error": {
                    "message": f"result is not wire-serializable: {e}",
                    "data": {"type": "SerializationError"},
                }
            }
        payload = {"result": result}
        if regs:
            payload["blobs"] = [b._register_params() for b in regs.values()]
        return payload
    except KeyboardInterrupt:
        # The interrupt landed inside the raise window — a distinct
        # result on the private channel so the parent can tell
        # interrupted from raced-to-completion (0008 §2). One-shot mode
        # never installs the handler — a stray KI there stays a plain
        # exception payload the parent already knows how to map.
        if not _interrupt_installed:
            raise
        return {"interrupted": True}
    except _json.InvalidParams as e:
        # Caller sent values the signature can't hold — a params error,
        # not a worker failure; the parent maps `code` through.
        return {
            "error": {
                "code": proto.INVALID_PARAMS,
                "message": str(e),
                "data": {"type": "InvalidParams"},
            }
        }
    except BaseException as e:  # noqa: BLE001 - boundary: report everything
        return _exception_payload(e)


def _serve_loop(module_name: str, fn_name: str, out_fd: int) -> int:
    """Warm-worker mode: import once, then serve framed calls until EOF.

    Request frames are {"op": "call", "input": {...}}; {"op": "shutdown"}
    or stdin EOF ends the loop with exit 0. One frame in, one frame out —
    the parent serializes calls per worker, so no ids are needed.
    {"op": "ready"} is the prewarm handshake (0008 §6): it can only be
    answered after the import block ran, so its response proves the
    module imported and the fn resolved.
    """
    import_error: dict | None = None
    try:
        # Resolve through the registry this import populates — the
        # function may live in any module the app imports and need not be
        # re-exported by name from the app module.
        importlib.import_module(module_name)
        entry = exposed_functions().get(fn_name)
    except BaseException as e:  # noqa: BLE001 - serve the failure as a payload
        entry = None
        import_error = _exception_payload(e)

    chan = _transport.FrameChannel(rfd=0, wfd=out_fd)
    while True:
        body = chan.read_frame()
        if body is None:
            return 0
        msg = _json.loads(body)
        if msg.get("op") == "shutdown":
            return 0
        if msg.get("op") == "ready":
            if import_error is not None:
                payload = {"ready": False, "error": import_error["error"]}
            elif entry is None:
                payload = {
                    "ready": False,
                    "error": {
                        "message": f"no function named {fn_name!r} in {module_name!r}"
                    },
                }
            else:
                payload = {"ready": True}
            chan.write_frame(_json.dumps(payload).encode())
            continue
        regs: dict = {}
        payload = (
            import_error
            if import_error is not None
            else _invoke(
                entry,
                fn_name,
                module_name,
                msg.get("input") or {},
                on_start=lambda: chan.write_frame(_STARTED),
                regs=regs,
            )
        )
        chan.write_frame(_json.dumps_collect(payload, regs).encode())


def main() -> int:
    _exit_with_parent()
    argv = [a for a in sys.argv[1:] if a != "--serve"]
    serve = len(argv) < len(sys.argv) - 1
    if len(argv) != 2:
        print(
            "usage: python -m pylair._worker [--serve] <module> <fn>",
            file=sys.stderr,
        )
        return 2
    module_name, fn_name = argv

    if sys.platform == "win32":
        import msvcrt

        msvcrt.setmode(0, os.O_BINARY)

    # Private result channel: dup the real stdout, then send fd 1 to stderr.
    out_fd = os.dup(1)
    os.dup2(2, 1)
    sys.stdout = sys.stderr
    if sys.platform == "win32":
        # Framed bodies carry a binary length prefix — text-mode CRLF
        # translation on the dup'd fd would corrupt a 0x0A length byte.
        msvcrt.setmode(out_fd, os.O_BINARY)

    if serve:
        # Before any user import: the interrupt handler must be live —
        # on Windows SIGBREAK's default disposition is terminate.
        _install_interrupt()
        return _serve_loop(module_name, fn_name, out_fd)

    try:
        # Resolve through the registry this import populates — the
        # function may live in any module the app imports and need not be
        # re-exported by name from the app module.
        importlib.import_module(module_name)
        entry = exposed_functions().get(fn_name)
        raw = sys.stdin.buffer.read()
        input = _json.loads(raw) if raw else {}
        regs: dict = {}
        payload = _invoke(entry, fn_name, module_name, input, regs=regs)
    except BaseException as e:  # noqa: BLE001 - boundary: report everything
        payload = _exception_payload(e)
        regs = {}

    os.write(out_fd, _json.dumps_collect(payload, regs).encode())
    return 0


if __name__ == "__main__":
    sys.exit(main())
