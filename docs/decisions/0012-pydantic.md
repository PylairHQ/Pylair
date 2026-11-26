# 0012: Pydantic v2 models in the typed bridge

Status: accepted (spike: `spikes/p0-pydantic` — every claim below cites
output reproducible by running the spike scripts; see §Evidence index).
Accepted after the review-mandated strict-mode matrix (`q7`/`q8`) settled
§4; identical results on CPython 3.15 and the bundled 3.12.

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
- `q7_strict_matrix.py` — the review-mandated conversion matrix: every
  type × strict-python / strict-json / strict+pre-coercion, plus the
  stringly-typed bug cases.
- `q8_decoded_and_cost.py` — strict-python behavior on already-decoded
  objects, timedelta's accepted forms, the re-serialization cost
  comparison, the bytes-through-strict-json failure.

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

## 4. Validation in: strict Python validation + comprehensive pre-coercion

Settled by the review-mandated matrix (`q7_strict_matrix.py`,
`q8_decoded_and_cost.py` — identical outcomes on CPython 3.15 and the
bundled 3.12). The matrix covers int/str/bool/float plus datetime, date,
time, timedelta, UUID, Decimal, Enum-by-value, tuple, set/frozenset,
bytes and Path, each in its JSON-native wire form, under three candidate
designs.

**First, a correction this review cycle surfaced**: the original spike's
"`model_validate_json` still coerces `'1'` to `1`" was *lax* JSON mode
(the default). **Strict JSON mode — `model_validate_json(raw,
strict=True)` — is exactly the conversion table this design needs**: it
rejects every stringly-typed bug (`"1"` for int, `"true"` for bool,
`1` for str, `"1.5"` for float — all REJECT) while accepting the
JSON-native forms of non-JSON types (ISO datetimes, UUID strings,
Decimals, enum values, arrays for tuples/sets — all OK). The reviewer's
suspicion was correct and the matrix proves it.

**And yet strict JSON mode is not the design**, for architectural
reasons the matrix made decisive:

1. **pylair validation never sees JSON text.** The frame parser's
   `object_hook` decodes `$b64` → real `bytes` and `$bigint` → `BigInt`
   *at parse time*; validation receives a Python object graph.
   Validating from JSON means re-serializing that graph first.
2. **Bytes can't survive the round trip at all.** `json.dumps` raises
   TypeError on bytes — so a pre-pass is required *either way* — and
   worse, strict-JSON interprets any string as UTF-8 bytes literally:
   the base64 text `'aGVsbG8='` comes out as `b'aGVsbG8='` (the spike
   shows it), payload unrecoverable. Strict JSON mode is semantically
   wrong for bytes, not merely awkward.
3. **It costs 2.6-2.7x more** for the privilege: 9.7 µs vs 3.7 µs per
   nested-order validation on 3.15 (10.6 vs 3.9 on 3.12) — the
   re-serialization dominates.

Meanwhile strict **Python** mode on the decoded objects needs no help
for anything already decoded: bytes objects, `BigInt` (an `int`
subclass — passes strict `int` validation natively), datetime objects,
UUID/Decimal/tuple/timedelta/Path objects all validate as-is.

**The design: `model_validate(obj, strict=True)` on the decoded input,
after a pre-coercion walk converts the JSON-native wire forms at exactly
the annotated paths.** The walk is the same annotation walk codegen
already needs. Per the matrix, the pre-coercion list must cover every
type strict-Python rejects in its wire form — the full list, with its
wire form per type:

| type | wire form | pre-coercion |
|---|---|---|
| datetime / date / time | ISO-8601 strings | `fromisoformat` |
| timedelta | ISO-8601 duration (`"PT1H0.5S"`) — **not** seconds: seconds are rejected in *both* strict modes; the duration string is the one form strict-JSON accepted, so it is the canonical wire form | small duration parser (no stdlib one exists — an implementation cost honestly noted) |
| UUID | canonical string | `UUID(v)` |
| Decimal | string or number | `Decimal(str(v))` — string form preferred, lossless |
| Enum | its value | `Cls(v)` |
| tuple / set / frozenset | arrays | `tuple(v)` / `set(v)` / `frozenset(v)` |
| Path | string | `Path(v)` |
| bytes | — | **not in the list**: `$b64` is already decoded to real bytes at parse time, which strict mode accepts natively |
| BigInt | — | **not in the list**: already a `BigInt`/`int` object at parse time |

(The first spike iteration of this list had only datetime/date/time/
bytes/BigInt; the matrix caught tuple/set/frozenset and the
timedelta-seconds trap — strict-Python rejects arrays for all three and
seconds in both modes. That is exactly the hole review item 3 predicted.)

Stringly-typed input is still rejected after pre-coercion (`"1"` for int
fails with `int_type` — proven in the spike), correct typed input passes
in all modes, and int-for-float passes (correct — JSON itself does not
distinguish).

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
  custom validator fails mid-object). Pre-coercion failures (a malformed
  ISO datetime, a bad duration) raise before validation at the same
  path — they map into the identical shape. Custom validators: their
  `ValueError("must be an even number")` surfaces as
  `type: value_error` with `msg` carrying the user's text — the human
  `message` field uses it verbatim, the machine shape comes from `loc`.

## 5. Results out: json mode with sentinel byte-swapping

`model_dump(mode="json", by_alias=True)` — with declared bytes fields
exchanged for sentinel strings beforehand and restored afterwards. This
supersedes the earlier python-mode recommendation; the encoding matrix
below (spike-verified, pydantic 2.14.x) settled it:

| case | python mode | json mode | hybrid (python + to_jsonable_python) |
|---|---|---|---|
| bytes valid utf-8 | bytes object | utf-8 string — wrong | bytes object |
| bytes arbitrary | bytes object | **crash** UnicodeDecodeError | bytes object |
| HttpUrl / EmailStr / SecretStr / IPvAnyAddress / NameEmail | objects, not strings | correct JSON strings | **crash** — to_jsonable_python doesn't know them |
| datetime / UUID / Decimal | objects | correct JSON strings | correct — matches json exactly |
| `@field_serializer(when_used="json")` | **silently skipped** | honored | **silently skipped** |
| `PlainSerializer(when_used="json")` | **silently skipped** | honored | **silently skipped** |

Nothing may be silently different from pydantic's JSON output other than
bytes becoming `$b64`. Python mode fails that twice (silent serializer
skips, special types as objects) and the to_jsonable_python hybrid can't
even run on the special types. Json mode is the only correct base — so
bytes are handled around it:

- **No bytes in the annotation tree** (decoration-time analysis, cached):
  straight `model_dump(mode="json", by_alias=True)` — output identical
  to pydantic's own JSON.
- **Bytes present**: `model_copy(deep=True)`, every bytes/bytearray leaf
  swapped for a random-uuid sentinel string, json-mode dump of the copy,
  sentinels restored to real bytes in the output. pylair's encoder then
  applies `$b64` — the single intended divergence. The user's instance
  is never mutated. The serializer warnings the swap triggers are scoped-
  suppressed (they are correct from pydantic's perspective and noise here).
- **No silent fallthrough in the swap.** Writes into the deep copy use
  `__dict__` directly — the copy is ours, so bypassing `frozen` and
  `validate_assignment` is safe by construction, while `setattr` would
  silently leave bytes in place (utf-8-valid bytes would then cross as
  plain strings where TS expects Uint8Array). Frozen models, strict
  validate_assignment models and models nested in frozen parents all
  round-trip as `$b64` (tested with utf-8-valid and non-utf-8 payloads).
- **Serializer-owned bytes fields are excluded from the swap.** A bytes
  field with its own serializer (`@field_serializer`, `PlainSerializer`,
  `WrapSerializer`, Annotated serializers — detected at decoration as a
  function-plain/function-wrap serialization entry in the field's core
  schema) owns its representation: the serializer receives the real bytes
  and its output crosses exactly as json mode produces it (e.g. `.hex()`
  arrives as the hex string). Codegen types such fields from the
  serializer's return annotation, else unknown (PR 2).
- **Warning suppression is scoped** to the exact message the swap
  provokes (`Pydantic serializer warnings`, plain UserWarning — pydantic
  has no dedicated category for it); any other warning still surfaces.
- **Bytes inside computed-field or custom-serializer output** cannot be
  swapped (they don't exist before the dump): json mode raises
  UnicodeDecodeError, surfaced as a named TypeError telling the author to
  expose the payload as a declared bytes field.
- `computed_field` output included in the response shape as before;
  `model_serializer` remains loud-unsupported at decoration (§7).
- `$bigint` tagging composes after the dump as before: json mode passes
  Python ints through untouched, pylair's encoder tags the big ones.

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
does more work per call (BigInt tagging, depth checks, blob probing).
The strict+pre-coercion design of §4 measures 3.7-3.9 µs against 3.5 µs
lax — the pre-pass is noise, not a structural cost — while the rejected
strict-JSON alternative costs 9.7-10.6 µs because re-serialization
dominates (both measured on the same nested order, 3.15 and 3.12).

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
- **strict JSON mode** (`model_validate_json(raw, strict=True)`) — the
  right conversion table, architecturally impossible here: validation
  receives decoded objects not JSON text, bytes can't re-serialize at
  all (and lose their payload if forced through strings), and the round
  trip costs 2.6-2.7x. Evidence in `q7`/`q8`.
- **field names instead of aliases** — rejected in §3: breaks every
  model that declares an alias, which is the population that needs this
  feature most.
- **silent v1 degradation** — rejected in §1: a wrong `object` type
  discovered at runtime is worse than a clear error at build time.
