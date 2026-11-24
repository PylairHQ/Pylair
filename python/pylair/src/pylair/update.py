"""Update-window introspection (decision 0011 section 4).

When an update is installed and the app relaunches, the new version's
first boot runs *pending verification*: the watchdog has not yet
confirmed the app is healthy and may still roll it back. Apps that
need to migrate state on an update must not do so on the pending
boot — they run read-only first and migrate once the watchdog writes
``update-ok``.

``state()`` reports where the app stands:

- ``"none"`` — no update activity (also the answer on hosts without
  update support).
- ``"pending"`` — this boot is a pending update's verification
  window; a rollback is still possible.
- ``"confirmed"`` — the watchdog wrote ``update-ok``; migrations are
  safe.

``on_update_confirmed(fn)`` runs ``fn`` exactly once when the state
reaches ``"confirmed"`` — immediately if it already is — so apps can
defer schema/state migration until the update is proven.

The host and watchdog share ``<app_data_dir>/updater-state/`` — the
app data dir arrives via ``PYLAIR_APP_DATA_DIR``, already pushed to
the child environment by the runtime spec (the same channel 0009's
model store uses). Nothing here talks to the host over IPC; the
files are the contract.
"""

from __future__ import annotations

import os
import sys
import threading
import time
from collections.abc import Callable
from pathlib import Path
from typing import Literal

UpdateState = Literal["none", "pending", "confirmed"]

_STATE_SUBDIR = "updater-state"
_PENDING_FILE = "pending.json"
_UPDATE_OK_FILE = "update-ok"

# Poll cadence for the confirmation watcher. A pending window can last
# minutes; the daemon thread costs one stat per interval.
POLL_INTERVAL_S = 1.0


def _state_dir() -> Path | None:
    base = os.environ.get("PYLAIR_APP_DATA_DIR")
    if not base:
        return None
    return Path(base) / _STATE_SUBDIR


def state() -> UpdateState:
    """Where this boot stands in the update verification window."""
    d = _state_dir()
    if d is None:
        return "none"
    if (d / _UPDATE_OK_FILE).exists():
        return "confirmed"
    if (d / _PENDING_FILE).exists():
        return "pending"
    return "none"


_lock = threading.Lock()
_fns: list[Callable[[], None]] = []
_watching = False


def on_update_confirmed(fn: Callable[[], None]) -> None:
    """Run ``fn`` once when this update is confirmed healthy.

    Fires inline if the state is already ``"confirmed"``. If a pending
    window ends without confirmation (rolled back, pending cleared)
    registered callbacks never run — there is nothing to migrate on a
    restored build.
    """
    global _watching
    if state() == "confirmed":
        fn()
        return
    with _lock:
        _fns.append(fn)
        if not _watching:
            _watching = True
            threading.Thread(target=_watch, name="pylair-update", daemon=True).start()


def _watch() -> None:
    global _watching
    while True:
        s = state()
        if s == "confirmed":
            with _lock:
                fns, _fns[:] = list(_fns), []
                _watching = False
            for f in fns:
                try:
                    f()
                except Exception as e:  # noqa: BLE001 - a bad migration hook
                    # must not kill the watcher or the other hooks
                    print(
                        f"[pylair] update: on_update_confirmed hook failed: {e}",
                        file=sys.stderr,
                    )
            return
        if s != "pending":
            with _lock:
                _watching = False
            return
        time.sleep(POLL_INTERVAL_S)
