"""Test app for the pydantic e2e — spawned only when pydantic is installed."""

import datetime
import uuid

from pydantic import BaseModel, Field, computed_field

from pylair import expose


class Item(BaseModel):
    name: str
    qty: int


class Order(BaseModel):
    order_id: str = Field(alias="orderId")
    items: list[Item]
    placed_at: datetime.datetime
    ident: uuid.UUID

    @computed_field
    @property
    def total(self) -> int:
        return sum(i.qty for i in self.items)


@expose
def place_order(order: Order) -> Order:
    return order


@expose
def order_schema_probe(order: Order) -> int:
    return len(order.items)
