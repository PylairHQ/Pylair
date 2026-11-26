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


def is_typed_dict_class(annotation: Any) -> bool:
    """True for typing.TypedDict and typing_extensions.TypedDict classes.

    `typing.is_typeddict` doesn't recognize typing_extensions classes on
    every version (3.14), and typing_extensions may be absent — the shared
    metaclass plus the TypedDict-only attrs is the version-proof test.
    """
    return (
        inspect.isclass(annotation)
        and type(annotation).__name__ == "_TypedDictMeta"
        and hasattr(annotation, "__total__")
        and hasattr(annotation, "__required_keys__")
    )


# Qualifier wrappers a TypedDict value annotation can carry. get_type_hints
# strips most of them version-permitting; what survives is unwrapped here
# so every version behaves the same. Requiredness never comes from the
# wrappers — `__required_keys__` is the single source of truth.
_QUALIFIERS: set[Any] = {typing.Required, typing.NotRequired}
if hasattr(typing, "ReadOnly"):  # 3.13+
    _QUALIFIERS.add(typing.ReadOnly)
try:  # typing_extensions is not a hard dependency — present iff the app uses it
    import typing_extensions as _te

    for _q in ("Required", "NotRequired", "ReadOnly"):
        if hasattr(_te, _q):
            _QUALIFIERS.add(getattr(_te, _q))
except ImportError:
    pass


def strip_extras(annotation: Any) -> Any:
    """Unwrap Annotated/Required/NotRequired/ReadOnly layers — the value
    type is always the innermost argument."""
    while True:
        origin = typing.get_origin(annotation)
        if origin is typing.Annotated or origin in _QUALIFIERS:
            args = typing.get_args(annotation)
            if not args:
                return annotation
            annotation = args[0]
        else:
            return annotation


def td_hints(cls: Any) -> dict[str, Any]:
    """Ordered, extras-stripped type hints for a TypedDict, flattened over
    its inheritance chain. Keys that fail resolution are absent — callers
    treat them as `any`, same as unresolved dataclass fields."""
    try:
        hints = typing.get_type_hints(cls, include_extras=True)
    except Exception:  # noqa: BLE001 - unresolved refs degrade to any
        hints = {}
    return {k: strip_extras(v) for k, v in hints.items()}


def _typeddict_schema(cls: Any, seen: frozenset) -> dict[str, Any]:
    if cls in seen:
        raise TypeError(
            f"recursive TypedDict '{cls.__name__}' is not supported in @expose "
            "signatures — the schema embeds fields inline, so a cycle can't "
            "terminate (recursive dataclasses are unsupported for the same "
            "reason). Type the recursive key as a plain dict instead."
        )
    seen = seen | {cls}
    hints = td_hints(cls)
    required = cls.__required_keys__
    optional = cls.__optional_keys__
    # Definition order first (hints), then anything resolution dropped.
    keys = list(hints) + [k for k in required | optional if k not in hints]
    return {
        "type": "typeddict",
        "name": cls.__name__,
        "fields": [
            {
                "name": k,
                "required": k in required,
                "schema": schema_of(hints.get(k, _EMPTY), seen),
            }
            for k in keys
        ],
    }


def schema_of(annotation: Any, _seen: frozenset = frozenset()) -> dict[str, Any]:
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
            "opts": [schema_of(a, _seen) for a in args],
        }

    if origin is typing.Literal:
        return {"type": "literal", "values": list(args)}

    # Containers (typing.X and builtin generics share get_origin)
    if origin in (list, set, frozenset, cabc.Sequence, cabc.Iterable) or (
        inspect.isclass(annotation) and issubclass(annotation, (list, set, frozenset))
    ):
        item = schema_of(args[0], _seen) if args else {"type": "any"}
        return {"type": "list", "items": item}

    if origin is tuple or annotation is tuple:
        if len(args) == 2 and args[1] is Ellipsis:
            return {"type": "list", "items": schema_of(args[0], _seen)}
        return {
            "type": "tuple",
            "items": [schema_of(a, _seen) for a in args],
        }

    if origin is dict or annotation is dict:
        key_t = schema_of(args[0], _seen) if len(args) == 2 else {"type": "any"}
        val_t = schema_of(args[1], _seen) if len(args) == 2 else {"type": "any"}
        return {"type": "dict", "keys": key_t, "values": val_t}

    if origin in (cabc.AsyncGenerator, cabc.AsyncIterator) or str(
        annotation
    ).startswith(("typing.AsyncGenerator", "typing.AsyncIterator")):
        item = schema_of(args[0], _seen) if args else {"type": "any"}
        return {"type": "asyncgen", "items": item}

    if origin is not None:
        if is_typed_dict_class(origin):
            raise TypeError(
                f"subscripted generic TypedDict {annotation} is not supported "
                "in @expose signatures — there is no type-parameter machinery; "
                "annotate the bare class instead (TypeVar keys degrade to "
                "unknown, same as generic dataclass fields)"
            )
        return {"type": "any"}
    if not inspect.isclass(annotation):
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
                    "schema": schema_of(_resolve(annotation, f), _seen),
                }
                for f in dataclasses.fields(annotation)
            ],
        }
    if is_typed_dict_class(annotation):
        return _typeddict_schema(annotation, _seen)
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


def has_bigint_leaf(schema: Any) -> bool:
    """True iff the schema tree declares a bigint leaf anywhere.

    Precomputed once per exposed fn (ExposedFn.ret_schema): when the
    returns schema can't contain a tag, result coercion is skipped
    entirely and results cross untouched.
    """
    if not isinstance(schema, dict):
        return False
    t = schema.get("type")
    if t == "bigint":
        return True
    if t == "union":
        return any(has_bigint_leaf(s) for s in schema.get("opts") or ())
    if t in ("list", "asyncgen"):
        return has_bigint_leaf(schema.get("items"))
    if t == "tuple":
        return any(has_bigint_leaf(s) for s in schema.get("items") or ())
    if t == "dict":
        return has_bigint_leaf(schema.get("values"))
    if t in ("dataclass", "typeddict"):
        return any(has_bigint_leaf(f.get("schema")) for f in schema.get("fields") or ())
    return False


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
