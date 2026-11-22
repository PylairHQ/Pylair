"""m9-models spike: cross-process byte-range lock proof.

Proves the properties `pylair.models` relies on for `<name>.lock`:
  1. a second process's non-blocking lock attempt fails while held,
  2. it acquires once the holder releases,
  3. a KILLED holder releases the lock (handle death) — no stale locks.

Windows uses msvcrt.locking; POSIX uses fcntl.flock. Both are kernel
locks tied to the open fd, which is what makes property 3 free.

Measured on Windows 11 (msvcrt): held -> child EACCES(13); released ->
acquired; terminated holder -> acquired. fcntl.flock semantics are the
documented POSIX equivalent — same properties, proven on the POSIX legs
of the model-manager test plan when implemented.
"""

import os
import subprocess
import sys
import tempfile
import time

if os.name == "nt":
    import msvcrt

    def lock(fd):
        os.lseek(fd, 0, os.SEEK_SET)
        msvcrt.locking(fd, msvcrt.LK_NBLCK, 1)

    def unlock(fd):
        os.lseek(fd, 0, os.SEEK_SET)
        msvcrt.locking(fd, msvcrt.LK_UNLCK, 1)
else:
    import fcntl

    def lock(fd):
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)

    def unlock(fd):
        fcntl.flock(fd, fcntl.LOCK_UN)


CHILD = """
import os, sys
if os.name == 'nt':
    import msvcrt
    def lock(fd):
        os.lseek(fd, 0, os.SEEK_SET); msvcrt.locking(fd, msvcrt.LK_NBLCK, 1)
else:
    import fcntl
    def lock(fd):
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
fd = os.open(sys.argv[1], os.O_RDWR)
try:
    lock(fd)
except OSError as e:
    print('child: blocked, errno', e.errno); sys.exit(0)
print('child: ACQUIRED')
"""

HOLDER = """
import os, sys, time
if os.name == 'nt':
    import msvcrt
    def lock(fd):
        os.lseek(fd, 0, os.SEEK_SET); msvcrt.locking(fd, msvcrt.LK_NBLCK, 1)
else:
    import fcntl
    def lock(fd):
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
fd = os.open(sys.argv[1], os.O_RDWR)
lock(fd)
print('holding', flush=True)
time.sleep(60)
"""


def main() -> None:
    d = tempfile.mkdtemp()
    path = os.path.join(d, "m.lock")
    open(path, "wb").close()
    fd = os.open(path, os.O_RDWR)

    lock(fd)
    r = subprocess.run(
        [sys.executable, "-c", CHILD, path],
        capture_output=True,
        text=True,
        check=False,
    )
    print("held:", r.stdout.strip(), "rc", r.returncode)

    unlock(fd)
    r = subprocess.run(
        [sys.executable, "-c", CHILD, path],
        capture_output=True,
        text=True,
        check=False,
    )
    print("free:", r.stdout.strip(), "rc", r.returncode)

    holder = subprocess.Popen(
        [sys.executable, "-c", HOLDER, path],
        stdout=subprocess.PIPE,
        text=True,
    )
    assert holder.stdout is not None
    holder.stdout.readline()
    holder.terminate()
    holder.wait(timeout=10)
    try:
        lock(fd)
        print("after holder death: ACQUIRED")
    except OSError as e:
        print("after holder death: still blocked", e.errno)
    os.close(fd)


if __name__ == "__main__":
    main()
