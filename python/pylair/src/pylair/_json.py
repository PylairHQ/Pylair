"""Wire JSON for the pylair protocol.

Plain json.dumps can't serialize bytes, dataclass instances, or
datetime — this layer makes the annotated types in docs/codegen.md
actually round-trip:

  bytes/bytearray -> {"$b64": "<base64>"}  (restored by object_hook)
  pylair.Blob     -> {"$blob": {"id","mime","size"}} — bytes live in
                     PYLAIR_BLOB_DIR; registration rides ahead of the
                     carrying frame (decision 0010)
  pylair.BigInt   -> {"$bigint": "<decimal>"}  (restored by object_hook)
  @dataclass      -> dataclasses.asdict
  datetime/date/time -> ISO 8601 string
  Enum -> its value; set/frozenset -> list

Param coercion (coerce_kwargs) reconstructs annotated types on the call
path: dict -> dataclass, containers recurse. Construction failures raise
InvalidParams (wire INVALID_PARAMS); annotations that don't map to a
reconstructable type pass the wire value through untouched.

NaN/Infinity are rejected at encode time (allow_nan=False) — a bare NaN
token would be a malformed frame to the Rust host, and a model returning
a NaN score must fail one call, never the runtime (protocol §2).
"""

from __future__ import annotations

import base64
import binascii
import dataclasses
import datetime as _dt
import enum
import inspect
import json
import math
import types
import typing
import weakref
from typing import Any

from pylair._bigint import BIGINT_MAX_DIGITS, BIGINT_RE, BIGINT_TAG, BigInt
from pylair._blob import Blob, BlobFallbackExceeded  # noqa: F401 — re-export for senders

_B64_TAG = "$b64"

# JS Number.MAX_SAFE_INTEGER — the largest int that survives JSON.parse.
_SAFE_INT = (1 << 53) - 1


class InvalidParams(Exception):
    """Incoming params couldn't be coerced by annotation.

    Maps to INVALID_PARAMS on the wire (protocol §8) — the host sent a
    value the annotated signature can't hold (e.g. a dataclass field
    missing), which is a caller error, not an internal failure.
    """


def _default(obj: Any) -> Any:
    if isinstance(obj, Blob):
        return obj._to_wire(None)
    if isinstance(obj, (bytes, bytearray)):
        return {_B64_TAG: base64.b64encode(bytes(obj)).decode("ascii")}
    if dataclasses.is_dataclass(obj) and not isinstance(obj, type):
        return dataclasses.asdict(obj)
    if isinstance(obj, (_dt.datetime, _dt.date, _dt.time)):
        return obj.isoformat()
    if isinstance(obj, enum.Enum):
        return obj.value
    if isinstance(obj, (set, frozenset)):
        return sorted(obj, key=repr)
    raise TypeError(f"{type(obj).__name__} is not wire-serializable")


def _object_hook(d: dict) -> Any:
    if set(d) == {_B64_TAG} and isinstance(d[_B64_TAG], str):
        try:
            return base64.b64decode(d[_B64_TAG], validate=True)
        except Exception:  # noqa: BLE001 - invalid tag stays a plain object
            return d
    if set(d) == {BIGINT_TAG} and isinstance(d[BIGINT_TAG], str):
        v = d[BIGINT_TAG]
        if not BIGINT_RE.fullmatch(v):
            return d  # not canonical decimal — caller data, pass through
        if len(v.lstrip("-")) > BIGINT_MAX_DIGITS:
            raise ValueError(
                f"$bigint payload exceeds the {BIGINT_MAX_DIGITS}-digit cap"
            )
        return BigInt(v)
    return d


class DepthExceeded(Exception):
    """check_wire found nesting deeper than the caller's limit.

    Distinct from ValueError so callers can map it to LIMIT_EXCEEDED —
    an over-deep frame would arrive as a parse failure on the host, the
    same class of protection as the frame cap (protocol §2, §8).
    """


def check_wire(obj: Any, depth_limit: int | None = None) -> None:
    """One pre-serialization walk over `obj`: nesting depth, the int
    range and NaN/Infinity — everything the emitted JSON must satisfy,
    checked on the pre-dump object so `_default` conversions (dataclass
    fields, enum values, set items) are covered exactly like the old
    re-parse guard was. Iterative for the same reason exceeds_depth is:
    a recursive walk hits Python's own recursion limit on the payloads
    this exists to catch.
    """
    # `seen` is the deepest depth each container id was walked at. The
    # int/NaN guards need each node visited once, but the DEPTH check
    # needs a container re-walked whenever it is reached deeper than
    # before: json.dumps expands every reference, so a shared container
    # first seen shallow and later reached deeper has its second copy's
    # contents sitting deeper in the emitted text than the first walk
    # measured. A copy reached at or shallower than a previous visit is
    # always covered (its subtree emits at strictly smaller depths).
    # With no depth_limit there is no depth to re-measure, so any
    # revisit is skipped — that is also the cycle guard, letting the
    # structure reach json.dumps' own circular-reference error. With a
    # limit, re-walks stay bounded: each revisit is strictly deeper, so
    # a cycle climbs to DepthExceeded and terminates.
    seen: dict[int, int] = {}
    stack = [(obj, 1)]
    while stack:
        node, depth = stack.pop()
        if depth_limit is not None and depth > depth_limit:
            raise DepthExceeded(depth_limit)
        if node is None or isinstance(node, (str, bool)):
            continue
        if isinstance(node, BigInt):
            # _tag_bigints -> {"$bigint": str} — a plain object leaf. No
            # range check: crossing safely is the type's whole point.
            continue
        if isinstance(node, int):
            # bool was filtered above — it is a JSON literal, never an int.
            if abs(node) > _SAFE_INT:
                raise ValueError(
                    f"integer {node} exceeds the JSON safe-integer range "
                    "(±2^53-1); return it as a string or use pylair.BigInt "
                    "for exact big integers"
                )
            continue
        if isinstance(node, float):
            if not math.isfinite(node):
                raise ValueError(f"{node} is not wire-serializable")
            continue
        i = id(node)
        prev = seen.get(i)
        if prev is not None and (depth_limit is None or prev >= depth):
            continue
        seen[i] = depth
        if isinstance(node, dict):
            stack.extend((v, depth + 1) for v in node.values())
            continue
        if isinstance(node, (list, tuple)):
            stack.extend((v, depth + 1) for v in node)
            continue
        if isinstance(node, (set, frozenset)):
            # _default -> sorted list: items nest one level deeper.
            stack.extend((v, depth + 1) for v in node)
            continue
        if isinstance(node, (bytes, bytearray)):
            continue  # _default -> {"$b64": str} — a plain string leaf.
        if isinstance(node, Blob):
            # _default -> {"$blob": {...}} — a leaf. The capability-off
            # fallback has to fit the frame cap BEFORE the encode writes
            # files or the call dies mid-serialize.
            node._fallback_check()
            continue
        if dataclasses.is_dataclass(node) and not isinstance(node, type):
            # _default -> asdict: the fields land one level deeper and can
            # carry guarded values the old re-parse caught on the text.
            stack.extend(
                (getattr(node, f.name), depth + 1) for f in dataclasses.fields(node)
            )
            continue
        if isinstance(node, (_dt.datetime, _dt.date, _dt.time)):
            continue  # _default -> ISO string leaf.
        if isinstance(node, enum.Enum):
            # _default -> the value itself, at this position — it may be
            # a guarded int, so it goes back through the walk.
            stack.append((node.value, depth))
            continue
        # Anything else: _default raises TypeError at dumps time — the
        # same error the caller maps today; no pre-check needed.


def _tag_bigints(obj: Any) -> Any:
    """Pre-dump transform: every BigInt leaf -> {"$bigint": "<decimal>"}.

    Why a pass and not a `default` case: json's encoder serializes int
    subclasses natively, so `default` never fires for BigInt. A subtree
    with no BigInt inside is returned AS-IS — containers are rebuilt
    only along changed paths, so ordinary frames pay a walk, not a
    copy. Shared nodes are tagged once via the memo (a shared subtree
    is visited once), and a cycle rebuilds as a cycle so dumps raises
    its own circular error. bytes, Blob, datetime and friends are left
    for _default; a set can't hold tag dicts, so a changed set emits
    _default's sorted list directly.
    """
    memo: dict[int, Any] = {}

    def tag(node: Any) -> Any:
        if isinstance(node, BigInt):
            digits = str(int(node)).lstrip("-")
            if len(digits) > BIGINT_MAX_DIGITS:
                raise ValueError(
                    f"BigInt exceeds the {BIGINT_MAX_DIGITS}-digit wire cap"
                )
            return {BIGINT_TAG: str(int(node))}
        if node is None or isinstance(node, (bool, int, float, str)):
            return node
        nid = id(node)
        if nid in memo:
            return memo[nid]
        if isinstance(node, dict):
            out: dict = {}
            memo[nid] = out  # cycles must see the in-progress rebuild
            changed = False
            for k, v in node.items():
                c = tag(v)
                if c is not v:
                    changed = True
                out[k] = c
            if not changed:
                memo[nid] = node
                return node
            return out
        if isinstance(node, (list, tuple)):
            out = []
            memo[nid] = out
            changed = False
            for v in node:
                c = tag(v)
                if c is not v:
                    changed = True
                out.append(c)
            if not changed:
                memo[nid] = node
                return node
            return out
        if isinstance(node, (set, frozenset)):
            out = []
            changed = False
            for v in sorted(node, key=repr):
                c = tag(v)
                if c is not v:
                    changed = True
                out.append(c)
            return out if changed else node
        if dataclasses.is_dataclass(node) and not isinstance(node, type):
            out = {}
            memo[nid] = out
            changed = False
            for f in dataclasses.fields(node):
                v = getattr(node, f.name)
                c = tag(v)
                if c is not v:
                    changed = True
                out[f.name] = c
            if not changed:
                memo[nid] = node
                return node
            return out
        if isinstance(node, enum.Enum):
            return tag(node.value)
        return node  # everything _default owns — leave it alone

    return tag(obj)


def dumps_collect(
    obj: Any, regs: dict[str, Blob], depth_limit: int | None = None
) -> str:
    """dumps + collect the handles minted during serialization.

    `regs` fills with {Blob: _BlobHandle} — keyed by object identity, so
    the same Blob twice in one frame is one handle while each PASS mints
    fresh ones (the Blob is re-sendable). The caller flushes each handle
    as a `blob.register` notification AHEAD of the carrying frame
    (decision 0010 §3 — a single writer's FIFO makes
    registration-before-use atomic). If the dump raises after some
    handles already wrote files, those files are unlinked: nothing was
    ever registered, so Python still owns them.
    """
    check_wire(obj, depth_limit)

    def default(o: Any) -> Any:
        if isinstance(o, Blob):
            return o._to_wire(regs)
        return _default(o)

    try:
        return json.dumps(
            _tag_bigints(obj), default=default, separators=(",", ":"), allow_nan=False
        )
    except BaseException:
        for b in regs.values():
            b._unlink()
        raise


def dumps_probe(obj: Any, depth_limit: int | None = None) -> str:
    """The size-check twin of `dumps_collect`: byte-identical output,
    except a Blob emits a placeholder `$blob`/`$b64` tag of the real
    length instead of materializing a handle. For cap checks that run
    before the send's own collecting dump — probing with dumps_collect
    would write a file the real dump then writes AGAIN (the dry pass's
    handle is unlinked, never registered).
    """
    check_wire(obj, depth_limit)

    def default(o: Any) -> Any:
        if isinstance(o, Blob):
            return o._to_wire_dry()
        return _default(o)

    return json.dumps(
        _tag_bigints(obj), default=default, separators=(",", ":"), allow_nan=False
    )


def dumps(obj: Any, depth_limit: int | None = None) -> str:
    check_wire(obj, depth_limit)
    # allow_nan=False stays as the hard guarantee: even if a path ever
    # reached dumps without check_wire, a bare NaN/Infinity token can
    # still never hit the wire (serde_json rejects it as malformed).
    return json.dumps(
        _tag_bigints(obj), default=_default, separators=(",", ":"), allow_nan=False
    )


def loads(raw: str | bytes) -> Any:
    return json.loads(raw, object_hook=_object_hook)


# ---- input validation + coercion: dict -> annotated types ------------------

_SHORT_LIMIT = 80


def _typename(ann: Any) -> str:
    return getattr(ann, "__name__", str(ann))


def _short(value: Any) -> str:
    """repr for error messages — never echo a huge user-supplied value."""
    r = repr(value)
    return r if len(r) <= _SHORT_LIMIT else r[:_SHORT_LIMIT] + "…"


def coerce(annotation: Any, value: Any) -> Any:
    """Validate `value` (wire JSON) against `annotation` and rebuild the
    annotated type. Raises InvalidParams on mismatch — a value that can't
    hold the annotation must never reach the function body. Annotations
    this layer doesn't model pass through unchanged."""
    if (
        annotation is inspect.Parameter.empty
        or annotation is Any
        or annotation is object
    ):
        return value

    origin = typing.get_origin(annotation)
    args = typing.get_args(annotation)

    # Optional[T] / Union[...] — valid if any arm matches.
    if origin in (typing.Union, types.UnionType):
        if value is None and type(None) in args:
            return None
        for arm in args:
            if arm is type(None):
                continue
            try:
                return coerce(arm, value)
            except Exception:  # noqa: BLE001,S112 - try the next union arm
                continue
        raise InvalidParams(f"expected {annotation}, got {_short(value)}")

    # null is only valid for Optional (handled above).
    if value is None:
        raise InvalidParams(f"expected {_typename(annotation)}, got null")

    # Primitives: strict wire types. bool precedes int — isinstance(True, int)
    # is True, and JSON true is never a valid int/float.
    if annotation is bool:
        if isinstance(value, bool):
            return value
        raise InvalidParams(f"expected bool, got {_short(value)}")
    if annotation is int:
        # JSON can't distinguish 3 from 3.0 — whole floats are ints.
        if isinstance(value, int) and not isinstance(value, bool):
            return value
        if isinstance(value, float) and value.is_integer():
            return int(value)
        raise InvalidParams(f"expected int, got {_short(value)}")
    if annotation is BigInt:
        # Decoded tags arrive as BigInt already; a plain JSON number is
        # promoted so the fn sees the annotated type either way.
        if isinstance(value, int) and not isinstance(value, bool):
            return BigInt(value)
        raise InvalidParams(f"expected BigInt, got {_short(value)}")
    if annotation is float:
        if isinstance(value, (int, float)) and not isinstance(value, bool):
            return float(value)
        raise InvalidParams(f"expected float, got {_short(value)}")
    if annotation is str:
        if isinstance(value, str):
            return value
        raise InvalidParams(f"expected str, got {_short(value)}")

    if inspect.isclass(annotation) and dataclasses.is_dataclass(annotation):
        if not isinstance(value, dict):
            raise InvalidParams(
                f"expected object for {_typename(annotation)}, got {_short(value)}"
            )
        fields = _dataclass_fields(annotation)
        # Under `from __future__ import annotations` f.type is a
        # string — resolved hints are required for nested types.
        hints = _resolved_hints(annotation)
        try:
            kwargs = {
                k: coerce(hints.get(k, fields[k].type), v)
                for k, v in value.items()
                if k in fields
            }
            return annotation(**kwargs)
        except InvalidParams:
            raise
        except Exception as e:
            raise InvalidParams(f"{_typename(annotation)}: {e}") from e

    if origin in (list, set, frozenset) or annotation is list:
        if not isinstance(value, list):
            raise InvalidParams(
                f"expected list for {_typename(annotation)}, got {_short(value)}"
            )
        (item_t,) = args or (inspect.Parameter.empty,)
        items = [coerce(item_t, v) for v in value]
        return set(items) if origin in (set, frozenset) else items

    if origin is tuple:
        if not isinstance(value, list):
            raise InvalidParams(f"expected list for tuple, got {_short(value)}")
        if len(args) == 2 and args[1] is Ellipsis:
            return tuple(coerce(args[0], v) for v in value)
        return tuple(coerce(t, v) for t, v in zip(args, value, strict=False))

    if origin is dict or annotation is dict:
        if not isinstance(value, dict):
            raise InvalidParams(f"expected object for dict, got {_short(value)}")
        val_t = args[1] if len(args) == 2 else inspect.Parameter.empty
        return {k: coerce(val_t, v) for k, v in value.items()}

    if annotation in (_dt.datetime, _dt.date, _dt.time):
        if isinstance(value, str):
            try:
                return annotation.fromisoformat(value)
            except ValueError:
                pass
        raise InvalidParams(
            f"expected ISO-8601 {_typename(annotation)}, got {_short(value)}"
        )

    if annotation is bytes:
        if isinstance(value, (bytes, bytearray)):
            return bytes(value)
        if isinstance(value, str):
            # TS may send a bare base64 string for a bytes-annotated param.
            try:
                return base64.b64decode(value, validate=True)
            except (binascii.Error, ValueError):
                raise InvalidParams(
                    f"expected base64 bytes, got {_short(value)}"
                ) from None
        raise InvalidParams(f"expected base64 bytes, got {_short(value)}")

    if inspect.isclass(annotation) and issubclass(annotation, enum.Enum):
        try:
            return annotation(value)
        except ValueError:
            raise InvalidParams(
                f"expected {_typename(annotation)}, got {_short(value)}"
            ) from None

    return value


# inspect.signature + get_type_hints are the two most expensive parts of
# argument coercion (docs/performance.md) and their results are fixed per
# function/class object. Resolved lazily on the FIRST call — not at
# @expose time, where forward refs may not exist yet — and cached on a
# WeakKeyDictionary: dev reload replaces function objects, so new objects
# resolve fresh and dead objects' entries GC away.
_SIG_CACHE: weakref.WeakKeyDictionary[Any, tuple] = weakref.WeakKeyDictionary()
_HINTS_CACHE: weakref.WeakKeyDictionary[Any, dict] = weakref.WeakKeyDictionary()
_FIELDS_CACHE: weakref.WeakKeyDictionary[Any, dict] = weakref.WeakKeyDictionary()


def _sig_parts(fn: Any) -> tuple | None:
    """(params, hints, has_varkw) for `fn`, or None when signature()
    can't introspect it — cached per function object."""
    try:
        return _SIG_CACHE[fn]
    except (KeyError, TypeError):  # TypeError: not weakref-able
        pass
    try:
        sig = inspect.signature(fn)
    except (TypeError, ValueError):
        parts = None
    else:
        try:
            hints = typing.get_type_hints(fn)
        except Exception:  # noqa: BLE001 - unresolved refs fall back to raw anns
            hints = {}
        parts = (
            sig.parameters,
            hints,
            any(
                p.kind is inspect.Parameter.VAR_KEYWORD for p in sig.parameters.values()
            ),
        )
    try:
        _SIG_CACHE[fn] = parts
    except TypeError:
        pass  # un-cacheable callable — resolved per call, as before
    return parts


def _resolved_hints(cls: Any) -> dict:
    """get_type_hints for a dataclass, cached per class object; {} when
    resolution fails (same fallback as the old inline try/except)."""
    try:
        return _HINTS_CACHE[cls]
    except (KeyError, TypeError):
        pass
    try:
        hints = typing.get_type_hints(cls)
    except Exception:  # noqa: BLE001 - fall back to raw field types
        hints = {}
    try:
        _HINTS_CACHE[cls] = hints
    except TypeError:
        pass
    return hints


def _dataclass_fields(cls: Any) -> dict:
    """{name: Field} map for a dataclass, cached per class object."""
    try:
        return _FIELDS_CACHE[cls]
    except (KeyError, TypeError):
        pass
    fields = {f.name: f for f in dataclasses.fields(cls)}
    try:
        _FIELDS_CACHE[cls] = fields
    except TypeError:
        pass
    return fields


def coerce_kwargs(fn: Any, input: dict) -> dict:
    """Validate an incoming `input` dict against `fn`'s signature and coerce
    values to the annotated types. Raises InvalidParams for missing required
    params, unexpected keys (unless `**kwargs`), or a value the annotation
    can't hold — the function body never sees bad input."""
    parts = _sig_parts(fn)
    if parts is None:
        return input
    params, hints, has_varkw = parts
    if not has_varkw:
        for k in input:
            if k not in params:
                raise InvalidParams(f"unexpected parameter '{k}'")

    for name, p in params.items():
        if (
            p.default is inspect.Parameter.empty
            and p.kind
            not in (inspect.Parameter.VAR_POSITIONAL, inspect.Parameter.VAR_KEYWORD)
            and name not in input
        ):
            raise InvalidParams(f"missing required parameter '{name}'")

    out = dict(input)
    for name, param in params.items():
        if name not in out or param.kind is inspect.Parameter.VAR_KEYWORD:
            continue
        # `x: str = None` — a None default admits null even when the
        # annotation isn't Optional (pre-PEP-484 style is everywhere).
        if out[name] is None and param.default is None:
            continue
        ann = hints.get(name, param.annotation)
        try:
            out[name] = coerce(ann, out[name])
        except Exception as e:
            # A construction failure must not silently deliver a dict
            # where the function expects an object (docs/codegen.md).
            raise InvalidParams(f"param '{name}': {e}") from e
    return out


# ---- result coercion: the declared return type decides (docs/codegen.md) ----


def coerce_result(schema: dict[str, Any] | None, value: Any) -> Any:
    """Return-side mirror of coerce(): where the signature's "returns"
    schema says bigint, a plain int becomes BigInt before encoding — the
    declared type decides, not the value's class (int arithmetic always
    drops the subclass, so `a + b` on two BigInts is a plain int).

    Best-effort widening only: a non-int under a bigint leaf, a bool
    anywhere, and every unmapped shape keep the value's own encoding —
    no silent str/float conversion and no new error. A container is
    rebuilt only when a child actually changed — an unchanged walk
    returns the original object (identity), and a changed one is a
    plain list/dict, which encodes identically on the wire and never
    trips a NamedTuple-style constructor. `schema` is the
    callable_schema() "returns" dict; for a generator that is the YIELD
    schema, so the same walk covers stream chunks.
    """
    t = schema.get("type") if isinstance(schema, dict) else None
    if t == "bigint":
        if isinstance(value, int) and not isinstance(value, bool):
            return value if isinstance(value, BigInt) else BigInt(value)
        return value
    if t == "union":
        for opt in schema.get("opts") or ():
            out = coerce_result(opt, value)
            if out is not value:
                return out
        return value
    if t == "list":
        item = schema.get("items") or {}
        if isinstance(value, (list, tuple, set, frozenset)):
            return _widened_seq(item, value)
        return value
    if t == "tuple":
        items = schema.get("items") or ()
        if isinstance(value, (list, tuple)) and len(items) == len(value):
            out = []
            changed = False
            for s, v in zip(items, value, strict=True):
                c = coerce_result(s, v)
                if c is not v:
                    changed = True
                out.append(c)
            return out if changed else value
        return value
    if t == "dict":
        vals = schema.get("values") or {}
        if isinstance(value, dict):
            out = {}
            changed = False
            for k, v in value.items():
                c = coerce_result(vals, v)
                if c is not v:
                    changed = True
                out[k] = c
            return out if changed else value
        return value
    if t == "dataclass":
        fields = {f["name"]: f.get("schema") for f in schema.get("fields") or ()}
        if dataclasses.is_dataclass(value) and not isinstance(value, type):
            out = {}
            changed = False
            for f in dataclasses.fields(value):
                v = getattr(value, f.name)
                c = coerce_result(fields[f.name], v) if f.name in fields else v
                if c is not v:
                    changed = True
                out[f.name] = c
            # A plain field dict encodes identically to the instance —
            # dataclasses.replace would re-run __init__/__post_init__.
            return out if changed else value
        if isinstance(value, dict):
            out = {}
            changed = False
            for k, v in value.items():
                c = coerce_result(fields[k], v) if k in fields else v
                if c is not v:
                    changed = True
                out[k] = c
            return out if changed else value
        return value
    return value


def _widened_seq(item_schema: Any, seq: Any) -> Any:
    """coerce_result over a sequence's items: the original object when
    nothing changed, else a plain list — sets and tuple subclasses
    encode as JSON arrays anyway, so rebuilding via type(seq)(...) is
    both unnecessary and fragile (NamedTuple's constructor)."""
    out = []
    changed = False
    for v in seq:
        c = coerce_result(item_schema, v)
        if c is not v:
            changed = True
        out.append(c)
    return out if changed else seq
