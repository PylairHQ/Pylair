"""End-to-end through the real spawned runtime: pydantic models in exposed
functions, wire forms in, aliased+computed shapes out."""

import pytest

pydantic = pytest.importorskip("pydantic")

from conftest import Runtime
from pylair import _protocol as proto


def call(rid, fn, task, input):
    return proto.request(rid, "call", {"fn": fn, "task": task, "input": input})


def reply_for(runtime, rid):
    return runtime.recv_until(lambda m: m.get("id") == rid, timeout=15)


WIRE = {
    "orderId": "o-1",
    "items": [{"name": "widget", "qty": 3}],
    "placed_at": "2026-01-01T10:30:00+00:00",
    "ident": "12345678-1234-5678-1234-567812345678",
}


def test_round_trip_through_real_runtime():
    with Runtime(module="testapp_pydantic") as r:
        hello = r.recv()
        names = [e["name"] for e in hello["params"]["exposes"]]
        assert "place_order" in names
        r.send(call(1, "place_order", 1, {"order": WIRE}))
        rep = reply_for(r, 1)
    assert "error" not in rep, rep.get("error")
    result = rep["result"]
    # by-alias out, computed field present, datetime back as ISO string
    assert result["orderId"] == "o-1"
    assert result["total"] == 3
    assert result["placed_at"].startswith("2026-01-01T10:30")
    assert result["ident"] == "12345678-1234-5678-1234-567812345678"


def test_invalid_params_structured_through_runtime():
    with Runtime(module="testapp_pydantic") as r:
        r.recv()
        bad = dict(WIRE, items=[{"name": "widget", "qty": "SECRET-WRONG"}])
        r.send(call(1, "place_order", 1, {"order": bad}))
        rep = reply_for(r, 1)
    err = rep["error"]
    assert err["code"] == proto.INVALID_PARAMS
    errors = err["data"]["errors"]
    assert errors[0]["path"] == "args.order.items[0].qty"
    assert errors[0]["expected"] == "int"
    assert errors[0]["received"] == "string"
    import json

    assert "SECRET-WRONG" not in json.dumps(err)


def test_stringly_typed_rejected_through_runtime():
    with Runtime(module="testapp_pydantic") as r:
        r.recv()
        bad = dict(WIRE, items=[{"name": "w", "qty": "3"}])
        r.send(call(1, "place_order", 1, {"order": bad}))
        rep = reply_for(r, 1)
    assert rep["error"]["code"] == proto.INVALID_PARAMS
