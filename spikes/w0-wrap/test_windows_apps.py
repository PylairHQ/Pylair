#!/usr/bin/env python3
"""W0b Windows app-level probes against the BUILT spike exe.

For each app fixture: launch the real exe, time app-process-start to
READY (the launcher's own HTTP probe passing), then for fastapi wait for
the webview's IPC probe to land - the remote page POSTing its invoke()
results back is the only signal that WebView2 actually navigated to the
wrapped URL. The app is then killed by PID and the venv-tree pids are
asserted gone (Job Object KILL_ON_JOB_CLOSE).

Usage: python test_windows_apps.py <path-to-w0-wrap.exe>
"""

import ctypes
import json
import os
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
READY_FILE = os.path.join(tempfile.gettempdir(), "w0-ready.txt")
PID_FILE = os.path.join(tempfile.gettempdir(), "w0-pids.txt")
IPC_FILE = os.path.join(tempfile.gettempdir(), "w0-ipc.json")

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


def wait_file(path, timeout, needle=None):
    t0 = time.time()
    while time.time() - t0 < timeout:
        try:
            data = open(path, encoding="utf-8").read()
            if data and (needle is None or needle in data):
                return time.time() - t0, data
        except FileNotFoundError:
            pass
        time.sleep(0.25)
    return None, None


def run_app(exe, app, ready_timeout, ipc_probe=False, settle=8.0):
    for f in (READY_FILE, PID_FILE, IPC_FILE):
        if os.path.exists(f):
            os.remove(f)
    env = {
        **os.environ,
        "W0_APP": app,
        "W0_READY_FILE": READY_FILE,
        "W0_PID_FILE": PID_FILE,
    }
    t0 = time.time()
    proc = subprocess.Popen([exe], env=env)
    try:
        dt, data = wait_file(READY_FILE, ready_timeout)
        if dt is None:
            print(f"[{app}] no READY within {ready_timeout}s - FAIL")
            return None
        print(f"[{app}] READY in {dt:.1f}s: {data.strip().splitlines()[-1]}")

        if ipc_probe:
            dt2, ipc = wait_file(IPC_FILE, 30)
            if dt2 is None:
                print(f"[{app}] remote page never POSTed /ipc-probe - FAIL")
            else:
                rep = json.loads(ipc)
                print(
                    f"[{app}] ipc probe +{dt2:.1f}s: "
                    f"__TAURI__={rep.get('tauri')} __TAURI_INTERNALS__={rep.get('internals')} "
                    f"invoke={rep.get('invoke')}"
                )
                for name, res in rep.get("attempts", {}).items():
                    print(f"    {name}: {res}")
                bad = [n for n, r in rep.get("attempts", {}).items() if r == "RESOLVED"]
                if bad:
                    print(f"[{app}] RESOLVED (ipc leak!): {bad} - FAIL")

        # give the webview a moment, then hard-kill the app by PID
        time.sleep(settle)
        pids = []
        try:
            for line in open(PID_FILE):
                tag, _, n = line.partition(" ")
                if tag in ("launcher", "server"):
                    pids.append(int(n))
        except FileNotFoundError:
            pass
        subprocess.run(["taskkill", "/F", "/PID", str(proc.pid)], capture_output=True)
        proc.wait(timeout=15)
        time.sleep(2.0)
        survivors = [p for p in pids if alive(p)]
        status = "clean" if not survivors else f"SURVIVORS {survivors}"
        print(f"[{app}] app killed; venv tree {status}")
        return dt
    finally:
        if proc.poll() is None:
            subprocess.run(["taskkill", "/F", "/PID", str(proc.pid)], capture_output=True)
            proc.wait(timeout=15)


def main():
    exe = sys.argv[1]
    results = {}
    results["fastapi"] = run_app(exe, "fastapi", ready_timeout=60, ipc_probe=True)
    results["gradio"] = run_app(exe, "gradio", ready_timeout=120)
    results["streamlit"] = run_app(exe, "streamlit", ready_timeout=120)
    print("\nlaunch-to-ready:")
    for app, dt in results.items():
        print(f"  {app}: {dt:.1f}s" if dt else f"  {app}: FAILED")


if __name__ == "__main__":
    main()
