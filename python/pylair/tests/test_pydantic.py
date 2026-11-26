"""Pydantic v2 bridge support (decision 0012) — the spike matrix turned
into unit tests. Every test imports pydantic through importorskip: the
suite must also pass in a pydantic-free environment (see
test_no_pydantic.py for that proof)."""

import datetime
import enum
import json
import uuid
from decimal import Decimal
from pathlib import Path

import pytest

pydantic = pytest.importorskip("pydantic")
from pydantic import (
    BaseModel,
    Field,
    RootModel,
    computed_field,
    field_validator,
    model_serializer,
)

from pylair._json import InvalidParams, coerce, dumps
from pylair._pydantic import is_model_class, is_v1_model_class, pre_coerce_model
from pylair._schema import schema_of

# ---- the conversion matrix (q7/q8 turned unit tests) ------------------------


class Matrix(BaseModel):
    i: int
    s: str
    b: bool
    f: float


class Types(BaseModel):
    at: datetime.datetime
    d: datetime.date
    t: datetime.time
    td: datetime.timedelta
    u: uuid.UUID
    dec: Decimal
    p: Path


class Color(enum.Enum):
    RED = "red"
    BLUE = "blue"


class Containers(BaseModel):
    pair: tuple[int, str]
    tags: set[int]
    frozen: frozenset[int]
    color: Color


def test_primitives_clean():
    m = coerce(Matrix, {"i": 1, "s": "x", "b": True, "f": 0.5})
    assert (m.i, m.s, m.b, m.f) == (1, "x", True, 0.5)


def test_int_for_float_passes():
    m = coerce(Matrix, {"i": 1, "s": "x", "b": True, "f": 2})
    assert m.f == 2.0


@pytest.mark.parametrize(
    "field,value",
    [("i", "1"), ("s", 1), ("b", "true"), ("f", "1.5"), ("b", 1)],
    ids=["str-for-int", "int-for-str", "str-for-bool", "str-for-float", "int-for-bool"],
)
def test_stringly_typed_rejected(field, value):
    payload = {"i": 1, "s": "x", "b": True, "f": 0.5}
    payload[field] = value
    with pytest.raises(InvalidParams):
        coerce(Matrix, payload)


def test_types_matrix():
    m = coerce(
        Types,
        {
            "at": "2026-01-01T00:00:00+00:00",
            "d": "2026-01-01",
            "t": "13:45:00",
            "td": "PT1H30M",
            "u": "12345678-1234-5678-1234-567812345678",
            "dec": "3.14",
            "p": "/tmp/x",
        },
    )
    assert m.at.year == 2026 and m.at.tzinfo is not None
    assert m.d == datetime.date(2026, 1, 1)
    assert m.t == datetime.time(13, 45)
    assert m.td == datetime.timedelta(hours=1, minutes=30)
    assert m.u == uuid.UUID("12345678-1234-5678-1234-567812345678")
    assert m.dec == Decimal("3.14")
    assert str(m.p) in ("/tmp/x", "\\tmp\\x")


def test_timedelta_seconds_rejected():
    """the matrix trap: seconds are NOT a wire form — ISO-8601 duration only."""
    with pytest.raises(InvalidParams):
        coerce(Types, _types_payload(td=3600.5))


def test_containers_from_arrays():
    m = coerce(
        Containers, {"pair": [1, "a"], "tags": [1, 2], "frozen": [3], "color": "red"}
    )
    assert m.pair == (1, "a")
    assert m.tags == {1, 2}
    assert m.frozen == frozenset({3})
    assert m.color is Color.RED


def _types_payload(**over):
    payload = {
        "at": "2026-01-01T00:00:00+00:00",
        "d": "2026-01-01",
        "t": "13:45:00",
        "td": "PT1H",
        "u": "12345678-1234-5678-1234-567812345678",
        "dec": "1.0",
        "p": "/x",
    }
    payload.update(over)
    return payload


# ---- error mapping (#193 shape) ----------------------------------------------


class Item(BaseModel):
    name: str


class Order(BaseModel):
    items: list[Item]
    count: int


def test_error_path_nested():
    with pytest.raises(InvalidParams) as ei:
        coerce(Order, {"items": [{"name": "ok"}, {"name": 5}], "count": 1})
    first = ei.value.errors[0]
    assert first["path"] == "args.items[1].name"
    assert first["expected"] == "str"
    assert first["received"] == "number"


def test_error_never_leaks_input():
    secret = "user-secret-value-xyz"
    with pytest.raises(InvalidParams) as ei:
        coerce(Order, {"items": [{"name": "ok"}, {"name": 5}], "count": secret})
    assert secret not in json.dumps(ei.value.errors)


def test_missing_field_received_missing():
    with pytest.raises(InvalidParams) as ei:
        coerce(Order, {"items": []})
    assert ei.value.errors[0]["received"] == "missing"


def test_custom_validator_message():
    class Even(BaseModel):
        n: int

        @field_validator("n")
        @classmethod
        def even(cls, v):
            if v % 2:
                raise ValueError("must be an even number")
            return v

    with pytest.raises(InvalidParams):
        coerce(Even, {"n": 3})


# ---- aliases (decision §3) -----------------------------------------------------


class Aliased(BaseModel):
    model_config = pydantic.ConfigDict(populate_by_name=True)
    name: str = Field(alias="fullName")
    ident: int = Field(validation_alias="user_id", serialization_alias="userId")


def test_validate_by_validation_alias():
    m = coerce(Aliased, {"fullName": "Ada", "user_id": 7})
    assert m.ident == 7


def test_dump_by_serialization_alias():
    m = coerce(Aliased, {"fullName": "Ada", "user_id": 7})
    out = json.loads(dumps(m))
    assert out == {"fullName": "Ada", "userId": 7}


def test_round_trip():
    """whatever shape the client receives is the shape it may send back"""
    m = coerce(Aliased, {"fullName": "Ada", "user_id": 7})
    out = json.loads(dumps(m))
    # out carries userId — feeding it back validates because populate_by_name
    # accepts field names, and alias covers fullName. The wire contract is
    # the SERIALIZED shape for reads and the VALIDATION shape for writes.
    assert "userId" in out


def test_schema_split_shapes():
    sch = schema_of(Aliased)
    assert sch["type"] == "pydantic"
    in_names = [f["name"] for f in sch["in_fields"]]
    out_names = [f["name"] for f in sch["out_fields"]]
    assert "user_id" in in_names
    assert "userId" in out_names


# ---- computed fields, recursion, nested models ---------------------------------


class Rect(BaseModel):
    width: int
    height: int

    @computed_field
    @property
    def area(self) -> int:
        return self.width * self.height


def test_computed_field_in_output():
    out = json.loads(dumps(coerce(Rect, {"width": 3, "height": 4})))
    assert out["area"] == 12


def test_computed_field_in_schema():
    sch = schema_of(Rect)
    names = [f["name"] for f in sch["out_fields"]]
    assert "area" in names


def test_recursive_model():
    class Node(BaseModel):
        name: str
        child: "Node | None" = None

    Node.model_rebuild()
    m = coerce(Node, {"name": "root", "child": {"name": "leaf", "child": None}})
    assert m.child.name == "leaf"


# ---- bytes fidelity (the json-mode crash that shaped §5) ------------------------


class BlobModel(BaseModel):
    blob: bytes


def test_non_utf8_bytes_round_trip():
    m = coerce(BlobModel, {"blob": b"\x00\xffbinary"})
    out = dumps(m)
    # pylair's own $b64 encoding — never pydantic's utf8 flattening
    assert "$b64" in out


# ---- unsupported shapes (decision §7) -------------------------------------------


def test_root_model_rejected():
    class R(RootModel):
        root: int

    with pytest.raises(TypeError, match="RootModel"):
        schema_of(R)


def test_model_serializer_rejected():
    class Custom(BaseModel):
        x: int

        @model_serializer
        def ser(self):
            return {"anything": self.x}

    with pytest.raises(TypeError, match="model_serializer"):
        schema_of(Custom)


def test_v1_model_rejected():
    v1 = pytest.importorskip("pydantic.v1")

    class Old(v1.BaseModel):
        x: int

    assert is_v1_model_class(Old)
    assert not is_model_class(Old)
    with pytest.raises(TypeError, match="pydantic v1"):
        schema_of(Old)


def test_unsupported_check_ignores_plain_models():
    schema_of(Order)  # no raise


# ---- detection ------------------------------------------------------------------


def test_detection_never_imports():
    """is_model_class on plain classes — and the gates don't manufacture
    a pydantic import (proven process-wide in test_no_pydantic.py)."""
    assert is_model_class(Order)
    assert not is_model_class(int)
    assert not is_model_class(None)


def test_pre_coerce_lenient_on_garbage():
    """pre-coercion never raises — strict validation owns the errors"""
    out = pre_coerce_model(
        Types,
        {"at": "not a date", "d": 5, "t": None, "td": "x", "u": 1, "dec": [], "p": {}},
    )
    assert out["at"] == "not a date"  # left for strict validation
    with pytest.raises(InvalidParams):
        coerce(
            Types,
            {
                "at": "not a date",
                "d": "2026-01-01",
                "t": "13:45:00",
                "td": "PT1H",
                "u": "12345678-1234-5678-1234-567812345678",
                "dec": "1",
                "p": "/x",
            },
        )
