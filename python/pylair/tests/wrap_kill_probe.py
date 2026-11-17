"""Helper for test_wrap.py: run `kill_tree` against a server whose grandchild
is orphaned by the server's own exit, with the ordering forced.

    python wrap_kill_probe.py PIDFILE

The server dies at once on SIGTERM and its grandchild ignores SIGTERM. The
helper makes the second tree scan wait until the server has really exited,
so the scan that decides whether to escalate always runs after the
grandchild was reparented to init (or a subreaper), where no descendant walk
can see it. Prints one JSON line: the grandchild pid, and whether a plain
descendant walk could still see it at that point (the precondition).
"""

import json
import os
import sys
import time
from pathlib import Path

from pylair.wrap import _launcher as L

SERVER = str(Path(__file__).with_name("wrap_server.py"))
pidfile = sys.argv[1]
proc = L.spawn_server(
    [sys.executable, SERVER, "0", "--never-listen", "--grandchild", pidfile],
    dict(os.environ),
)
pid_path = Path(pidfile)
while not (pid_path.exists() and pid_path.read_text().strip()):
    time.sleep(0.05)
gpid = int(pid_path.read_text())

real_scan = L._scan
calls = []
invisible = []


def scan(known):
    calls.append(1)
    if len(calls) == 2:
        proc.wait()  # the server has exited on SIGTERM: grandchild orphaned
        invisible.append(gpid not in real_scan({})[0])
    return real_scan(known)


L._scan = scan
L.kill_tree(proc, grace=2.0)
print(json.dumps({"gpid": gpid, "orphan_invisible_to_a_descendant_walk": invisible[0]}))
sys.stdout.flush()
os._exit(0)
