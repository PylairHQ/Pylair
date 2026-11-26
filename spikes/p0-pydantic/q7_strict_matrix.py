"""Q7: the full conversion matrix the review asked for.

Per type: does its JSON-native wire form validate under
  (a) strict Python mode    — model_validate(obj, strict=True)
  (b) strict JSON mode      — model_validate_json(raw, strict=True)
  (c) strict Python + pre-coercion (pylair converts the type first)

Plus the primitive bug-hiding cases ("1" for int etc.) under (a)/(b).

Run: uv run python q7_strict_matrix.py
"""

import enum
import json
import uuid
from datetime import date, datetime, time, timedelta, timezone
from decimal import Decimal
from pathlib import Path

from pydantic import BaseModel, ValidationError


def section(t):
    print(f"\n{'=' * 72}\n== {t}\n{'=' * 72}")


def verdict(fn) -> str:
    try:
        return "OK     " + repr(fn())[:44]
    except ValidationError as e:
        return "REJECT " + "; ".join(
            f"{'.'.join(str(x) for x in er['loc'])}:{er['type']}" for er in e.errors()
        )[:60]
    except Exception as e:
        return f"ERROR  {type(e).__name__}: {str(e)[:50]}"


# JSON-native wire forms — what can actually arrive inside a pylair frame
# (parsed JSON plus already-decoded bytes/BigInt; see _json._object_hook)
CASES = [
    # (label, annotation, json_native_wire_value)
    ("int", int, 5),
    ("str", str, "hi"),
    ("bool", bool, True),
    ("float", float, 1.5),
    ("float<-int", float, 2),  # JSON doesn't distinguish — must pass
    ("datetime", datetime, "2026-01-01T00:00:00+00:00"),
    ("date", date, "2026-01-01"),
    ("time", time, "13:45:00"),
    ("timedelta", timedelta, 3600.5),  # seconds — pydantic's own json form
    ("UUID", uuid.UUID, "12345678-1234-5678-1234-567812345678"),
    ("Decimal", Decimal, "3.14"),  # decimal-as-string is lossless json form
    ("Decimal-num", Decimal, 3.14),  # as a json number
    ("Enum-by-value", enum.Enum("C", [("RED", "red"), ("BLUE", "blue")]).__class__ if False else None, None),  # placeholder, replaced below
    ("tuple", tuple[int, str], [1, "a"]),  # arrays carry tuples on the wire
    ("set", set[int], [1, 2, 3]),
    ("frozenset", frozenset[int], [1, 2, 3]),
    ("bytes", bytes, "aGVsbG8="),  # base64 — or utf8? pydantic decides
    ("bytes-utf8", bytes, "hello"),
    ("Path", Path, "/tmp/x"),
]


class Color(enum.Enum):
    RED = "red"
    BLUE = "blue"


CASES[12] = ("Enum-by-value", Color, "red")


def model_for(ann):
    class M(BaseModel):
        v: ann

    return M


def pre_coerce(ann, value):
    """pylair's proposed pre-pass: convert the wire form to the object
    the strict python validator expects, at exactly the annotated paths."""
    if ann is datetime and isinstance(value, str):
        return datetime.fromisoformat(value)
    if ann is date and isinstance(value, str):
        return date.fromisoformat(value)
    if ann is time and isinstance(value, str):
        return time.fromisoformat(value)
    if ann is timedelta and isinstance(value, (int, float)):
        return timedelta(seconds=value)
    if ann is uuid.UUID and isinstance(value, str):
        return uuid.UUID(value)
    if ann is Decimal and isinstance(value, (str, int, float)):
        return Decimal(str(value))
    if ann is bytes and isinstance(value, str):
        import base64

        return base64.b64decode(value)  # pylair's $b64 already decoded
    if ann is Path and isinstance(value, str):
        return Path(value)
    if isinstance(ann, type) and issubclass(ann, enum.Enum) and not isinstance(value, ann):
        return ann(value)
    return value


section("the matrix — JSON-native wire form per type, three modes")
print(f"{'type':16} {'(a) strict-python':30} {'(b) strict-json':30} {'(c) strict+pre':30}")
for label, ann, wire in CASES:
    if ann is None:
        continue
    M = model_for(ann)
    # (a) strict python mode on the decoded wire object
    a = verdict(lambda: M.model_validate({"v": wire}, strict=True).v)
    # (b) strict json mode — the frame re-serialized to text
    try:
        raw = json.dumps({"v": wire})
        b = verdict(lambda: M.model_validate_json(raw, strict=True).v)
    except (TypeError, ValueError) as e:
        b = f"UNSERIALIZABLE: {e}"
    # (c) strict python + pre-coercion
    c = verdict(lambda: M.model_validate({"v": pre_coerce(ann, wire)}, strict=True).v)
    print(f"{label:16} {a:30} {b:30} {c:30}")

section("the bug-hiding cases — stringly-typed input must be REJECTED")
BUGS = [
    ('"1" for int', int, "1"),
    ('"true" for bool', bool, "true"),
    ('1 for str', str, 1),
    ('"1.5" for float', float, "1.5"),
    ('1 for bool', bool, 1),
]
print(f"{'case':20} {'(a) strict-python':30} {'(b) strict-json':30}")
for label, ann, wire in BUGS:
    M = model_for(ann)
    a = verdict(lambda: M.model_validate({"v": wire}, strict=True).v)
    raw = json.dumps({"v": wire})
    b = verdict(lambda: M.model_validate_json(raw, strict=True).v)
    print(f"{label:20} {a:30} {b:30}")

section("the control — correct typed input must pass in all modes")
GOOD = [("int", int, 5), ("str", str, "x"), ("bool", bool, False), ("float", float, 0.5)]
for label, ann, wire in GOOD:
    M = model_for(ann)
    a = verdict(lambda: M.model_validate({"v": wire}, strict=True).v)
    b = verdict(lambda: M.model_validate_json(json.dumps({"v": wire}), strict=True).v)
    print(f"{label:20} {a:30} {b:30}")

section("and lax json mode for contrast (the original finding)")
M = model_for(int)
print('lax json "1" for int:', verdict(lambda: M.model_validate_json(json.dumps({"v": "1"})).v))
print('strict json "1" for int:', verdict(lambda: M.model_validate_json(json.dumps({"v": "1"}), strict=True).v))
