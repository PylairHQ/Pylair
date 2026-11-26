/**
 * Type codegen: manifest (python -m pylair.codegen) -> api.generated.ts.
 * Spec: docs/codegen.md.
 */

export interface Schema {
  type: string;
  items?: Schema | Schema[];
  opts?: Schema[];
  keys?: Schema;
  /** dict: value schema; literal/enum: raw values */
  values?: Schema | unknown[];
  name?: string;
  fields?: { name: string; required: boolean; schema: Schema; computed?: boolean; serializer?: boolean }[];
  /** pydantic models with split wire shapes (decision 0012 §3): validation
   * names on input, serialization names on output */
  in_fields?: { name: string; required: boolean; schema: Schema; computed?: boolean; serializer?: boolean }[];
  out_fields?: { name: string; required: boolean; schema: Schema; computed?: boolean; serializer?: boolean }[];
}

export interface Param {
  name: string;
  kind: string;
  required: boolean;
  schema: Schema;
}

export interface Fn {
  name: string;
  kind: "sync" | "async" | "generator" | "isolated";
  doc: string | null;
  params: Param[];
  returns: Schema;
}

export interface Manifest {
  version: number;
  functions?: Fn[];
  error?: string;
}

const RESERVED = new Set(
  (
    "arguments await break case catch class const continue debugger default " +
    "delete do else enum eval export extends false finally for function if " +
    "implements import in instanceof interface let new null package private " +
    "protected public return static super switch this throw true try typeof " +
    "var void while with yield async of static get set"
  ).split(" "),
);

function ident(name: string): string {
  const cleaned = name.replace(/[^A-Za-z0-9_$]/g, "_");
  const base = /^[A-Za-z_$]/.test(cleaned) ? cleaned : `_${cleaned}`;
  return RESERVED.has(base) ? `${base}_` : base;
}

function camel(name: string): string {
  const out = ident(name.replace(/_+([a-zA-Z0-9])/g, (_, c) => c.toUpperCase()));
  return RESERVED.has(out) ? `${out}_` : out;
}

/** schema -> TS type expression. `dir` matters only for pydantic models
 * with split wire shapes: params use the input interface, results the
 * output one (decision 0012 §3). */
function ts(s: Schema | undefined, dir: "in" | "out" = "out"): string {
  if (!s) return "unknown";
  const inner = (x: Schema | undefined) => ts(x, dir);
  switch (s.type) {
    case "int":
    case "float":
      return "number";
    case "bigint":
      return "bigint";
    case "str":
    case "timedelta": // ISO-8601 duration string
    case "uuid": // canonical UUID string
    case "path":
      return "string";
    case "decimal":
      return "string"; // lossless string form; input also accepts number
    case "bool":
      return "boolean";
    case "none":
      return "null";
    case "any":
      return "unknown";
    case "bytes":
      return "Uint8Array";
    case "datetime":
      return "string"; // ISO 8601
    case "list": {
      const item = s.items as Schema;
      // Union-ish members need parens: list[int|str] is (number|string)[],
      // not number|string[] (which parses as number | (string[])).
      return item && ["union", "literal", "enum"].includes(item.type)
        ? `(${inner(item)})[]`
        : `${inner(item)}[]`;
    }
    case "tuple":
      return `[${((s.items as Schema[]) ?? []).map(inner).join(", ")}]`;
    case "dict":
      return !s.keys || s.keys.type === "str" || s.keys.type === "any"
        ? `Record<string, ${inner(s.values as Schema)}>`
        : "unknown"; // non-string keys can't round-trip through JSON objects
    case "union":
      return (s.opts ?? []).map(inner).join(" | ") || "unknown";
    case "literal":
    case "enum":
      return (s.values as unknown[] ?? []).map((v) => JSON.stringify(v)).join(" | ") ||
        "unknown";
    case "dataclass":
    case "typeddict":
      return ident(s.name ?? "Anonymous");
    case "pydantic": {
      const base = ident(s.name ?? "Anonymous");
      // split shapes: params speak the validation names, results the
      // serialization ones — the two interfaces are distinct types
      return s.in_fields && dir === "in" ? `${base}Input` : base;
    }
    default:
      return "unknown";
  }
}

/** Recursively collect every dataclass/TypedDict schema reachable from a
 * schema — both are emitted as interfaces, deduped by name. */
function collectDataclasses(s: Schema | undefined, out: Map<string, Schema>): void {
  if (!s) return;
  if ((s.type === "dataclass" || s.type === "typeddict" || s.type === "pydantic") && s.name) {
    if (!out.has(s.name)) {
      out.set(s.name, s);
      for (const f of s.fields ?? []) collectDataclasses(f.schema, out);
      for (const f of s.in_fields ?? []) collectDataclasses(f.schema, out);
      for (const f of s.out_fields ?? []) collectDataclasses(f.schema, out);
    }
    return;
  }
  // items: Schema | Schema[]; keys/values: Schema (dict) or primitives
  // (literal/enum) — the object guard sorts them out.
  for (const sub of [s.items, s.keys, s.values].flat()) {
    if (sub && typeof sub === "object") collectDataclasses(sub as Schema, out);
  }
  for (const o of s.opts ?? []) collectDataclasses(o, out);
}

/** The doc line lands inside a `/** ... *​/` comment — `*​/` would end it
 * early and emit live code, control/line-separator chars break the
 * single line, and a trailing backslash must not run into the close. */
function docComment(doc: string): string {
  return doc
    .replace(/\p{Cc}|\p{Cf}|\p{Zl}|\p{Zp}/gu, " ")
    .replace(/\*\//g, "*\\/")
    .replace(/\\+$/, " ");
}

/** Render the manifest as a TypeScript module. */
export function emit(m: Manifest, source: string): string {
  if (m.error) throw new Error(`manifest error: ${m.error}`);
  const fns = m.functions ?? [];

  const dataclasses = new Map<string, Schema>();
  for (const f of fns) {
    for (const p of f.params) collectDataclasses(p.schema, dataclasses);
    collectDataclasses(f.returns, dataclasses);
  }

  // Wire encoding ({"$b64": ...}, nested transforms) lives inside
  // @pylair/client's call/stream — generated stubs stay thin calls so
  // wire fixes ship with the client package, never by regenerating apps.
  const out: string[] = [
    `// GENERATED by pylair codegen from the source module below — do not edit.`,
    `// Source module: ${source}`,
    `// Ephemeral by design: regenerated by \`pylair dev\` and \`pylair build\`,`,
    `// gitignored, never committed.`,
    // Aliased imports: a Python function or param can be named `call`,
    // `stream` or `CallOptions` — un-aliased imports would collide or be
    // shadowed by generated identifiers.
    `import { call as pylairCall, stream as pylairStream, type CallOptions as PylairCallOptions } from "@pylair/client";`,
    "",
  ];

  for (const dc of dataclasses.values()) {
    // TypedDict keys are wire names — kept exactly: valid identifiers
    // emit bare (reserved words are legal property names in TS), anything
    // else emits quoted. Dataclass fields keep the historical ident().
    const keyOf =
      dc.type === "typeddict" || dc.type === "pydantic"
        ? (k: string) => (/^[A-Za-z_$][A-Za-z0-9_$]*$/.test(k) ? k : JSON.stringify(k))
        : ident;
    if (dc.type === "pydantic" && dc.in_fields) {
      // split wire shapes (decision 0012 §3): FooInput is what params
      // accept (validation aliases, optional keys for defaulted fields),
      // Foo what results contain (serialization aliases, computed fields,
      // serializer-typed fields — everything pydantic always dumps).
      const renderFields = (
        fields: NonNullable<Schema["in_fields"]>,
        dir: "in" | "out",
      ) =>
        fields.map((f) => {
          const note = f.computed
            ? " // computed field — output only, never send"
            : f.serializer
              ? " // custom serializer owns this field's wire shape"
              : "";
          return `  ${keyOf(f.name)}${f.required ? "" : "?"}: ${ts(f.schema, dir)};${note}`;
        });
      const inLines = renderFields(dc.in_fields, "in");
      const outLines = renderFields(dc.out_fields ?? [], "out");
      out.push(
        `export interface ${ident(dc.name!)}Input {\n${inLines.join("\n")}\n}\n`,
        `export interface ${ident(dc.name!)} {\n${outLines.join("\n")}\n}\n`,
      );
      continue;
    }
    const dir = dc.type === "pydantic" ? "out" : "out";
    const lines = (dc.fields ?? []).map(
      (f) => `  ${keyOf(f.name)}${f.required ? "" : "?"}: ${ts(f.schema, dir)};`,
    );
    out.push(`export interface ${ident(dc.name!)} {\n${lines.join("\n")}\n}\n`);
  }

  const fnames = new Set<string>();
  for (const f of fns) {
    if (f.doc) out.push(`/** ${docComment(f.doc.split("\n")[0])} */`);
    // *args/**kwargs can't be expressed as named args over the bridge
    // (the wire input is a kwargs dict) — drop them from the stub.
    const vparams = f.params.filter((p) => !p.kind.includes("VAR_"));
    // TS forbids an optional param before a required one, but Python
    // allows a defaulted positional before a required keyword-only arg.
    // Those become required `T | undefined` params — callers pass
    // undefined explicitly rather than getting a signature that lies.
    const requiredAfter = vparams.map((_, i) =>
      vparams.slice(i + 1).some((q) => q.required),
    );
    // Param idents must stay unique after cleaning (foo-bar vs foo_bar),
    // and the appended CallOptions param must dodge a Python param
    // literally named `opts`.
    const taken = new Set<string>();
    const pname = vparams.map((p) => {
      let n = ident(p.name);
      while (taken.has(n)) n = `${n}_`;
      taken.add(n);
      return n;
    });
    let optsName = "opts";
    while (taken.has(optsName)) optsName = `${optsName}_`;
    const params = vparams.map((p, i) =>
      !p.required && requiredAfter[i]
        ? `${pname[i]}: ${ts(p.schema, "in")} | undefined`
        : `${pname[i]}${p.required ? "" : "?"}: ${ts(p.schema, "in")}`,
    );
    params.push(`${optsName}?: PylairCallOptions`);
    const inputObj = vparams.length
      ? `{ ${vparams.map((p, i) => `${JSON.stringify(p.name)}: ${pname[i]}`).join(", ")} }`
      : "{}";
    let fname = camel(f.name);
    while (fnames.has(fname)) fname = `${fname}_`;
    fnames.add(fname);
    if (f.kind === "generator") {
      const item = ts(f.returns);
      out.push(
        `export function ${fname}(${params.join(", ")}): AsyncGenerator<${item}, void, void> {`,
        `  return pylairStream<${item}>(${JSON.stringify(f.name)}, ${inputObj}, ${optsName});`,
        `}`,
        "",
      );
    } else {
      const ret = ts(f.returns);
      out.push(
        `export function ${fname}(${params.join(", ")}): Promise<${ret}> {`,
        `  return pylairCall<${ret}>(${JSON.stringify(f.name)}, ${inputObj}, ${optsName});`,
        `}`,
        "",
      );
    }
  }
  return out.join("\n");
}
