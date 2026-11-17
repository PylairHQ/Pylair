#!/usr/bin/env python3
"""W0b Windows containment proof, run against the BUILT spike exe.

Automates the kill rows of the matrix that don't need eyeballs:

  1. launch the app (real supervisor, real Job Object) and read the
     READY url + launcher/server/orphan pids from the env-named files
  2. taskkill /F /PID <server> - the server dies, the launcher exits,
     and the supervisor's Job Object closes on monitor end. If the
     orphan grandchild dies too, the JOB did it: the launcher's own
     taskkill /T targets the (already dead) server pid, a no-op.
  3. supervisor restarts -> second READY -> new pids
  4. taskkill /F /PID <w0-wrap.exe> - hard kill of the app. Its open
     Job-handle dies with the process -> KILL_ON_JOB_CLOSE fires ->
     the whole new tree must be gone. This is the "no orphans ever"
     property a POSIX killpg approximates.

Pid liveness uses OpenProcess/GetExitCodeProcess - never os.kill on
Windows (any non-console signal is TerminateProcess there).

Usage: python test_windows_containment.py <path-to-w0-wrap.exe> [app]
       app defaults to "orphan" (the grandchild fixture).
"""

import ctypes
import os
import subprocess
import sys
import tempfile
import time
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
READY_FILE = os.path.join(tempfile.gettempdir(), "w0-ready.txt")
PID_FILE = os.path.join(tempfile.gettempdir(), "w0-pids.txt")
ORPHAN_FILE = os.path.join(tempfile.gettempdir(), "w0-orphan.txt")

k32 = ctypes.windll.kernel32


def alive(pid):
    h = k32.OpenProcess(0x1000, False, pid)  # PROCESS_QUERY_LIMITED_INFORMATION
    if not h:
        return False
    try:
        code = ctypes.c_ulong()
        if not k32.GetExitCodeProcess(h, ctypes.byref(code)):
            return False
        return code.value == 259  # STILL_ACTIVE
    finally:
        k32.CloseHandle(h)


def kill(pid):
    """taskkill a single pid, no /T - the Job Object must do the rest."""
    subprocess.run(["taskkill", "/F", "/PID", str(pid)], capture_output=True)


def read_pids():
    """Return {'launcher': pid, 'server': pid} from the LAST two pid lines."""
    launcher = server = None
    for line in open(PID_FILE):
        tag, _, n = line.partition(" ")
        if tag == "launcher":
            launcher = int(n)
        elif tag == "server":
            server = int(n)
    return launcher, server


def orphan_pid():
    try:
        with open(ORPHAN_FILE) as fh:
            first = fh.readline()
        return int(first.removeprefix("pid=").strip())
    except Exception:
        return None


def wait_orphan(not_pid=None, timeout=15):
    """Wait until the orphan file names a live pid (new one after restart).

    The grandchild boots asynchronously: a fast server can answer the
    READY probe before the orphan's first heartbeat write lands."""
    t0 = time.time()
    while time.time() - t0 < timeout:
        p = orphan_pid()
        if p and p != not_pid:
            return p
        time.sleep(0.2)
    return None


def wait_ready_line(count, timeout=90):
    """Wait until READY_FILE has at least `count` urls; return the last one."""
    t0 = time.time()
    while time.time() - t0 < timeout:
        try:
            urls = [u.strip() for u in open(READY_FILE) if u.strip()]
            if len(urls) >= count:
                return urls[-1]
        except FileNotFoundError:
            pass
        time.sleep(0.25)
    raise AssertionError(f"no READY line #{count} within {timeout}s")


def get(url):
    return urllib.request.urlopen(url + "/", timeout=5).read()


def main():
    exe = sys.argv[1]
    app = sys.argv[2] if len(sys.argv) > 2 else "orphan"
    for f in (READY_FILE, PID_FILE, ORPHAN_FILE):
        if os.path.exists(f):
            os.remove(f)

    env = {
        **os.environ,
        "W0_APP": app,
        "W0_READY_FILE": READY_FILE,
        "W0_PID_FILE": PID_FILE,
    }
    t0 = time.time()
    app_proc = subprocess.Popen([exe], env=env)

    # 1. first READY: page data served by the wrapped server
    url = wait_ready_line(1)
    ready_s = time.time() - t0
    body = get(url)
    assert b"W0-WRAP-MARKER" in body, f"marker missing at {url}"
    print(f"[1] ready in {ready_s:.1f}s at {url}; GET / serves the marker")

    l1, s1 = read_pids()
    o1 = wait_orphan()
    print(f"    pids: app={app_proc.pid} launcher={l1} server={s1} orphan={o1}")
    assert l1 and s1 and alive(l1) and alive(s1)
    assert o1 and alive(o1), "orphan grandchild never started"

    # 2. kill the server only. Launcher's taskkill /T then targets a dead
    #    pid and cannot account for the orphan's death - the Job Object
    #    closing on the dead launcher's monitor end must.
    kill(s1)
    url2 = wait_ready_line(2, timeout=120)
    l2, s2 = read_pids()
    print(f"[2] killed server {s1}; supervisor restarted: {url2} (launcher={l2} server={s2})")

    time.sleep(1.0)
    o2 = wait_orphan(not_pid=o1)
    dead = [p for p in (l1, s1, o1) if alive(p)]
    assert not dead, f"old tree still alive: {dead}"
    assert o2 != o1 and alive(o2), f"no new orphan: {o2}"
    print(f"    job object reaped launcher={l1} orphan={o1} without taskkill (server already dead)")

    # 3. hard-kill the app itself: the Job handle dies with the process,
    #    KILL_ON_JOB_CLOSE must reap launcher + server + orphan.
    kill(app_proc.pid)
    app_proc.wait(timeout=15)
    time.sleep(2.0)
    dead = [p for p in (l2, s2, o2) if alive(p)]
    assert not dead, f"survivors after hard kill of the app: {dead}"
    print(f"[3] hard-killed the app; launcher={l2} server={s2} orphan={o2} all gone")
    print("PASS: the Job Object reaps the whole wrap tree on every exit path")


if __name__ == "__main__":
    main()
