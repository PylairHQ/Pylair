"""Annotation → wire schema for codegen (docs/codegen.md).

Produces {"type": ...} dicts consumed by `pylair codegen` and carried
in the hello handshake's exposes[].signature. Anything unmapped is
{"type": "any"} — honest, never a guess.
"""

from __future__ import annotations

import collections.abc as cabc
import dataclasses
import datetime as _dt
import enum
import inspect
import types
import typing
from typing import Any

from pylair._bigint import BigInt

_EMPTY = inspect.Parameter.empty


def schema_of(annotation: Any) -> dict[str, Any]:
    if annotation in (_EMPTY, None, Any):
        return {"type": "any"} if annotation is not None else {"type": "none"}
    if annotation is type(None):
        return {"type": "none"}

    # PEP 604 (X | Y) and typing.Union
    origin = typing.get_origin(annotation)
    args = typing.get_args(annotation)
    if origin in (typing.Union, types.UnionType):
        return {
            "type": "union",
            "opts": [schema_of(a) for a in args],
        }

    if origin is typing.Literal:
        return {"type": "literal", "values": list(args)}

    # Containers (typing.X and builtin generics share get_origin)
    if origin in (list, set, frozenset, cabc.Sequence, cabc.Iterable) or (
        inspect.isclass(annotation) and issubclass(annotation, (list, set, frozenset))
    ):
        item = schema_of(args[0]) if args else {"type": "any"}
        return {"type": "list", "items": item}

    if origin is tuple or annotation is tuple:
        if len(args) == 2 and args[1] is Ellipsis:
            return {"type": "list", "items": schema_of(args[0])}
        return {
            "type": "tuple",
            "items": [schema_of(a) for a in args],
        }

    if origin is dict or annotation is dict:
        key_t = schema_of(args[0]) if len(args) == 2 else {"type": "any"}
        val_t = schema_of(args[1]) if len(args) == 2 else {"type": "any"}
        return {"type": "dict", "keys": key_t, "values": val_t}

    if origin in (cabc.AsyncGenerator, cabc.AsyncIterator) or str(
        annotation
    ).startswith(("typing.AsyncGenerator", "typing.AsyncIterator")):
        item = schema_of(args[0]) if args else {"type": "any"}
        return {"type": "asyncgen", "items": item}

    if origin is not None or not inspect.isclass(annotation):
        return {"type": "any"}

    # Bare classes
    if annotation is int:
        return {"type": "int"}
    if annotation is BigInt:
        return {"type": "bigint"}
    if annotation is float:
        return {"type": "float"}
    if annotation is str:
        return {"type": "str"}
    if annotation is bool:
        return {"type": "bool"}
    if issubclass(annotation, (bytes, bytearray)):
        return {"type": "bytes"}
    if issubclass(annotation, (_dt.datetime, _dt.date, _dt.time)):
        return {"type": "datetime"}
    if issubclass(annotation, enum.Enum):
        vals = [m.value for m in annotation]
        return {"type": "enum", "values": vals}
    if dataclasses.is_dataclass(annotation):
        return {
            "type": "dataclass",
            "name": annotation.__name__,
            "fields": [
                {
                    "name": f.name,
                    "required": (
                        f.default is dataclasses.MISSING
                        and f.default_factory is dataclasses.MISSING
                    ),
                    "schema": schema_of(_resolve(annotation, f)),
                }
                for f in dataclasses.fields(annotation)
            ],
        }
    return {"type": "any"}


def _resolve(cls: type, f: dataclasses.Field) -> Any:
    """Resolve a dataclass field type — may be a string under
    `from __future__ import annotations`."""
    if isinstance(f.type, str):
        try:
            return typing.get_type_hints(cls)[f.name]
        except Exception:  # noqa: BLE001 - unresolved hint -> any
            return _EMPTY
    return f.type


def callable_schema(fn: Any) -> dict[str, Any]:
    """Full signature schema for one @expose'd function."""
    try:
        sig = inspect.signature(fn)
    except (TypeError, ValueError):
        return {"params": [], "returns": {"type": "any"}}
    try:
        hints = typing.get_type_hints(fn)
    except Exception:  # noqa: BLE001 - unresolved refs fall back to raw anns
        hints = {}
    params = [
        {
            "name": p.name,
            "kind": str(p.kind),
            "required": p.default is _EMPTY,
            "schema": schema_of(hints.get(p.name, p.annotation)),
        }
        for p in sig.parameters.values()
    ]
    ret_ann = hints.get("return", sig.return_annotation)
    if inspect.isasyncgenfunction(fn):
        # For generators, `returns` is the yield type (docs/codegen.md).
        s = schema_of(ret_ann)
        ret = s["items"] if s.get("type") == "asyncgen" else {"type": "any"}
    else:
        ret = schema_of(ret_ann)
    return {"params": params, "returns": ret}
