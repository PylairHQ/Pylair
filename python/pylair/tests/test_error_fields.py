"""Wire-visible error fields: dev-gated tracebacks, function suggestions,
structured INVALID_PARAMS (docs/protocol-v1.md §8). Driven end-to-end
through the real spawned runtime — the same frames the host sees."""

import pytest

from conftest import Runtime
from pylair import _protocol as proto
from pylair._devtb import format_dev_traceback


def call(rid: int, fn: str, task: int, input: dict | None = None) -> dict:
    return proto.request(rid, "call", {"fn": fn, "task": task, "input": input or {}})


def reply_for(runtime, rid: int, timeout: float = 10.0) -> dict:
    return runtime.recv_until(lambda m: m.get("id") == rid, timeout)


# ---- the dev gate end-to-end through the real runtime ---------------------


def test_traceback_present_when_host_opts_in():
    """PYLAIR_DEV_TRACEBACKS=1 (what the host sets in debug builds): an
    exception in an exposed function carries the formatted traceback."""
    with Runtime(env={"PYLAIR_DEV_TRACEBACKS": "1"}) as r:
        r.recv()  # hello
        r.send(call(1, "boom", 1))
        rep = reply_for(r, 1)
    err = rep["error"]
    assert err["code"] == proto.INTERNAL
    tb = (err.get("data") or {}).get("traceback")
    assert tb is not None, f"no traceback in {err}"
    assert "ValueError: boom" in tb
    # app frames survive; pylair's own machinery frames are dropped
    assert "testapp" in tb
    assert "_tasks.py" not in tb


def test_traceback_absent_without_env(monkeypatch):
    """The release-build shape: no env var, no traceback — source paths
    and code must never reach end-user machines. The Runtime child
    inherits os.environ, so scrub the var there first."""
    monkeypatch.delenv("PYLAIR_DEV_TRACEBACKS", raising=False)
    with Runtime() as r:
        r.recv()
        r.send(call(1, "boom", 1))
        rep = reply_for(r, 1)
    assert "traceback" not in (rep["error"].get("data") or {})
    assert rep["error"]["data"]["type"] == "ValueError"


def test_unit_gate_without_env(monkeypatch):
    monkeypatch.delenv("PYLAIR_DEV_TRACEBACKS", raising=False)
    try:
        raise ValueError("x")
    except ValueError as e:
        assert format_dev_traceback(e) is None


def test_unit_gate_with_env(monkeypatch):
    monkeypatch.setenv("PYLAIR_DEV_TRACEBACKS", "1")
    try:
        raise ValueError("x")
    except ValueError as e:
        tb = format_dev_traceback(e)
    assert tb is not None and "ValueError: x" in tb


# ---- suggestions on function-not-found ------------------------------------


def test_no_such_function_close_match(runtime):
    runtime.recv()  # hello
    runtime.send(call(1, "ad", 1))  # close to "add"
    rep = reply_for(runtime, 1)
    err = rep["error"]
    assert err["code"] == proto.FUNCTION_NOT_FOUND
    sugg = (err.get("data") or {}).get("suggestions")
    assert sugg and "add" in sugg
    assert len(sugg) <= 3
    assert err["message"].endswith("did you mean 'add'?")


def test_no_such_function_no_match(runtime):
    runtime.recv()
    runtime.send(call(1, "zzzzqqqq", 1))
    rep = reply_for(runtime, 1)
    err = rep["error"]
    assert err["code"] == proto.FUNCTION_NOT_FOUND
    assert "did you mean" not in err["message"]
    assert not (err.get("data") or {}).get("suggestions")


# ---- structured INVALID_PARAMS --------------------------------------------


def test_invalid_params_wrong_type(runtime):
    runtime.recv()
    runtime.send(call(1, "add", 1, {"a": "x", "b": 2}))
    rep = reply_for(runtime, 1)
    err = rep["error"]
    assert err["code"] == proto.INVALID_PARAMS
    errors = (err.get("data") or {}).get("errors")
    assert errors, f"no structured errors in {err}"
    first = errors[0]
    assert first["path"] == "args.a"
    assert first["expected"] == "int"
    assert first["received"] == "string"


def test_invalid_params_missing_required(runtime):
    runtime.recv()
    runtime.send(call(1, "add", 1, {"a": 1}))
    rep = reply_for(runtime, 1)
    errors = rep["error"]["data"]["errors"]
    assert errors[0] == {"path": "args.b", "expected": "int", "received": "missing"}


def test_invalid_params_unknown_keyword(runtime):
    runtime.recv()
    runtime.send(call(1, "add", 1, {"a": 1, "b": 2, "secret": "user-value-xyz"}))
    rep = reply_for(runtime, 1)
    err = rep["error"]
    errors = err["data"]["errors"]
    assert errors[0]["path"] == "args.secret"
    assert errors[0]["expected"] == "absent"
    # never the value itself — only its JSON type
    import json

    assert "user-value-xyz" not in json.dumps(err["data"])


def test_invalid_params_nested_path_unit():
    """Nested dataclass/list paths: the deepest leaf is named exactly."""
    import dataclasses

    from pylair._json import InvalidParams, coerce_kwargs

    @dataclasses.dataclass
    class Item:
        name: str

    def fn(items: list[Item]) -> None:
        pass

    with pytest.raises(InvalidParams) as ei:
        coerce_kwargs(fn, {"items": [{"name": "ok"}, {"name": 5}]})
    assert ei.value.errors[0]["path"] == "args.items[1].name"
    assert ei.value.errors[0]["expected"] == "str"
    assert ei.value.errors[0]["received"] == "number"
