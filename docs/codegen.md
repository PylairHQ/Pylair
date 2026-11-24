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
- **Honest gaps**: unknown keys inside a structured value are ignored
  (a dataclass is built only from keys matching its fields), `Literal`
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
  a JS `bigint` (codegen emits `bigint`). Note the tag is opt-in:
  a plain `int` annotation still maps to `number`, and `pylair.BigInt`
  requires matching `pylair` and `@pylair/client` versions — an old
  client receiving a tag sees a plain `{"$bigint": ...}` object.
- Non-serializable results (numpy values, torch tensors, open objects)
  fail the *call* with `INTERNAL` — serialize to list/dict/bytes first.

## Emitted stubs

```ts
// GENERATED by pylair codegen — do not edit.
// Source module: myapp.backend
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
- Dataclasses dedupe into named `interface`s; unknown annotations emit
  `unknown` — never a guess.

## CLI

```
pylair codegen --module myapp.backend [--python path/to/python]
                   [--out src/api.generated.ts] [--check]
```

- `--check`: exit 1 if the file is stale (CI drift guard).
- Python resolution: `--python` > `PYTHON` env > `python3` on PATH. The
  app's own venv is the right interpreter in practice (M4 wires it).

## Non-goals (v1)

- Pydantic/attrs models, `TypedDict` — map as `unknown` for now.
- Overloads, generics beyond containers, Protocol classes → `unknown`.
- Watch mode — `dev` re-runs codegen on change later (M4).
- `*args`/`**kwargs` params emit `unknown` positions — expose explicit
  kwargs for a typed surface.
