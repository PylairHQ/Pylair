# 0012: Pydantic v2 models in the typed bridge

Status: proposed (spike: `spikes/p0-pydantic` — every claim below cites
output reproducible by running the spike scripts; see §Evidence index).

## Question

Users writing Python backends for Pylair apps often already model their
domain with Pydantic v2 — it is the de-facto standard in the ecosystem
they come from (FastAPI, LangChain, most modern data libraries). Today
the typed bridge understands dataclasses (and, in flight, TypedDict);
a pydantic model passed to or returned from an `@expose`d function is
not part of the contract. This decision designs support: how models are
detected without making pydantic a dependency, where the TypeScript
types come from, which names cross the wire, how validation maps onto
the protocol's error shape, what results look like, what it costs, and
what is explicitly out of scope for v1.

Eight questions, answered with spike evidence below.

## Evidence index

All in `spikes/p0-pydantic` (pydantic 2.14.4, CPython 3.15; ratios
matter, not absolute timings):

- `q1_detection.py` — detection never imports pydantic; v1/v2 anatomy.
- `q2_schema_source.py` — the two schema sources compared on aliases,
  Optional/defaults, unions, recursion, generics, constraints,
  Literal/Enum, datetime/bytes.
- `q3_q4_q5_wire.py` — alias round-trip; strict/lax/json-mode
  validation matrix; `ValidationError.errors()` anatomy;
  `computed_field`/`model_serializer`/`field_serializer` dump shapes;
  BigInt interaction.
- `q6_bench.py` — validate/dump cost vs the dataclass path on a nested
  order model (10 line items, 4000 iterations, best of 5).

## 1. Dependency: detection without importing pydantic

**pylair must not require pydantic** — it stays a pure-stdlib core
dependency set. The design consequence: the bridge may only ever *read*
pydantic that the application itself already imported.

The spike proves a two-gate detection (`q1_detection.py`):

1. `sys.modules.get("pydantic")` — if the app never imported pydantic,
   no class in the process can be a `BaseModel` subclass, so the answer
   is cheaply "no". Crucially, this gate **never triggers the import**:
   verified in the spike — detection of a plain class leaves
   `"pydantic" not in sys.modules` true.
2. `issubclass(cls, mod.BaseModel)` against the already-loaded module —
   correct for any subclass depth, zero cost.

**Pydantic v1**: v1 models carry `__fields__` and lack `model_fields`;
v2 models carry `model_fields` (with `__fields__` present only as a
deprecated compat shim). Anatomy is therefore distinguishable without
installing anything. **Recommendation: a clear unsupported error, not
silent degrade.** Silent degradation would mean treating a v1 model as
an opaque object — the user gets a useless `object` TS type and finds
out at runtime. The error names the cause and the fix: *"pydantic v1
model `User` — the bridge supports pydantic v2; upgrade with
`pip install -U pydantic` (v1 and v2 can coexist during migration via
`pydantic.v1`)".* v1 is end-of-life (June 2024) and v1/v2 differ in
exactly the machinery this design depends on (`model_fields`, strict
validation semantics, `model_dump`); supporting both doubles the
surface for a shrinking user population.

## 2. Schema source: `model_fields` through pylair's own walker

The spike (`q2_schema_source.py`) compared both sources across every
feature the bridge cares about. The decisive rows:

| question codegen must answer | `model_fields` | `model_json_schema()` |
|---|---|---|
| exact annotation objects (live types) | yes | no — serialized strings |
| validation vs serialization alias **split** | yes, both | one name per render — needs two renders and still loses the pairing |
| bigint vs int distinction | yes — same `int` annotation pylair already reads | **no — both become `"integer"`** |
| bytes vs str | yes | `"format": "binary"` hint only |
| constraints (`conint`/`constr`/`Field`) | yes, as objects | yes, as JSON keywords |
| unions / discriminated unions | manual walk of `typing` objects | yes, with discriminator metadata |
| recursive models | same walk pylair already does for dataclasses | `$ref`/`$defs` machinery |
| generics | **concretized for free** — `IntBox.item` reads as `int` | also concretized |
| datetime | exact type object | `"format": "date-time"` string |

**Recommendation: `model_fields` through pylair's own schema walker.**
The bridge's existing contract types depend on distinctions JSON Schema
does not make: pylair has a BigInt wire encoding (`$bigint` tags) that
applies to `int`-annotated values, a bytes encoding, and datetime
handling — all keyed off the exact annotation object. `model_json_schema()`
flattens all of these into strings-plus-format-hints, and rehydrating
them means re-implementing a type resolver on top of a lossy
serialization. The walker approach is also *consistent*: dataclasses,
TypedDict, and pydantic models then share one schema pipeline, one
codegen, one mental model — and the walk is trivially small because
pydantic has already resolved forward refs, concretized generics, and
split the aliases before we ever look. What we give up: pydantic's
discriminator metadata (walked manually — three lines) and its `$ref`
machinery (pylair's walker already recurses dataclasses the same way).

One more evidence detail: `model_json_schema(mode="serialization")` and
the default validation render **differ** (`userId` vs `user_id` in the
spike) — so the JSON-Schema route needs two renders per model plus a
merge step, and the merge is exactly what `model_fields` already is.

## 3. Wire names: by-alias both ways

Evidence (`q3_q4_q5_wire.py`, model with `validation_alias="user_id"`,
`serialization_alias="userId"`, `alias="fullName"`):

- validation accepts the validation alias (or, with
  `populate_by_name=True`, the python name too) — but **not** the
  serialization alias;
- `model_dump(mode="json", by_alias=True)` emits the serialization
  alias; `by_alias=False` emits python field names.

So the two directions can legitimately carry *different* names, and
picking "python field names" breaks any model that declares an alias —
those models exist precisely because their wire format is not the
python naming convention (camelCase frontends, legacy JSON).

**Recommendation: by-alias both ways, and it round-trips.**
IN: validate with the names the model declares for validation
(`validation_alias` or `alias`, falling back to the field name).
OUT: `model_dump(mode="json", by_alias=True)` — the names the model
declares for serialization. TS types use those same wire names. The
round-trip is then exact by construction: whatever shape the TS side
receives is the shape it may send back. When validation and
serialization aliases differ, the object literally has different key
names in each direction — that is the model author's explicit choice,
and the TS types must mirror it (request type vs response type can
differ, which codegen already supports structurally). `alias_generator`
needs no special-casing: pydantic resolves it into the per-field alias
values before the walker ever looks.

## 4. Validation in: strict, with JSON-mode allowances by pre-coercion

The spike's matrix (`q3_q4_q5_wire.py`) rules out both naive modes:

- **lax** (the default): `"1"` validates as `1` — silently. That is the
  canonical frontend type bug, and the bridge's whole value proposition
  is surfacing it as `INVALID_PARAMS` instead of letting it through.
- **strict** (`model_validate(..., strict=True)`): rejects ISO-8601
  datetime *strings* — but strings are the only way JSON can carry a
  datetime. Strict-as-shipped is unshippable.
- **`model_validate_json`**: not strict either — the spike shows it
  *still* coerces `"1"` to `1`, and it would mean re-serializing input
  pylair has already parsed.

**Recommendation: strict validation, with pylair pre-coercing the small
set of types JSON cannot express.** The bridge's annotation walk already
knows where `datetime`/`date`/`time`/`bytes`/`BigInt`-shaped values are
declared (it needs the same walk for codegen); before validation it
converts the wire representation (ISO strings, base64) into the real
objects at exactly those paths. Proven in the spike:

- pre-coerced strict: ISO datetime validates — OK;
- `"1"` as count after pre-coercion: still rejected (`int_type`) — the
  bug-hiding hole stays closed;
- int-for-float passes strict (correct — JSON itself does not
  distinguish, evidence in the probe);
- a malformed ISO string fails at the pre-coercion step with a clean
  `ValueError` — maps to `INVALID_PARAMS` at that path, same shape.

**Error mapping.** `ValidationError.errors()` entries carry
`{type, loc, msg, input, url}`; `loc` is a tuple like
`('items', 1, 'name')`. The mapping to the protocol shape (#193):

- `path` — `args.<param>` plus the loc walk: dotted for keys, `[i]` for
  indices — `args.order.items[1].name`, identical shape to the
  dataclass path;
- `received` — the JSON **type** of the offending input, never the
  value: `input` is read only to classify it and is then dropped. (The
  spike shows raw entries embed the full input — the mapping layer is
  the one place it exists, and it must not cross into the error data.)
- `expected` — **not** pydantic's machine tag: `errors()` types are
  validator-failure tags (`int_parsing`, `string_type`, `missing`), not
  the declared type #193's contract specifies. The expected type comes
  from looking up the annotation at the failure path in the same walk
  that produced the schema (falling back to the tag-derived hint when a
  custom validator fails mid-object). Custom validators (`Q4c`):
  their `ValueError("must be an even number")` surfaces as
  `type: value_error` with `msg` carrying the user's text — the human
  `message` field uses it verbatim ("Value error, must be an even
  number"), the machine shape comes from `loc` as usual.

## 5. Results out: `model_dump(mode="json", by_alias=True)`

Evidence (`q3_q4_q5_wire.py`):

- **`computed_field`** — included in the dump, and statically visible:
  `Model.model_computed_fields` carries the return annotation
  (`area -> int` in the spike). **Supported**: the TS type includes
  computed fields, read-only by nature.
- **`model_serializer`** — can emit literally any shape
  (`{"totally": "different", ...}` in the spike). There is nothing to
  walk: **unsupported in v1**, loudly — the schema generator raises
  with the model name and the reason when it detects one, rather than
  emitting a type that lies. (Escape hatch documented: annotate the
  function's return as a TypedDict/dataclass mirror of the real shape.)
- **`field_serializer`** — changes the wire type of one field (datetime
  became `15/01/2026`). Statically visible, but return annotations are
  often absent in practice. v1: honor it when a return annotation
  exists; absent one, the field degrades to its JSON-schema-emitted
  type with a documented-lossy note. Not a lie only because pydantic's
  own schema applies the same serializer — worth one test either way
  in implementation.
- **exclude/include** — `model_dump(exclude=...)` is a *call-site*
  choice; the bridge always dumps the full model. No interaction.
- **Pylair's own encodings** — `mode="json"` keeps python ints exact,
  so the `$bigint` tag wrapping applies to pydantic output exactly as
  it does to dataclass output — **consistent, not conflicting**: the
  tag layer runs after `model_dump`, in pylair's encoder, unchanged.
  Datetimes come out of `mode="json"` as ISO strings, the same wire
  shape the dataclass path produces. Bytes need the existing base64
  handling rather than pydantic's (pydantic emits utf-8-ish strings in
  json mode — the implementation must verify byte-fidelity in a test;
  worst case the bridge encodes bytes itself after the dump, before the
  tag pass).

## 6. Performance

`q6_bench.py`, nested order model (10 line items), best-of-5:

| operation | pydantic v2 | pylair dataclass path |
|---|---|---|
| validate IN | **3.8 µs** | 34.3 µs (`coerce_kwargs`) |
| dump OUT | **3.3 µs** (dict) / 7.9 µs (+ `json.dumps`) | 36.4 µs (`dumps`) |

pydantic's Rust core validates a realistic nested payload roughly **9x
faster** than the pure-python coercion walk, and even with JSON
serialization added the out path is ~4.5x faster. Absolute numbers are
microseconds — small next to IPC — but the ratio kills the performance
objection to adoption and even suggests a future where the dataclass
path borrows the same structure. Caveats honestly noted: pylair's path
does more work per call (BigInt tagging, depth checks, blob probing),
and the strict+pre-coercion layering adds the pre-pass cost on top of
pydantic's numbers — a small constant on the same order, not a
structural change.

## 7. Explicitly unsupported in v1

Each of these raises a named error at codegen/introspection time, not a
silent wrong type:

- **pydantic v1 models** — error names the model and the upgrade path.
- **`model_serializer` models** — output shape unknowable statically;
  error suggests the TypedDict-mirror escape hatch.
- **RootModel** — a model whose whole shape is one wrapped scalar
  conflicts with the bridge's object-shaped function boundary;
  re-evaluate with real user demand.
- **forward refs that never resolved** (`model_rebuild` never ran) —
  error tells the user to call it; pydantic itself can't validate them
  either.
- **arbitrary-types-allowed fields** (`arbitrary_types_allowed=True`) —
  no wire meaning; error names the field.
- **field-level `exclude` in serialization** that's statically
  invisible, and runtime `exclude=`/`include=` dump arguments — the
  bridge always dumps full models.

## 8. Implementation split and test plan

PR 1 — **runtime validation + wire behavior** (`python/pylair`):
detection (§1), model walking shared with codegen, strict+pre-coercion
validation (§4), error mapping onto #193's `data.errors`, dump pipeline
with `$bigint`/bytes pass-through (§5). Tests: spike-matrix-turned-unit-tests
(each evidence row becomes a test), error-shape parity with the
dataclass path, round-trip through the real runtime harness for aliased
and recursive models, unsupported-list errors.

PR 2 — **codegen + client types** (`packages/cli` codegen, TS emit):
wire names per §3 (including split-direction aliases), computed fields,
nesting/generics/recursion emission. Tests: golden TS output per model
feature, parity with the schema walker, scaffold typecheck.

PR 3 — **docs + integration**: protocol-v1 §8 amendment (models are a
schema source, same error contract), a models doc section, CHANGELOG.
Tests: docs index, an end-to-end exposed function taking and returning
a model in the reference-app style.

Ordering note: PR 1 is independently useful (correct runtime behavior
even before typed TS lands) and small; the TypedDict work in flight
touches the same walker and lands first — this design deliberately
builds on its shape rather than racing it.

## Alternatives considered

- **`model_json_schema()` as the schema source** — rejected in §2:
  loses bigint/bytes/datetime distinctions the wire depends on, needs
  two renders plus a merge for aliases, and reimplements a resolver
  over a lossy format.
- **lax validation** — rejected in §4: hides the exact frontend type
  bugs the bridge exists to surface.
- **field names instead of aliases** — rejected in §3: breaks every
  model that declares an alias, which is the population that needs this
  feature most.
- **silent v1 degradation** — rejected in §1: a wrong `object` type
  discovered at runtime is worse than a clear error at build time.
