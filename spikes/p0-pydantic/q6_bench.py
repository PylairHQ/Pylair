"""Q6: validate+dump cost of pydantic v2 vs pylair's dataclass coerce path,
on a realistic nested model.

Run: uv run python q6_bench.py
"""

import dataclasses
import statistics
import sys
import timeit
from datetime import datetime, timezone
from typing import Optional

from pydantic import BaseModel

# pylair's own coercion path, imported from the source tree read-only
sys.path.insert(0, "../../python/pylair/src")
from pylair._json import coerce_kwargs, dumps as pydumps  # noqa: E402
import json as _json


# ---- the realistic shape: an order with line items ------------------------

class ItemModel(BaseModel):
    sku: str
    name: str
    price: float
    qty: int


class OrderModel(BaseModel):
    order_id: str
    items: list[ItemModel]
    note: Optional[str] = None
    placed_at: datetime


@dataclasses.dataclass
class ItemDC:
    sku: str
    name: str
    price: float
    qty: int


@dataclasses.dataclass
class OrderDC:
    order_id: str
    items: list[ItemDC]
    note: Optional[str]
    placed_at: datetime


def order_dc_fn(order: OrderDC) -> None:
    pass


wire = {
    "order_id": "o-123",
    "items": [
        {"sku": f"sku-{i}", "name": f"item {i}", "price": 9.99, "qty": i}
        for i in range(10)
    ],
    "note": None,
    "placed_at": datetime.now(timezone.utc).isoformat(),
}

N = 4000

print(f"nested order payload: 10 line items, {N} iterations each")
print()

# pydantic: validate (the IN path)
t = timeit.repeat(lambda: OrderModel.model_validate(wire), number=N, repeat=5)
best = min(t) / N * 1e6
print(f"pydantic model_validate in : {best:8.2f} us/call   (all runs: {[round(x / N * 1e6, 1) for x in t]})")

# dataclass: coerce (the IN path)
t = timeit.repeat(lambda: coerce_kwargs(order_dc_fn, {"order": wire}), number=N, repeat=5)
best = min(t) / N * 1e6
print(f"pylair coerce_kwargs in    : {best:8.2f} us/call   (all runs: {[round(x / N * 1e6, 1) for x in t]})")

print()

# pydantic: dump (the OUT path)
m = OrderModel.model_validate(wire)
t = timeit.repeat(lambda: m.model_dump(mode="json"), number=N, repeat=5)
best = min(t) / N * 1e6
print(f"pydantic model_dump out    : {best:8.2f} us/call   (all runs: {[round(x / N * 1e6, 1) for x in t]})")

# dataclass: encode (pylair's OUT path) — full object-to-JSON-string,
# against the pydantic equivalent json.dumps(model_dump(mode='json'))
dc = coerce_kwargs(order_dc_fn, {"order": wire})["order"]
t = timeit.repeat(lambda: pydumps(dc), number=N, repeat=5)
best = min(t) / N * 1e6
print(f"pylair dumps out           : {best:8.2f} us/call   (all runs: {[round(x / N * 1e6, 1) for x in t]})")

t = timeit.repeat(lambda: _json.dumps(m.model_dump(mode="json")), number=N, repeat=5)
best = min(t) / N * 1e6
print(f"json.dumps(model_dump) out : {best:8.2f} us/call   (all runs: {[round(x / N * 1e6, 1) for x in t]})")

print()
print("context: one bridge call's TOTAL overhead today is in the tens of")
print("microseconds end-to-end (docs/performance.md); even several us of")
print("validation cost per call is small next to IPC. The question is the")
print("RATIO, not the absolute number.")
