# p0-pydantic — design spike: Pydantic v2 models in the typed bridge

Evidence for `docs/decisions/0012-pydantic.md` (status proposed). Nothing
here ships: no changes to `python/pylair`, the codegen, or any package.

## Run

```sh
cd spikes/p0-pydantic
uv sync
uv run python q1_detection.py      # detection without importing pydantic
uv run python q2_schema_source.py  # model_fields vs model_json_schema()
uv run python q3_q4_q5_wire.py     # aliases, strict/lax, error mapping, dump shapes
uv run python q6_bench.py          # validate+dump cost vs the dataclass path
```

Each script prints its evidence sections and exits 0. Numbers quoted in
the decision record come from these runs (Windows, CPython 3.15,
pydantic 2.14.4 — ratios, not absolute timings, are the evidence).

## Files

| file | question it settles |
|---|---|
| `q1_detection.py` | detect BaseModel subclasses without importing pydantic; v1 anatomy |
| `q2_schema_source.py` | feature-by-feature comparison of the two schema sources |
| `q3_q4_q5_wire.py` | alias round-trip; strict/lax/json-mode validation; `ValidationError.errors()` mapping; `computed_field`/`model_serializer` shapes; BigInt interaction |
| `q6_bench.py` | `model_validate`/`model_dump` cost vs `coerce_kwargs`/`dumps` on a nested order model |
| `q7_strict_matrix.py` | the full type x strict-mode conversion matrix + stringly-typed bug cases |
| `q8_decoded_and_cost.py` | already-decoded objects under strict-python; timedelta forms; re-serialization cost; bytes-through-strict-json failure |
