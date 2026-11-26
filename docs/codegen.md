# M2: Type Codegen — Python signatures → TypeScript client stubs

`pylair codegen` generates a typed `api.generated.ts` from the app's
`@expose`d functions, so the front end calls Python with compile-time
types instead of raw `call("name", {...})` strings.

## Pipeline

```
app module --(import)--> @expose registry --(_schema.py)--> manifest JSON
    `--(packages/cli emit)--> api.generated.ts --> wraps call/stream
```

1. **Python** emits a manifest: `python -m pylair.codegen <module>`
   prints JSON to stdout (imports the module; decorators fill the
   registry; annotations are mapped to schema).
2. **CLI** spawns that against the app's interpreter, then runs the TS
   emitter: `pylair codegen --module myapp.backend --out src/api.generated.ts`.

The same schema also rides in the `hello.exposes[].signature` metadata,
so the handshake and the stubs share one source of truth.

## Who runs codegen, and when

`src/api.generated.ts` is **ephemeral by design**: gitignored, never
committed, regenerated from the python source of truth. Three producers,
one output:

- **`pylair dev`** writes it once before the frontend dev server starts
  (not only on change), then rewrites it on every python save.
- **`pylair build`** writes it before the frontend build/typecheck inside
  the installer build, importing the app module with the *bundled*
  interpreter and the *staged* site-packages — the same environment the
  smoke test uses, so the import needs no venv and works on a fresh clone
  or CI machine. An import failure fails the build with the python error
  named: shipping stubs that are stale or absent is never right, because
  the frontend typecheck is what proves the bridge types line up.
- **`pylair codegen`** (this CLI) writes it on demand for editor setup or
  the `--check` drift guard.

The consequence for app repos: do not commit the file, do not edit it,
and do not wire your own codegen step into the frontend build — the build
already owns it.

## Manifest

```json
{
  "version": 1,
  "functions": [
    {
      "name": "add", "kind": "sync", "doc": "adds numbers",
      "params": [{"name": "a", "required": true, "schema": {"type": "int"}}],
      "returns": {"type": "int"}
    }
  ]
}
```

`kind` is the existing handshake kind (`sync | async | generator |
isolated`). Generators put their *yield* type in `returns`.

## Schema (`_schema.py`)

Annotation → `{"type": ...}` JSON:

| Python                        | schema                     | TS emit            |
|-------------------------------|----------------------------|--------------------|
| `int`, `float`                | `int` / `float`            | `number`           |
| `pylair.BigInt`               | `bigint`                   | `bigint`           |
| `str`                         | `str`                      | `string`           |
| `bool`                        | `bool`                     | `boolean`          |
| `None`                        | `none`                     | `null`             |
| `bytes`, `bytearray`          | `bytes`                    | `Uint8Array`†      |
| `datetime/date/time`          | `datetime`                 | `string` (ISO)     |
| `list[T]`, `Sequence`, `Iterable`, `set` | `list` + `items`  | `T[]`              |
| `tuple[A,B]` / `tuple[T,...]` | `tuple` + `items`          | `[A,B]` / `T[]`    |
| `dict[str,T]`, `Mapping`      | `dict` + `values`          | `Record<string,T>` |
| `Optional[T]` / `T \| None`   | `union` + `opts`           | `T \| null`        |
| `Union[A,B]`                  | `union` + `opts`           | `A \| B`           |
| `Literal[...]`                | `literal` + `values`       | literal union      |
| `Enum`                        | `enum` + `values`          | literal union      |
| `@dataclass`                  | `dataclass` + `name`,`fields` | `interface`     |
| `TypedDict`                   | `typeddict` + `name`,`fields` | `interface`     |
| `BaseModel` (pydantic v2)     | `pydantic` + `name`,`fields` or `in_fields`/`out_fields` | `interface` (two when shapes split) |
| `timedelta`                   | `timedelta`                 | `string` (ISO-8601 duration) |
| `UUID`                        | `uuid`                      | `string`        |
| `Decimal`                     | `decimal`                   | `string`        |
| `Path`                        | `path`                      | `string`        |
| enum member types             | `enum` + `values`           | union of literal values |
| `tuple[A, B]`                 | `tuple` + `items`           | fixed-length tuple `[A, B]` |
| `set`/`frozenset`             | `list`                      | `T[]` — order is NOT guaranteed |
| `AsyncGenerator[X,_]`         | yield `X` in `returns`     | `stream<X>`        |
| missing / anything else       | `any`                      | `unknown`          |

† bytes travel the wire base64-tagged (`{"$b64": "..."}` — see Wire JSON
below); `@pylair/client` converts `Uint8Array` ↔ base64 inside
`call`/`stream`, so callers and stubs never touch the encoding.

## Wire JSON (`_json.py`)

Plain `json.dumps` cannot serialize `bytes`, `datetime`, or dataclass
instances — without this, emitted types would lie about what crosses the
wire. The runtime now encodes:

- `bytes`/`bytearray` → `{"$b64": "<base64>"}` and back (`object_hook`)
- `pylair.BigInt` → `{"$bigint": "<decimal>"}` and back — a JS `bigint`
  argument encodes the same way (`toWire`), and both sides decode the
  tag defensively: only `{"$bigint": s}` where `s` matches
  `^-?(0|[1-9][0-9]*)$` becomes a big integer, anything else stays
  plain data, and payloads over 4096 digits are rejected outright.
  **The declared type decides:** under a `-> pylair.BigInt` annotation
  (or `Optional`/`list[BigInt]`, or `AsyncGenerator[BigInt, _]` yield
  schema for streams), plain `int` results and chunks are widened to
  `BigInt` before encoding — int arithmetic drops the subclass, so
  `a + b` still tags. A non-int result (str, float, bool) keeps the
  value's own encoding — the signature only widens ints.
- `@dataclass` → `dataclasses.asdict` (params reconstruct by annotation)
- `datetime`/`date`/`time` → ISO 8601 string (params stay strings; Python
  accepts them via annotation coercion where the arg is annotated)
- `Enum` → its value; `set`/`frozenset` → list

**Param validation + coercion** (`coerce_kwargs`): before dispatch,
each incoming kwarg is checked against the signature — annotated
parameters strictly, on every call mode including isolated workers:

- **Primitives**: `int` accepts ints and whole-number floats (JSON
  can't tell `3` from `3.0`); `float` accepts int/float; `str` accepts
  strings; `bool` accepts booleans. `bool` is never a valid `int` or
  `float`. `pylair.BigInt` accepts decoded `$bigint` tags and plain
  JSON ints (promoted, so the fn sees the annotated type). `Optional[T]`/`Union` accepts any matching arm; `null` only
  where `None` is an arm.
- **Structured types**: dict → dataclass, `list[T]`/`dict[K,V]` recurse
  element validation, `Optional`/`Union` tries arms in order, enums
  rebuild by value, ISO strings become `datetime`, base64 → `bytes`. A
  reconstruction failure is `InvalidParams` → wire `INVALID_PARAMS`.
- **Arity**: a missing required parameter or an unexpected extra key is
  `INVALID_PARAMS` naming the parameter (`**kwargs` permits extras).
- **Unchecked**: unannotated parameters and annotations with no wire
  mapping pass the raw JSON value through — Python is duck-typed and
  the function may still handle it; failures there surface as
  `INTERNAL` as usual. Scalar coercion (e.g. `"5"` → `int`) is *not*
  attempted — a wrong primitive is rejected, not converted.
- **TypedDicts**: a `TypedDict` annotation (typing or typing_extensions)
  validates the incoming object against the class: every key's value is
  coerced per its declared type (nested dataclasses, `BigInt`, datetimes
  all work inside), missing required keys and wrong types are
  `INVALID_PARAMS` with the exact `data.errors` path
  (`args.opts.limit`), and **unknown keys are rejected** — stricter than
  dataclasses on purpose: a TypedDict is a closed wire shape, so an extra
  key is a caller bug, not forward compatibility. The value stays a
  plain dict — there is no class to construct. `total=False`,
  `Required[]`/`NotRequired[]` and inheritance between TypedDicts are
  respected (requiredness comes from `__required_keys__`); `ReadOnly[]`
  is accepted with no runtime effect. On the way out the declared return
  type drives `BigInt` widening inside the dict like any other
  container. Generic subscription (`Page[int]`) is a clear error — there
  is no type-parameter machinery — and recursive TypedDicts error the
  same way, because the schema embeds fields inline and a cycle can't
  terminate (recursive dataclasses are unsupported for the same reason).
- **Honest gaps**: unknown keys inside a *dataclass* value are ignored
  (it is built only from keys matching its fields — TypedDicts reject
  them instead, see above), `Literal`
  annotations are mapped for codegen but not validated at the boundary,
  and a `None` default admits `null` even without `Optional`
  (`x: str = None`).

All boundary failures are caller bugs reported before any task is
spawned: `INVALID_PARAMS` (`-32602`), never an `INTERNAL` `TypeError`
from inside the function.

## Limits to know

- **NaN/Infinity are not valid JSON.** `json.dumps(allow_nan=False)`
  rejects them at encode time — a model returning a NaN score fails
  that one call with `INTERNAL`, never the runtime (protocol §2). Use
  `Optional[float]` and return `None` for missing scores.
- **Python `int` is unbounded; JS `number` is not.** Integers beyond
  ±2^53 on a plain `int` annotation fail the call with `INTERNAL` —
  the wire keeps full precision, but `JSON.parse` would round
  silently, so the codec rejects them first and names the way out.
  For exact big integers (ids, counts, hashes) opt in by *type*:
  annotate or return `pylair.BigInt` — it crosses as
  `{"$bigint": "<decimal>"}` regardless of magnitude and comes back as
  a JS `bigint` (codegen emits `bigint`). Annotating the return is
  enough for arithmetic: `-> pylair.BigInt` results are widened from
  plain ints before encoding, so `return a + b` tags without
  re-wrapping. Note the tag is opt-in: a plain `int` annotation still
  maps to `number` (and still errors out of range), and `pylair.BigInt`
  requires matching `pylair` and `@pylair/client` versions — an old
  client receiving a tag sees a plain `{"$bigint": ...}` object.
- Non-serializable results (numpy values, torch tensors, open objects)
  fail the *call* with `INTERNAL` — serialize to list/dict/bytes first.

## Emitted stubs

```ts
// GENERATED by pylair codegen from the source module below — do not edit.
// Source module: myapp.backend
// Ephemeral by design: regenerated by `pylair dev` and `pylair build`,
// gitignored, never committed.
import { call, stream, type CallOptions } from "@pylair/client";

export interface Foo { name: string; count: number; }

export function makeFoo(name: string, count?: number, opts?: CallOptions):
    Promise<Foo> {
  return call<Foo>("make_foo", { name, count }, opts);
}

export function ticks(n: number, opts?: CallOptions) {
  return stream<number>("ticks", { n }, opts);
}
```

Stubs are deliberately thin — the wire transform (`$b64` tagging,
nested decode) lives inside `call`/`stream` in `@pylair/client`, so a
future wire fix ships with a client upgrade, not a regeneration of
every app's generated file. See `packages/cli/golden/` for real output.

Rules:

- snake_case → camelCase export names; the wire string stays snake_case.
- Positional Python params become positional TS params; `has_default` →
  `?`. `CallOptions` (abort signal) is appended last.
- `kind: "generator"` → `stream<T>` (async iterator + credit pull);
  everything else → `call<T>` (Promise). `isolated` is transparent.
- TS reserved-word param names get a `_` suffix; the wire key is unchanged.
- Dataclasses and TypedDicts dedupe into named `interface`s; a TypedDict's
  optional keys emit `key?: T` and its keys are kept exactly as written —
  they are wire names, so there is no camelCasing and no renaming of
  reserved words (property names can't collide in TS); keys that aren't
  valid identifiers emit quoted (`"foo-bar": T`). Unknown annotations emit
  `unknown` — never a guess.

## CLI

```
pylair codegen --module myapp.backend [--python path/to/python]
                   [--out src/api.generated.ts] [--check]
```

- `--check`: exit 1 if the file is stale (CI drift guard).
- Python resolution: `--python` > `PYTHON` env > `python3` on PATH. `pylair
  dev` uses the app's own venv; `pylair build` uses the bundled interpreter
  (see below).

## Non-goals (v1)

- Pydantic/attrs models — map as `unknown` for now.
- Overloads, generics beyond containers, Protocol classes → `unknown`.
- ~~Watch mode~~ — done: `pylair dev` regenerates the stubs on every python
  save, and `pylair build` regenerates them before the frontend build.
- `*args`/`**kwargs` params emit `unknown` positions — expose explicit
  kwargs for a typed surface.

## Using Pydantic models

Exposed functions may take and return pydantic v2 models when the app
uses pydantic (pylair never requires or imports it — detection reads
`sys.modules` only). What codegen emits:

- **One interface per model**, deduped by name like dataclasses. Keys
  are wire names — aliases exactly as they cross the wire, never the
  Python field names.
- **Split shapes**: when validation and serialization aliases differ,
  params use `FooInput` (validation aliases, defaulted fields optional)
  and results use `Foo` (serialization aliases, everything pydantic
  always dumps required). Symmetric models emit a single `Foo`.
- **computed_field** entries appear on the output interface only, with a
  comment — never send them back.
- **Serializer-owned fields** (a bytes field with its own
  `@field_serializer`, and similar) are typed from the serializer's
  return annotation when statically known, else `unknown` — the field's
  annotation would lie about the wire shape.
- **Validation is strict**: the bridge pre-coerces the JSON-native wire
  forms (ISO datetimes and durations, UUID strings, enum values, arrays
  for tuples/sets) and then validates with `model_validate(strict=True)`
  — stringly-typed mistakes like `"1"` for an int are rejected, same as
  the dataclass path, with structured `INVALID_PARAMS` errors.
- **Generics** arrive concretized (`IntBox.item` is `int` in the schema)
  — no type parameters cross the wire.

Unsupported in v1, each a named error at decoration time: pydantic v1
models (including the `pydantic.v1` shim), `RootModel`, `@model_serializer`
(the output shape is statically unknowable), unresolved forward refs,
and bytes emitted by computed fields or custom serializers (JSON cannot
carry bytes outside declared fields — declare a bytes field instead).

