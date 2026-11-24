"""pylair.update: the pending/confirmed window (decision 0011 §4).

State comes from files under <app_data_dir>/updater-state — the tests
simulate the watchdog by writing update-ok themselves.
"""

import json
import time

import pytest

from pylair import update


@pytest.fixture()
def statedir(tmp_path, monkeypatch):
    monkeypatch.setenv("PYLAIR_APP_DATA_DIR", str(tmp_path))
    return tmp_path / "updater-state"


def pending(statedir):
    statedir.mkdir(parents=True, exist_ok=True)
    (statedir / "pending.json").write_text(json.dumps({"to": "1.1.0"}))


def test_state_none_without_env(monkeypatch):
    monkeypatch.delenv("PYLAIR_APP_DATA_DIR", raising=False)
    assert update.state() == "none"


def test_state_none_empty_dir(statedir):
    statedir.mkdir(parents=True)
    assert update.state() == "none"


def test_state_transitions(statedir):
    assert update.state() == "none"
    pending(statedir)
    assert update.state() == "pending"
    (statedir / "update-ok").write_text("")
    assert update.state() == "confirmed"


def test_confirmed_wins_over_pending(statedir):
    pending(statedir)
    (statedir / "update-ok").write_text("")
    assert update.state() == "confirmed"


def test_hook_fires_immediately_when_confirmed(statedir):
    pending(statedir)
    (statedir / "update-ok").write_text("")
    calls = []
    update.on_update_confirmed(lambda: calls.append(1))
    assert calls == [1]


def test_hook_fires_once_on_late_confirmation(statedir, monkeypatch):
    monkeypatch.setattr(update, "POLL_INTERVAL_S", 0.01)
    pending(statedir)
    calls = []
    update.on_update_confirmed(lambda: calls.append(1))
    assert calls == []
    (statedir / "update-ok").write_text("")
    for _ in range(200):
        if calls:
            break
        time.sleep(0.01)
    assert calls == [1]
    # No double-fire after the watcher exits.
    time.sleep(0.05)
    assert calls == [1]


def test_hook_dropped_on_rollback(statedir, monkeypatch):
    monkeypatch.setattr(update, "POLL_INTERVAL_S", 0.01)
    pending(statedir)
    calls = []
    update.on_update_confirmed(lambda: calls.append(1))
    # Watchdog restores the previous version: pending goes away, no
    # update-ok ever lands.
    (statedir / "pending.json").unlink()
    (statedir / "rollback-reason.txt").write_text("{}")
    time.sleep(0.1)
    assert calls == []
    assert update.state() == "none"
