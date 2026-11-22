"""PYLAIR_APP_DATA_DIR handoff (decision 0009 §2, model manager PR 1).

The host sets the app's data dir in the spawn environment so the
runtime — and the isolated/warm workers it spawns — can reach per-user
storage (models, caches) that lives outside the install dir. These
tests drive a real `python -m pylair` subprocess with the variable set
and prove both the runtime fn and a warm worker see the same value;
the Rust side (`set_app_data_dir_env`) is covered in src/tests.rs.
"""

from __future__ import annotations

from conftest import Runtime
from pylair import _protocol as proto


def call(rid: int, fn: str, task: int) -> dict:
    return proto.request(rid, "call", {"fn": fn, "task": task, "input": {}})


def test_runtime_sees_app_data_dir(tmp_path):
    rt = Runtime(env={"PYLAIR_APP_DATA_DIR": str(tmp_path)})
    try:
        rt.recv()  # hello
        rt.send(call(1, "app_data_dir_env", task=1))
        assert rt.recv()["result"] == str(tmp_path)
    finally:
        rt.kill()
        rt.proc.wait(timeout=10)


def test_warm_worker_inherits_app_data_dir(tmp_path):
    """Workers spawn from the runtime — the variable must reach them
    through the inherited environment, no per-worker plumbing."""
    rt = Runtime(env={"PYLAIR_APP_DATA_DIR": str(tmp_path)})
    try:
        rt.recv()  # hello
        rt.send(call(1, "warm_app_data_dir_env", task=1))
        assert rt.recv()["result"] == str(tmp_path)
    finally:
        rt.kill()
        rt.proc.wait(timeout=10)


def test_empty_value_passes_through(tmp_path):
    """Env values pass through verbatim — an empty string is not
    conflated with unset. (Fully unset would depend on the dev
    machine's own env, so the deterministic absence case lives on the
    Rust side: set_app_data_dir_env simply pushes nothing.)"""
    rt = Runtime(env={"PYLAIR_APP_DATA_DIR": ""})
    try:
        rt.recv()
        rt.send(call(1, "app_data_dir_env", task=1))
        assert rt.recv()["result"] == "", "an empty value passes through as-is"
    finally:
        rt.kill()
        rt.proc.wait(timeout=10)
