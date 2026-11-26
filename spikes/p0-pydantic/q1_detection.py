"""Q1: how to detect a pydantic model without importing pydantic,
unless the app already imported it. And what pydantic v1 looks like.

Run: uv run python q1_detection.py
"""

import sys


def is_pydantic_model_class(cls: type) -> bool:
    """True when `cls` is a pydantic v2 BaseModel subclass — without
    importing pydantic. Two gates:

    1. sys.modules: if the app never imported pydantic, the question is
       moot — a class can't BE a BaseModel subclass without pydantic
       loaded, so we must not pay the import cost (or worse, succeed at
       importing pydantic into a process that didn't want it).
    2. issubclass against the already-loaded BaseModel — zero cost.
    """
    mod = sys.modules.get("pydantic")
    if mod is None:
        return False
    base = getattr(mod, "BaseModel", None)
    if base is None:
        return False
    try:
        return issubclass(cls, base)
    except TypeError:
        return False


def pydantic_major() -> int | None:
    """The loaded pydantic's major version, or None when not loaded."""
    mod = sys.modules.get("pydantic")
    if mod is None:
        return None
    return int(mod.VERSION.split(".", 1)[0])


print("== before any pydantic import ==")
print("'pydantic' in sys.modules:", "pydantic" in sys.modules)


class PlainDataclass:
    pass


print("plain class detected:", is_pydantic_model_class(PlainDataclass))
assert not is_pydantic_model_class(PlainDataclass)
# The detection itself must not have caused an import:
assert "pydantic" not in sys.modules, "detection must never import pydantic"
print("detection caused no import: OK")

print()
print("== after the app imports pydantic v2 ==")
from pydantic import BaseModel


class User(BaseModel):
    name: str


print("v2 model detected:", is_pydantic_model_class(User))
print("plain class still not detected:", is_pydantic_model_class(PlainDataclass))
print("major version:", pydantic_major())

print()
print("== what a v1 model looks like (static anatomy, no install needed) ==")
# v1 vs v2 differ in the class anatomy, detectable without installing v1:
#   v1: instances/class carry __fields__;  NO model_fields attribute
#   v2: model_fields;                      __fields__ deprecated-but-present
print("v2 has model_fields:", hasattr(User, "model_fields"))
print("v2 has __fields__ (deprecated compat):", hasattr(User, "__fields__"))
# A v1-only model would have __fields__ but NOT model_fields — so:
#   hasattr(cls, 'model_fields')   -> v2 fast path
#   hasattr(cls, '__fields__') and not model_fields -> v1: unsupported
# Neither attribute                              -> not a model at all

print()
print("== import cost being avoided ==")
# What the sys.modules gate saves when the app does NOT use pydantic:
import time

# measure a cold import cost by timing importlib machinery on an
# already-cheap module as a floor — the point is ordering, not precision
t0 = time.perf_counter()
import importlib

importlib.import_module("pydantic")
t1 = time.perf_counter()
print(f"import pydantic cost once: {(t1 - t0) * 1000:.1f} ms (first import in this process)")
print("sys.modules lookup cost: nanoseconds — the gate is ~free")
