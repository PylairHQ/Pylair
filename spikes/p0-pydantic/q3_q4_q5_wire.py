"""Q3/Q4/Q5: wire names, validation modes + error mapping, result dumping.

Run: uv run python q3_q4_q5_wire.py
"""

import json
from datetime import datetime, timezone

from pydantic import (
    BaseModel,
    ConfigDict,
    Field,
    ValidationError,
    computed_field,
    field_serializer,
    field_validator,
    model_serializer,
)


def section(t):
    print(f"\n{'=' * 70}\n== {t}\n{'=' * 70}")


# ---- Q3: wire names -------------------------------------------------------

section("Q3: aliases across the wire — what round-trips")


class Aliased(BaseModel):
    model_config = ConfigDict(populate_by_name=True)
    name: str = Field(alias="fullName")
    ident: int = Field(validation_alias="user_id", serialization_alias="userId")


# IN (validation): what keys does model_validate accept?
print("validate with validation names:", end=" ")
try:
    m = Aliased.model_validate({"fullName": "Ada", "user_id": 7})
    print("OK ->", m)
except ValidationError as e:
    print("FAIL", e.errors()[0]["msg"])

print("validate with serialization names:", end=" ")
try:
    m = Aliased.model_validate({"fullName": "Ada", "userId": 7})
    print("OK ->", m)
except ValidationError as e:
    print("FAIL:", e.errors()[0]["loc"], e.errors()[0]["msg"])

print("validate with python field names:", end=" ")
try:
    m = Aliased.model_validate({"name": "Ada", "ident": 7})
    print("OK ->", m)
except ValidationError as e:
    print("FAIL:", e.errors()[0]["loc"], e.errors()[0]["msg"])

# OUT (serialization): what keys does model_dump emit?
m = Aliased.model_validate({"fullName": "Ada", "user_id": 7})
print("dump by_alias=True :", json.dumps(m.model_dump(mode="json", by_alias=True), sort_keys=True))
print("dump by_alias=False:", json.dumps(m.model_dump(mode="json", by_alias=False), sort_keys=True))
print()
print("EVIDENCE: in accepts validation_alias (or populate_by_name python names);")
print("out emits serialization_alias ONLY with by_alias=True. Round-trip requires")
print("serialization names on the wire out + validation names on the wire in,")
print("and they DIFFER when the aliases differ (userId out, user_id in).")

# ---- Q4: validation strict vs lax + error mapping -------------------------

section("Q4: strict vs lax — what each does to typical frontend JSON")


class Typed(BaseModel):
    count: int
    label: str
    at: datetime
    ratio: float


cases = {
    "clean": {"count": 1, "label": "x", "at": "2026-01-01T00:00:00Z", "ratio": 1.5},
    "stringly-typed count (classic frontend bug)": {"count": "1", "label": "x", "at": "2026-01-01T00:00:00Z", "ratio": 1.5},
    "int where float expected": {"count": 1, "label": "x", "at": "2026-01-01T00:00:00Z", "ratio": 2},
    "datetime as ISO string (the ONLY way JSON carries it)": {"count": 1, "label": "x", "at": "2026-01-01T00:00:00+00:00", "ratio": 1.5},
}
for label, payload in cases.items():
    for strict in (False, True):
        try:
            m = Typed.model_validate(payload, strict=strict)
            verdict = f"OK count={m.count!r} ratio={m.ratio!r} at={m.at.isoformat()}"
        except ValidationError as e:
            verdict = "REJECTED: " + "; ".join(f"{'.'.join(str(x) for x in er['loc'])} {er['type']}" for er in e.errors())
        print(f"  strict={strict!s:5} {label}: {verdict}")
    print()

print("EVIDENCE: lax silently turns '1' into 1 — the exact frontend-bug-hiding")
print("behavior the bridge rejects elsewhere. strict rejects ISO datetime")
print("strings in python mode, BUT see model_validate_json below.")

print()
print("-- model_validate_json: strict-by-construction for JSON input --")
for label, payload in cases.items():
    wire = json.dumps(payload)
    try:
        m = Typed.model_validate_json(wire)
        verdict = f"OK count={m.count!r} ratio={m.ratio!r}"
    except ValidationError as e:
        verdict = "REJECTED: " + "; ".join(f"{'.'.join(str(x) for x in er['loc'])} {er['type']}" for er in e.errors())
    print(f"  {label}: {verdict}")

section("Q4b: ValidationError.errors() -> INVALID_PARAMS data.errors")


class Nested(BaseModel):
    class Item(BaseModel):
        name: str

    items: list[Item]
    count: int


try:
    Nested.model_validate({"items": [{"name": "ok"}, {"name": 5}], "count": "x", "extra": 1})
except ValidationError as e:
    raw = e.errors()
    print("raw errors() entries (input truncated):")
    for er in raw:
        print("  ", {k: (repr(v)[:30] if k == "input" else v) for k, v in er.items()})

    # mapping to #193's shape: path / expected / received — input dropped,
    # received is the JSON TYPE only
    def json_type(v):
        if v is None:
            return "null"
        if isinstance(v, bool):
            return "boolean"
        if isinstance(v, (int, float)):
            return "number"
        if isinstance(v, str):
            return "string"
        if isinstance(v, list):
            return "array"
        return "object"

    def loc_to_path(loc, root="args"):
        # pydantic loc: ('items', 1, 'name') tuple after the model root;
        # the function-param segment is prepended by the caller side
        out = root
        for part in loc:
            if isinstance(part, int):
                out += f"[{part}]"
            else:
                out += f".{part}"
        return out

    print()
    print("mapped data.errors (input NEVER leaves the process):")
    for er in raw:
        print("  ", {
            "path": loc_to_path(er["loc"]),
            "pydantic_type": er["type"],
            "received": json_type(er.get("input")),
        })
    print()
    print("NOTE: pydantic's `type` is a machine tag ('int_parsing', 'missing',")
    print("'string_type'), NOT the schema type name #193 specifies — `expected`")
    print("needs the field's own annotation looked up by loc, or a type-tag map.")

section("Q4c: custom validator messages")


class Validated(BaseModel):
    n: int

    @field_validator("n")
    @classmethod
    def even(cls, v):
        if v % 2:
            raise ValueError("must be an even number")
        return v


try:
    Validated.model_validate({"n": 3})
except ValidationError as e:
    er = e.errors()[0]
    print("custom validator error type:", er["type"])
    print("custom validator message   :", er["msg"])
    print("EVIDENCE: msg carries 'Value error, <your message>' — human text,")
    print("fine for the error `message` field; machine shape comes from loc.")

# ---- Q5: results out -------------------------------------------------------

section("Q5: model_dump shapes — computed_field, serializers")


class WithComputed(BaseModel):
    width: int
    height: int

    @computed_field
    @property
    def area(self) -> int:
        return self.width * self.height


m = WithComputed(width=3, height=4)
print("dump includes computed_field:", m.model_dump(mode="json"))
print("computed visible statically:", "area" in WithComputed.model_computed_fields)
print("  model_computed_fields:", {k: str(v.return_type) for k, v in WithComputed.model_computed_fields.items()})

print()


class CustomSer(BaseModel):
    value: int

    @model_serializer
    def ser(self):
        return {"totally": "different", "shape": self.value * 2}


m = CustomSer(value=5)
print("model_serializer dump:", m.model_dump(mode="json"))
print("EVIDENCE: a model_serializer can emit ANY shape — codegen cannot know")
print("it statically from the annotations. Unsupported in v1 (or object/any).")

print()


class FieldSer(BaseModel):
    at: datetime

    @field_serializer("at")
    def ser_at(self, v):
        return v.strftime("%d/%m/%Y")


m = FieldSer(at=datetime(2026, 1, 15, tzinfo=timezone.utc))
print("field_serializer dump:", m.model_dump(mode="json"))
print("EVIDENCE: field_serializer changes the WIRE type (datetime -> custom")
print("string) — statically visible IF we walk it, but the return annotation")
print("is often missing. v1: use it when annotated, else document as lossy.")

print()
print("-- interaction with pylair's own encodings --")
big = 2**70
class BigM(BaseModel):
    n: int
print("big int dump mode=json:", BigM(n=big).model_dump(mode="json")["n"], "(exact, python int)")
print("json.dumps of it     :", end=" ")
try:
    print(json.dumps(BigM(n=big).model_dump(mode="json")))
except Exception as e:
    print("FAILS:", e)
print("pylair bigint wire tag would be: {'$bigint': '1180591620717411303424'}")
print("EVIDENCE: mode='json' keeps python ints exact — JSON-serializable but")
print("the CLIENT can't hold >2^53 as a number. pylair's $bigint tagging must")
print("wrap pydantic ints the same as dataclass ints: consistent, not conflicting")
print("(the tag applies AFTER model_dump, in pylair's own encoder).")
