"""Q8: the matrix cells q7 left open, and the re-serialization cost.

1. strict-python behavior when input is ALREADY the right object type —
   what actually happens at pylair validation time, because object_hook
   has already decoded $b64 -> bytes and $bigint -> BigInt before validate.
2. timedelta's accepted forms in both strict modes.
3. the cost of re-serializing decoded objects back to JSON text for
   model_validate_json, vs validating the objects directly.

Run: uv run python q8_decoded_and_cost.py
"""

import enum
import json
import sys
import timeit
import uuid
from datetime import datetime, timedelta, timezone
from decimal import Decimal
from pathlib import Path

from pydantic import BaseModel, ValidationError

sys.path.insert(0, "../../python/pylair/src")
from pylair._json import BigInt  # noqa: E402


def verdict(fn) -> str:
    try:
        return "OK     " + repr(fn())[:50]
    except ValidationError as e:
        return "REJECT " + "; ".join(
            f"{'.'.join(str(x) for x in er['loc'])}:{er['type']}" for er in e.errors()
        )[:60]
    except Exception as e:
        return f"ERROR  {type(e).__name__}: {str(e)[:60]}"


def model_for(ann):
    class M(BaseModel):
        v: ann

    return M


print("== 1. input already the right OBJECT type (pylair's reality at validate time) ==")
print("   object_hook decodes $b64/$bigint at PARSE time — validation never")
print("   sees tags or json text, it sees python objects.")
for label, ann, obj in [
    ("bytes object", bytes, b"hello"),
    ("BigInt object (int subclass?)", int, BigInt(2**70)),
    ("datetime object", datetime, datetime.now(timezone.utc)),
    ("UUID object", uuid.UUID, uuid.uuid4()),
    ("Decimal object", Decimal, Decimal("3.14")),
    ("tuple object", tuple[int, str], (1, "a")),
    ("timedelta object", timedelta, timedelta(seconds=5)),
    ("Path object", Path, Path("/tmp")),
]:
    M = model_for(ann)
    r = verdict(lambda: M.model_validate({"v": obj}, strict=True).v)
    print(f"  strict-python {label:32} {r}")

print()
print("   (isinstance check — does BigInt even satisfy 'int' structurally)")
print("   BigInt is int subclass:", issubclass(BigInt, int))

print()
print("== 2. timedelta forms — what CAN carry it ==")
M = model_for(timedelta)
for label, obj, raw in [
    ("seconds float", 3600.5, json.dumps({"v": 3600.5})),
    ("seconds int", 3600, json.dumps({"v": 3600})),
    ("ISO-8601 duration", "PT1H0.5S", json.dumps({"v": "PT1H0.5S"})),
    ("timedelta object", timedelta(seconds=3600.5), None),
]:
    a = verdict(lambda: M.model_validate({"v": obj}, strict=True).v)
    b = verdict(lambda: M.model_validate_json(raw, strict=True).v) if raw else "(no json form)"
    print(f"  {label:22} strict-python: {a:38} strict-json: {b}")

print()
print("== 3. bytes through strict-json is WRONG, not just awkward ==")
M = model_for(bytes)
print("  b64 'aGVsbG8=' via strict-json yields:", repr(M.model_validate_json(json.dumps({"v": "aGVsbG8="}), strict=True).v))
print("  i.e. the base64 TEXT as bytes — the decoded payload is unrecoverable")

print()
print("== 4. re-serialization cost: decoded objects -> json text -> validate_json ==")
print("   vs validating the objects directly (strict python + pre-coerce)")


class ItemModel(BaseModel):
    sku: str
    price: float
    qty: int


class OrderModel(BaseModel):
    order_id: str
    items: list[ItemModel]
    placed_at: datetime


wire_decoded = {
    "order_id": "o-123",
    "items": [{"sku": f"sku-{i}", "price": 9.99, "qty": i} for i in range(10)],
    "placed_at": "2026-01-01T00:00:00+00:00",  # as the wire carries it
}

N = 4000
t = timeit.repeat(lambda: OrderModel.model_validate(wire_decoded, strict=False), number=N, repeat=5)
print(f"  reference lax validate       : {min(t) / N * 1e6:7.2f} us/call")


def pre_coerce_order(d):
    # minimal prototype: the one datetime path in this model
    d = dict(d)
    d["placed_at"] = datetime.fromisoformat(d["placed_at"])
    return d


t = timeit.repeat(lambda: OrderModel.model_validate(pre_coerce_order(wire_decoded), strict=True), number=N, repeat=5)
print(f"  strict python + pre-coerce   : {min(t) / N * 1e6:7.2f} us/call")


def roundtrip(d):
    return OrderModel.model_validate_json(json.dumps(d), strict=True)


t = timeit.repeat(lambda: roundtrip(wire_decoded), number=N, repeat=5)
print(f"  strict json (re-serialized)  : {min(t) / N * 1e6:7.2f} us/call")

print()
print("== 5. the bytes/bigint re-serialization wall ==")
try:
    json.dumps({"v": b"hello"})
    print("  json.dumps(bytes): worked?!")
except TypeError as e:
    print(f"  json.dumps(bytes) FAILS: {e}")
    print("  -> strict-json validation REQUIRES a pre-pass for bytes anyway,")
    print("     and any string encoding of bytes loses the payload (sec 3).")
try:
    print("  json.dumps(BigInt)      :", json.dumps({"v": BigInt(2**70)})[:40], "(works, server-side)")
except TypeError as e:
    print(f"  json.dumps(BigInt) FAILS: {e}")
