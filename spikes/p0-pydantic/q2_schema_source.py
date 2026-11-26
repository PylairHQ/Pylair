"""Q2: derive TS from model_fields (pylair's own schema walker) vs
model_json_schema(). Compare what each preserves and loses across the
type features the bridge cares about.

Run: uv run python q2_schema_source.py
"""

import enum
import json
from datetime import datetime
from typing import Annotated, Generic, Literal, Optional, TypeVar, Union

from pydantic import BaseModel, Field, constr, conint
from pydantic.fields import FieldInfo


def walk_model_fields(cls) -> dict:
    """What pylair's own walker sees: the raw model_fields entries.
    This is the SOURCE — annotation objects, defaults, aliases — before
    any serialization opinion is applied."""
    out = {}
    for name, f in cls.model_fields.items():
        out[name] = {
            "annotation": str(f.annotation),
            "alias": f.alias,
            "validation_alias": str(f.validation_alias) if f.validation_alias else None,
            "serialization_alias": f.serialization_alias,
            "default": repr(f.default)[:40] if f.default is not f.default_factory else None,
            "required": f.is_required(),
        }
    return out


def section(title):
    print(f"\n{'=' * 70}\n== {title}\n{'=' * 70}")


def compare(label, cls):
    print(f"\n--- {label} ---")
    mf = walk_model_fields(cls)
    print("model_fields walker sees:")
    for k, v in mf.items():
        print(f"  {k}: {v}")
    try:
        js = cls.model_json_schema()
        print("model_json_schema():")
        print("  " + json.dumps(js)[:600].replace("\n", ""))
    except Exception as e:
        print(f"  model_json_schema FAILED: {type(e).__name__}: {e}")


# ---- the feature matrix ---------------------------------------------------

section("1. Optional / defaults")


class M1(BaseModel):
    a: int
    b: Optional[int] = None
    c: str = "hi"
    d: int | None  # required-but-nullable


compare("optional/defaults", M1)

section("2. unions + discriminated unions")


class Cat(BaseModel):
    kind: Literal["cat"]
    lives: int


class Dog(BaseModel):
    kind: Literal["dog"]
    good: bool


class M2(BaseModel):
    pet: Union[Cat, Dog]  # discriminated by 'kind' literal
    free: int | str  # plain union, no discriminator


compare("unions", M2)

section("3. nested + recursive")


class Node(BaseModel):
    name: str
    child: "Node | None" = None


compare("recursive", Node)

section("4. generics")

T = TypeVar("T")


class Box(BaseModel, Generic[T]):
    item: T
    count: int


class IntBox(Box[int]):
    pass


compare("concretized generic IntBox", IntBox)

section("5. constrained types")


class M5(BaseModel):
    age: conint(ge=0, le=150)
    tag: constr(min_length=1, max_length=8, pattern=r"^[a-z]+$")
    score: Annotated[int, Field(gt=0, description="positive only")]


compare("constrained", M5)

section("6. Literal / Enum")


class Color(enum.Enum):
    RED = "red"
    BLUE = "blue"


class M6(BaseModel):
    lit: Literal["a", "b"]
    color: Color


compare("literal/enum", M6)

section("7. datetime + special types")


class M7(BaseModel):
    at: datetime
    blob: bytes
    big: int  # arbitrary precision in pydantic — pylair has BigInt tags


compare("datetime/bytes/int", M7)

section("8. aliases")


class M8(BaseModel):
    name: str = Field(alias="fullName")
    ident: int = Field(validation_alias="user_id", serialization_alias="userId")


compare("aliases", M8)

# ---- what json_schema drops -----------------------------------------------

section("KEY COMPARISON — what each source can answer")

print(
    """
Questions the TS codegen must answer, per source:

| question                                  | model_fields | model_json_schema |
|-------------------------------------------|--------------|-------------------|
| python annotation object (exact type)     | yes, live    | no — strings only |
| defaults present + value                  | yes          | yes               |
| required vs optional                      | yes          | yes               |
| validation vs serialization alias SPLIT   | yes, both    | ONE name only*    |
| discriminators                            | manual walk  | yes, explicit     |
| recursive models                          | natural      | $ref / $defs      |
| constraints (ge/le/pattern)               | yes, objects | yes, keywords     |
| bigint vs int distinction                 | annotation   | NO — both integer |
| bytes vs str distinction                  | yes          | format only       |
| python-specific types (e.g. tuple shapes) | exact        | lossy             |

* model_json_schema(serialization_alias=...) takes a mode argument — you
  must generate it twice to see both directions.
"""
)
# prove the alias-mode difference
js_by_val = M8.model_json_schema()
print("json_schema property names (validation mode):", sorted(js_by_val["properties"]))
# serialization mode is a different call in v2.3+:
try:
    js_by_ser = M8.model_json_schema(mode="serialization")
    print("json_schema property names (serialization mode):", sorted(js_by_ser["properties"]))
except TypeError as e:
    print("serialization mode unsupported:", e)
