"""Frame transport: stdout hygiene + length-prefixed frame I/O.

Hygiene (protocol §1): at startup, before any third-party import can run,
we dup the real stdout fd into a private frame channel and point fd 1 at
stderr. After that, `print()` and native fd-1 writes land in the log
stream and can never corrupt frames. On Windows both channels are put in
binary mode — CRLF translation would corrupt length prefixes.
"""

from __future__ import annotations

import os
import struct
import sys

MAX_FRAME = int(os.environ.get("PYLAIR_MAX_FRAME", str(16 * 1024 * 1024)))
# The host parses frames with serde_json, which refuses JSON nested
# deeper than ~128 — a deeper result must fail ITS call before it can be
# sent as an unparseable frame and kill the runtime (protocol §2).
MAX_DEPTH = int(os.environ.get("PYLAIR_MAX_DEPTH", "100"))
_LEN = struct.Struct(">I")


class FrameChannel:
    """Reads/writes length-prefixed JSON frames on dedicated fds."""

    def __init__(self, rfd: int, wfd: int) -> None:
        self._r = os.fdopen(rfd, "rb", buffering=0)
        self._w = os.fdopen(wfd, "wb", buffering=0)

    def read_frame(self) -> bytes | None:
        """Next frame body, or None at EOF (host gone)."""
        hdr = self._read_exactly(_LEN.size)
        if hdr is None:
            return None
        (n,) = _LEN.unpack(hdr)
        if n > MAX_FRAME:
            raise FrameTooLarge(n)
        return self._read_exactly(n)

    def write_frame(self, body: bytes) -> None:
        """Write an already-serialized frame body.

        Serialization happens on the caller's thread (_json.dumps in
        Server._send) so a TypeError/ValueError can become INTERNAL for
        that one call — a dead writer would kill the runtime (§2).

        The loop is load-bearing: FileIO.write is one raw write() and a
        signal interrupting a large transfer returns short progress —
        without the retry the frame truncates and the reader waits on
        bytes that never come (the warm SIGINT hammer proved it).
        """
        view = memoryview(_LEN.pack(len(body)) + body)
        while view:
            n = self._w.write(view)
            if not n:
                raise OSError("frame channel write returned 0 bytes")
            view = view[n:]

    def _read_exactly(self, n: int) -> bytes | None:
        out = bytearray()
        while len(out) < n:
            chunk = self._r.read(n - len(out))
            if not chunk:
                return None
            out += chunk
        return bytes(out)

    def close_write(self) -> None:
        try:
            self._w.close()
        except OSError:
            pass


class FrameTooLarge(Exception):
    def __init__(self, n: int) -> None:
        super().__init__(f"frame length {n} exceeds MAX_FRAME {MAX_FRAME}")


def open_frame_channel() -> FrameChannel:
    """Perform stdout hygiene and return the frame channel.

    MUST run before importing any user/third-party module that might
    write to stdout. Idempotent per process (the runtime entry point
    calls it exactly once).
    """
    if sys.platform == "win32":
        import msvcrt

        msvcrt.setmode(0, os.O_BINARY)
        msvcrt.setmode(1, os.O_BINARY)

    frame_fd = os.dup(1)  # the real stdout -> private frame channel
    os.dup2(2, 1)  # fd 1 now points at stderr; stray writes go to logs
    sys.stdout = sys.stderr  # belt and suspenders for print()

    return FrameChannel(rfd=0, wfd=frame_fd)
