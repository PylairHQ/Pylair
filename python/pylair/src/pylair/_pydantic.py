"""Pydantic v2 model support for the typed bridge (decision 0012).

pylair never requires pydantic: detection is a two-gate check that
reads sys.modules and never triggers an import. When the app does use
pydantic v2, models in @expose'd signatures get the same treatment as
dataclasses — schema walking for codegen, strict validation with
pre-coercion on the way in, by-alias dumping on the way out.

The module only ever references the ALREADY-LOADED pydantic module
object; every public entry point is safe to call when pydantic is not
installed (detection returns False, nothing else is reachable).
"""

from __future__ import annotations

import datetime as _dt
import enum
import inspect
import pathlib as _pathlib
import sys
import typing
import uuid as _uuid
from decimal import Decimal as _Decimal
from typing import Any

# ---- detection (decision 0012 §1) ------------------------------------------


def pydantic_loaded() -> bool:
    """True when the app itself has already imported pydantic. Never
    triggers the import — a pylair-only process must stay pydantic-free."""
    return "pydantic" in sys.modules


def _base_model() -> Any:
    mod = sys.modules.get("pydantic")
    return getattr(mod, "BaseModel", None) if mod is not None else None


def is_model_class(cls: Any) -> bool:
    """True when `cls` is a pydantic v2 BaseModel subclass. Two gates:
    the sys.modules probe (never imports), then issubclass against the
    already-loaded BaseModel. v2 only — v1 anatomy goes to
    is_v1_model_class."""
    base = _base_model()
    if base is None or not inspect.isclass(cls):
        return False
    try:
        return issubclass(cls, base) and hasattr(cls, "model_fields")
    except TypeError:
        return False


def is_model_instance(obj: Any) -> bool:
    base = _base_model()
    if base is None:
        return False
    return isinstance(obj, base) and hasattr(type(obj), "model_fields")


def is_v1_model_class(cls: Any) -> bool:
    """pydantic v1 anatomy: the class carries `__fields__` but there is no
    `model_fields` — distinguishable without installing anything, and true
    for both standalone v1 and the `pydantic.v1` compat shim (neither is a
    subclass of v2's BaseModel, so anatomy — not issubclass — is the test)."""
    if not inspect.isclass(cls) or hasattr(cls, "model_fields"):
        return False
    fields = getattr(cls, "__fields__", None)
    if not isinstance(fields, dict):
        return False
    return any(
        str(getattr(c, "__module__", "")).split(".")[0] == "pydantic"
        for c in getattr(cls, "__mro__", ())
    )


def is_root_model(cls: Any) -> bool:
    mod = sys.modules.get("pydantic")
    root = getattr(mod, "RootModel", None) if mod is not None else None
    if root is None or not inspect.isclass(cls):
        return False
    try:
        return issubclass(cls, root)
    except TypeError:
        return False


# ---- unsupported-feature errors (decision 0012 §7) --------------------------


def check_model_supported(cls: type) -> None:
    """Raise a named TypeError for every shape v1 of the bridge does not
    support. Runs at decoration time (inside the schema walk) so the app
    author learns at import, never at first call."""
    if is_v1_model_class(cls):
        raise TypeError(
            f"pydantic v1 model {cls.__name__!r} is not supported — the "
            "bridge supports pydantic v2. Upgrade with "
            "`pip install -U pydantic`; during migration, v1 models can "
            "live under `pydantic.v1` alongside v2."
        )
    if is_root_model(cls):
        raise TypeError(
            f"RootModel {cls.__name__!r} is not supported — the bridge's "
            "function boundary is object-shaped. Wrap the scalar in a "
            "named field of a regular BaseModel."
        )
    # unresolved forward refs: pydantic itself can't validate until
    # model_rebuild() runs — fields still carry ForwardRef annotations
    if hasattr(cls, "model_fields"):
        for name, f in cls.model_fields.items():
            if isinstance(f.annotation, typing.ForwardRef):
                raise TypeError(
                    f"model {cls.__name__!r} field {name!r} has an unresolved "
                    "forward reference — call `Model.model_rebuild()` after "
                    "the referenced class exists, before exposing."
                )
    # model_serializer: the output shape is whatever the function returns —
    # statically unknowable (decision 0012 §5 evidence). pydantic records a
    # plain model-level serializer as 'function-plain' in the model's core
    # schema serialization slot; wrap serializers keep model-shaped output
    # and are fine.
    core = getattr(cls, "__pydantic_core_schema__", None)
    if core is not None:
        ser_schema = core.get("serialization")
        if ser_schema is not None and ser_schema.get("type") == "function-plain":
            raise TypeError(
                f"model {cls.__name__!r} declares a @model_serializer — its "
                "output shape cannot be known statically, so the bridge "
                "cannot type it (decision 0012 §5). Mirror the shape with "
                "a TypedDict or dataclass as the function's return "
                "annotation instead."
            )
    # arbitrary types: fields annotated with types outside the wire model
    # are caught by the schema walker itself (it raises for unmodelled
    # types); nothing extra needed here.


# ---- pre-coercion (decision 0012 §4) ----------------------------------------


def _convert_lenient(annotation: Any, value: Any) -> Any:
    """Convert the JSON-native wire form of `annotation` to the object
    strict validation expects — leniently: any failure leaves the value
    untouched so pydantic's strict validation produces the proper error
    at the proper path. Covers the full matrix of decision 0012 §4:
    datetime/date/time (ISO), timedelta (ISO-8601 duration), UUID,
    Decimal, Enum by value, tuple/set/frozenset from arrays, Path.
    bytes/BigInt are absent — already decoded at frame parse time."""
    from pylair._json import parse_duration

    if value is None:
        return None
    if annotation is _dt.datetime and isinstance(value, str):
        try:
            return _dt.datetime.fromisoformat(value)
        except ValueError:
            return value
    if annotation is _dt.date and isinstance(value, str):
        try:
            return _dt.date.fromisoformat(value)
        except ValueError:
            return value
    if annotation is _dt.time and isinstance(value, str):
        try:
            return _dt.time.fromisoformat(value)
        except ValueError:
            return value
    if annotation is _dt.timedelta and isinstance(value, str):
        try:
            return parse_duration(value)
        except ValueError:
            return value
    if annotation is _uuid.UUID and isinstance(value, str):
        try:
            return _uuid.UUID(value)
        except ValueError:
            return value
    if (
        annotation is _Decimal
        and isinstance(value, (str, int))
        and not isinstance(value, bool)
    ):
        try:
            return _Decimal(value)
        except Exception:  # noqa: BLE001
            return value
    if annotation is _Decimal and isinstance(value, float):
        return _Decimal(str(value))
    if (
        inspect.isclass(annotation)
        and issubclass(annotation, _pathlib.PurePath)
        and isinstance(value, str)
    ):
        try:
            return annotation(value)
        except Exception:  # noqa: BLE001
            return value
    if (
        inspect.isclass(annotation)
        and issubclass(annotation, enum.Enum)
        and not isinstance(value, annotation)
    ):
        try:
            return annotation(value)
        except (ValueError, TypeError):
            return value
    return value


def _pre_coerce(annotation: Any, value: Any) -> Any:
    """Recursive lenient pre-coercion: containers and nested models walk,
    unions try each arm (first conversion wins), leaves convert per the
    matrix. Never raises."""
    if value is None:
        return None

    origin = typing.get_origin(annotation)
    args = typing.get_args(annotation)

    # unions / Optional: first arm that changes the value wins
    if origin in (typing.Union, type(int | str)):
        for arm in args:
            if arm is type(None):
                continue
            out = _pre_coerce(arm, value)
            if out is not value and out != value:
                return out
        return value

    # leaf conversion first — annotated type's own wire form
    converted = _convert_lenient(annotation, value)
    if converted is not value:
        return converted

    # nested pydantic model
    if is_model_class(annotation) and isinstance(value, dict):
        return {k: _pre_coerce_field(annotation, k, v) for k, v in value.items()}

    # containers
    if origin in (list, set, frozenset) and isinstance(value, list):
        item_t = args[0] if args else Any
        items = [_pre_coerce(item_t, v) for v in value]
        if origin is set:
            try:
                return set(items)
            except TypeError:
                return items
        if origin is frozenset:
            try:
                return frozenset(items)
            except TypeError:
                return items
        return items
    if origin is tuple and isinstance(value, list):
        if len(args) == 2 and args[1] is Ellipsis:
            return tuple(_pre_coerce(args[0], v) for v in value)
        return (
            tuple(_pre_coerce(t, v) for t, v in zip(args, value, strict=False))
            if args and len(args) == len(value)
            else tuple(value)
        )
    if origin is dict and isinstance(value, dict):
        val_t = args[1] if len(args) == 2 else Any
        return {k: _pre_coerce(val_t, v) for k, v in value.items()}

    return value


def _field_by_wire_name(cls: type, wire_name: str) -> Any:
    """The model_fields entry a wire key maps to: validation alias first,
    then the field name itself."""
    fields = cls.model_fields
    if wire_name in fields:
        return fields[wire_name]
    for f in fields.values():
        aliases = set()
        if f.alias:
            aliases.add(f.alias)
        va = f.validation_alias
        if isinstance(va, str):
            aliases.add(va)
        if wire_name in aliases:
            return f
    return None


def _pre_coerce_field(cls: type, wire_name: str, value: Any) -> Any:
    f = _field_by_wire_name(cls, wire_name)
    if f is None:
        return value  # unknown key — strict validation reports it
    return _pre_coerce(f.annotation, value)


def pre_coerce_model(cls: type, value: Any) -> Any:
    """The entry point: leniently convert wire forms throughout `value`
    according to the model's fields. Anything not a dict passes through —
    shape errors are strict validation's job."""
    if not isinstance(value, dict):
        return value
    return {k: _pre_coerce_field(cls, k, v) for k, v in value.items()}


# ---- validation + error mapping (decision 0012 §4) ---------------------------


def _annotation_at_path(cls: type, loc: tuple) -> Any:
    """Walk the model's annotations along a pydantic error loc tuple —
    used to name the EXPECTED type at the failure path. Returns None
    when the walk can't resolve (custom validators mid-object)."""
    current: Any = cls
    for part in loc:
        if current is None:
            return None
        if is_model_class(current) or (
            inspect.isclass(current) and hasattr(current, "model_fields")
        ):
            f = current.model_fields.get(part)
            if f is None:
                return None
            current = f.annotation
        elif isinstance(part, int):
            args = typing.get_args(current)
            origin = typing.get_origin(current)
            if origin in (list, set, frozenset) and args:
                current = args[0]
            elif origin is tuple and args and len(args) != 2:
                current = args[part] if part < len(args) else None
            elif origin is tuple and len(args) == 2 and args[1] is Ellipsis:
                current = args[0]
            elif origin is dict and len(args) == 2:
                current = args[1]
            else:
                return None
        else:
            # dict key inside a mapping, or unknown shape
            args = typing.get_args(current)
            if typing.get_origin(current) is dict and len(args) == 2:
                current = args[1]
            else:
                return None
    return current


def _expected_name(annotation: Any) -> str:
    """The expected type as the codegen schema names it (#193's rule) —
    delegates to the shared namer so model errors and dataclass errors
    read identically."""
    from pylair._json import _schema_name

    return _schema_name(annotation)


def validate_model(cls: type, value: Any, path: str) -> Any:
    """Pre-coerce, strict-validate, map failures onto INVALID_PARAMS.

    Raises pylair._json.InvalidParams with errors=[{path, expected,
    received}] — the exact shape of the dataclass path (#193), so a
    client can't tell which kind of model rejected the input."""
    from pylair._json import InvalidParams, _json_type

    mod = sys.modules["pydantic"]
    pre_coerced = pre_coerce_model(cls, value)
    try:
        return cls.model_validate(pre_coerced, strict=True)
    except mod.ValidationError as e:
        errors = []
        for er in e.errors():
            loc = er.get("loc", ())
            # path: same shape as the dataclass walker — dotted keys,
            # [i] indices, rooted at the caller-supplied prefix
            p = path
            for part in loc:
                if isinstance(part, int):
                    p += f"[{part}]"
                else:
                    p += f".{part}"
            ann = _annotation_at_path(cls, loc)
            expected = (
                _expected_name(ann) if ann is not None else er.get("type", "value")
            )
            errors.append(
                {
                    "path": p,
                    "expected": expected,
                    # the JSON TYPE only — `input` never leaves the process
                    "received": (
                        "missing"
                        if er.get("type") == "missing"
                        else _json_type(er.get("input"))
                    ),
                }
            )
        first = errors[0] if errors else None
        if first:
            msg = f"{first['path']}: expected {first['expected']}, got {first['received']}"
        else:
            msg = str(e).splitlines()[0] if str(e) else "validation failed"
        raise InvalidParams(msg, errors=errors) from e


# ---- dumping out (decision 0012 §5) ------------------------------------------


def dump_model(instance: Any) -> dict:
    """model_dump for the wire. mode='python', NOT 'json': json mode
    crashes on non-UTF-8 bytes (UnicodeDecodeError — verified in the
    p0-pydantic spike) and utf8-flattens the rest; python mode keeps
    real objects so pylair's own encoder applies the SAME bytes/datetime/
    bigint handling as the dataclass path. by_alias per decision §3."""
    return instance.model_dump(mode="python", by_alias=True)
