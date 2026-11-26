import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdtempSync, writeFileSync, readFileSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";
import { emit } from "../dist/codegen.js";
import { loadManifest } from "../dist/manifest.js";

const FN = (over) => ({
  name: "f",
  kind: "sync",
  doc: null,
  params: [],
  returns: { type: "none" },
  ...over,
});
const M = (functions) => ({ version: 1, functions });

test("emits plain call wrappers", () => {
  const ts = emit(
    M([
      FN({
        name: "add_them",
        params: [
          { name: "a", kind: "P", required: true, schema: { type: "int" } },
          { name: "b", kind: "P", required: true, schema: { type: "float" } },
        ],
        returns: { type: "float" },
      }),
    ]),
    "myapp",
  );
  assert.match(ts, /export function addThem\(a: number, b: number, opts\?: PylairCallOptions\): Promise<number>/);
  assert.match(ts, /pylairCall<number>\("add_them", \{ "a": a, "b": b \}, opts\)/);
  // no wire helpers pasted into generated files — they live in the client
  assert.doesNotMatch(ts, /_toWire|_fromWire|_mapStream|b64encode/);
});

test("optional params become ?", () => {
  const ts = emit(
    M([
      FN({
        params: [
          { name: "a", kind: "P", required: true, schema: { type: "int" } },
          { name: "b", kind: "P", required: false, schema: { type: "str" } },
        ],
      }),
    ]),
    "m",
  );
  assert.match(ts, /a: number, b\?: string/);
});

test("generators emit stream + async generator", () => {
  const ts = emit(
    M([FN({ name: "ticks", kind: "generator", returns: { type: "int" } })]),
    "m",
  );
  assert.match(ts, /AsyncGenerator<number, void, void>/);
  assert.match(ts, /pylairStream<number>\("ticks"/);
});

test("dataclasses dedupe into interfaces", () => {
  const point = {
    type: "dataclass",
    name: "Point",
    fields: [
      { name: "x", required: true, schema: { type: "float" } },
      { name: "y", required: true, schema: { type: "float" } },
      { name: "label", required: false, schema: { type: "str" } },
    ],
  };
  const ts = emit(
    M([
      FN({ name: "a", params: [{ name: "p", kind: "P", required: true, schema: point }], returns: point }),
      FN({ name: "b", returns: point }),
    ]),
    "m",
  );
  assert.equal(ts.match(/export interface Point/g).length, 1);
  assert.match(ts, /x: number;\n  y: number;\n  label\?: string;/);
});

test("nested dataclass fields are emitted", () => {
  const inner = {
    type: "dataclass",
    name: "Inner",
    fields: [{ name: "v", required: true, schema: { type: "int" } }],
  };
  const outer = {
    type: "dataclass",
    name: "Outer",
    fields: [{ name: "inner", required: true, schema: inner }],
  };
  const ts = emit(M([FN({ returns: outer })]), "m");
  assert.match(ts, /export interface Outer \{\n  inner: Inner;\n\}/);
  assert.match(ts, /export interface Inner \{\n  v: number;\n\}/);
});

test("typeddicts emit interfaces with exact wire keys", () => {
  const opts = {
    type: "typeddict",
    name: "SearchOpts",
    fields: [
      { name: "limit", required: true, schema: { type: "int" } },
      { name: "label", required: false, schema: { type: "str" } },
      // valid TS identifiers emit bare even when reserved — property
      // names can't collide; wire fidelity beats cosmetic renaming.
      { name: "class", required: false, schema: { type: "str" } },
    ],
  };
  const ts = emit(M([FN({ params: [{ name: "o", kind: "P", required: true, schema: opts }] })]), "m");
  assert.match(ts, /export interface SearchOpts \{\n  limit: number;\n  label\?: string;\n  class\?: string;\n\}/);
});

test("typeddict keys that are not identifiers emit quoted", () => {
  // Functional-syntax TypedDicts can carry arbitrary string keys —
  // they are wire names, so quoting keeps them exact.
  const weird = {
    type: "typeddict",
    name: "Weird",
    fields: [
      { name: "foo-bar", required: true, schema: { type: "int" } },
      { name: "9lives", required: false, schema: { type: "str" } },
      { name: "with space", required: false, schema: { type: "bool" } },
    ],
  };
  const ts = emit(M([FN({ returns: weird })]), "m");
  assert.match(ts, /"foo-bar": number;/);
  assert.match(ts, /"9lives"\?: string;/);
  assert.match(ts, /"with space"\?: boolean;/);
});

test("typeddicts dedupe like dataclasses, across kinds", () => {
  const opts = {
    type: "typeddict",
    name: "Opts",
    fields: [{ name: "limit", required: true, schema: { type: "int" } }],
  };
  const inner = {
    type: "typeddict",
    name: "Inner",
    fields: [{ name: "v", required: true, schema: { type: "int" } }],
  };
  const outer = {
    type: "typeddict",
    name: "Outer",
    fields: [{ name: "inner", required: true, schema: inner }],
  };
  const ts = emit(
    M([
      FN({ name: "a", params: [{ name: "o", kind: "P", required: true, schema: opts }], returns: opts }),
      FN({ name: "b", returns: opts }),
      FN({ name: "c", returns: outer }),
    ]),
    "m",
  );
  assert.equal(ts.match(/export interface Opts/g).length, 1);
  assert.match(ts, /export interface Outer \{\n  inner: Inner;\n\}/);
  assert.match(ts, /export interface Inner \{\n  v: number;\n\}/);
});

test("type mapping", () => {
  const cases = [
    [{ type: "list", items: { type: "int" } }, "number\\[\\]"],
    [{ type: "tuple", items: [{ type: "int" }, { type: "str" }] }, "\\[number, string\\]"],
    [{ type: "dict", keys: { type: "str" }, values: { type: "int" } }, "Record<string, number>"],
    [{ type: "dict", keys: { type: "int" }, values: { type: "int" } }, "unknown"],
    [{ type: "bigint" }, "bigint"],
    [{ type: "int" }, "number"],
    [{ type: "union", opts: [{ type: "bigint" }, { type: "none" }] }, "bigint \\| null"],
    [{ type: "list", items: { type: "bigint" } }, "bigint\\[\\]"],
    [{ type: "union", opts: [{ type: "str" }, { type: "none" }] }, "string \\| null"],
    [{ type: "literal", values: ["a", "b"] }, '"a" \\| "b"'],
    [{ type: "enum", values: ["red", "blue"] }, '"red" \\| "blue"'],
    [{ type: "bytes" }, "Uint8Array"],
    [{ type: "datetime" }, "string"],
    [{ type: "any" }, "unknown"],
  ];
  for (const [schema, want] of cases) {
    const ts = emit(
      M([FN({ params: [{ name: "x", kind: "P", required: true, schema }] })]),
      "m",
    );
    assert.match(ts, new RegExp(`x: ${want}`), JSON.stringify(schema));
  }
});

test("reserved words get a suffix but keep wire name", () => {
  const ts = emit(
    M([
      FN({
        name: "f",
        params: [{ name: "class", kind: "P", required: true, schema: { type: "str" } }],
      }),
    ]),
    "m",
  );
  assert.match(ts, /class_: string/);
  assert.match(ts, /"class": class_/);
});

test("docs become jsdoc, manifest errors throw", () => {
  const ts = emit(M([FN({ doc: "hello\nworld" })]), "m");
  assert.match(ts, /\/\*\* hello \*\//);
  assert.throws(() => emit({ version: 1, error: "boom" }, "m"), /boom/);
});

test("a docstring containing */ cannot break the generated file", async () => {
  const ts = emit(
    M([FN({ doc: "does x */ evil(); return 0" })]),
    "m",
  );
  // The emitted file must still parse — an unescaped */ would end the
  // doc comment early and leave code + a stray */ behind.
  const { createSourceFile, ScriptTarget } = await import("typescript");
  const sf = createSourceFile("gen.ts", ts, ScriptTarget.Latest, true);
  assert.equal(sf.parseDiagnostics.length, 0, `unparseable output:\n${ts}`);
  // no unescaped */ survives inside the emitted doc line
  const docLine = ts.split("\n").find((l) => l.includes("evil"));
  assert.ok(docLine, "doc text should still be present");
  assert.doesNotMatch(docLine, /\*\/[^/]*evil/, "unescaped */ ends the comment");
});

test("control characters and a trailing backslash stay inert", () => {
  const ts = emit(M([FN({ doc: "tab\vhere\\\rmore" })]), "m");
  for (const l of ts.split("\n")) {
    if (l.includes("evil")) continue;
    assert.doesNotMatch(l, /[\x00-\x08\x0b-\x1f\u2028\u2029]/);
  }
});

test("snake_case names stay the wire name", () => {
  const ts = emit(M([FN({ name: "make_thing" })]), "m");
  assert.match(ts, /export function makeThing\(/);
  assert.match(ts, /pylairCall<null>\("make_thing", \{\}, opts\)/);
});

// ---- P2-11: signatures that used to be wrong or illegal -------------------

test("union members inside list get parens", () => {
  const ts = emit(
    M([
      FN({
        params: [
          {
            name: "xs",
            kind: "P",
            required: true,
            schema: {
              type: "list",
              items: { type: "union", opts: [{ type: "int" }, { type: "str" }] },
            },
          },
        ],
      }),
    ]),
    "m",
  );
  assert.match(ts, /xs: \(number \| string\)\[\]/);
  assert.doesNotMatch(ts, /number \| string\[\]/);
});

test("optional param before a required one becomes T | undefined", () => {
  // Python allows `def f(a=1, *, b)` — TS forbids `a?: T, b: U`.
  const ts = emit(
    M([
      FN({
        params: [
          { name: "a", kind: "P", required: false, schema: { type: "int" } },
          { name: "b", kind: "KEYWORD_ONLY", required: true, schema: { type: "str" } },
        ],
      }),
    ]),
    "m",
  );
  assert.match(ts, /a: number \| undefined, b: string/);
  assert.doesNotMatch(ts, /a\?: number, b: string/);
});

test("a python param named opts does not clobber CallOptions", () => {
  const ts = emit(
    M([
      FN({
        params: [{ name: "opts", kind: "P", required: true, schema: { type: "str" } }],
      }),
    ]),
    "m",
  );
  assert.match(ts, /opts: string, opts_\?: PylairCallOptions/);
  assert.match(ts, /, opts_\)/); // CallOptions passed through, not the string
});

test("python functions named call/stream can't shadow the imports", () => {
  const ts = emit(
    M([FN({ name: "call" }), FN({ name: "stream" }), FN({ name: "CallOptions" })]),
    "m",
  );
  assert.match(ts, /pylairCall<null>\("call"/);
  assert.match(ts, /pylairCall<null>\("stream"/);
  assert.doesNotMatch(ts, /[^a-zA-Z]call<null>\("call"/); // would be infinite recursion
});

test("names colliding after camelCase dedupe", () => {
  const ts = emit(M([FN({ name: "foo_bar" }), FN({ name: "fooBar" })]), "m");
  assert.match(ts, /export function fooBar\(/);
  assert.match(ts, /export function fooBar_\(/);
  assert.match(ts, /\("foo_bar"/);
  assert.match(ts, /\("fooBar"/);
});

test("param idents colliding after cleaning dedupe", () => {
  const ts = emit(
    M([
      FN({
        params: [
          { name: "foo_bar", kind: "P", required: true, schema: { type: "int" } },
          { name: "foo-bar", kind: "P", required: true, schema: { type: "str" } },
        ],
      }),
    ]),
    "m",
  );
  assert.match(ts, /foo_bar: number, foo_bar_: string/);
  assert.match(ts, /"foo-bar": foo_bar_/); // wire name keeps the real param name
});

test("*args/**kwargs are dropped — they can't cross the bridge", () => {
  const ts = emit(
    M([
      FN({
        params: [
          { name: "x", kind: "P", required: true, schema: { type: "int" } },
          { name: "args", kind: "VAR_POSITIONAL", required: true, schema: { type: "any" } },
          { name: "kwargs", kind: "VAR_KEYWORD", required: true, schema: { type: "any" } },
        ],
      }),
    ]),
    "m",
  );
  assert.match(ts, /export function f\(x: number, opts\?/);
  assert.doesNotMatch(ts, /args:|kwargs:/);
});

// The audit's requirement: generated output must actually compile. Emit a
// fixture exercising every tricky case and typecheck it with the real
// @pylair/client types.
test("generated fixture compiles under tsc --strict", () => {
  const dataclass = {
    type: "dataclass",
    name: "Point",
    fields: [
      { name: "x", required: true, schema: { type: "float" } },
      { name: "y", required: false, schema: { type: "float" } },
    ],
  };
  const tdict = {
    type: "typeddict",
    name: "SearchOpts",
    fields: [
      { name: "limit", required: true, schema: { type: "bigint" } },
      { name: "label", required: false, schema: { type: "str" } },
      { name: "point", required: false, schema: dataclass },
    ],
  };
  const ts = emit(
    M([
      FN({
        name: "mixed_params",
        params: [
          { name: "a", kind: "P", required: false, schema: { type: "int" } },
          { name: "b", kind: "KEYWORD_ONLY", required: true, schema: { type: "str" } },
          { name: "opts", kind: "KEYWORD_ONLY", required: false, schema: { type: "str" } },
          {
            name: "xs",
            kind: "P",
            required: true,
            schema: {
              type: "list",
              items: { type: "union", opts: [{ type: "int" }, { type: "str" }] },
            },
          },
          { name: "p", kind: "P", required: true, schema: dataclass },
        ],
        returns: { type: "union", opts: [dataclass, { type: "none" }] },
      }),
      FN({
        name: "search",
        params: [{ name: "opts", kind: "P", required: true, schema: tdict }],
        returns: { type: "union", opts: [tdict, { type: "none" }] },
      }),
      FN({ name: "call" }),
      FN({ name: "stream", kind: "generator", returns: { type: "bytes" } }),
      FN({ name: "foo_bar" }),
      FN({ name: "fooBar" }),
      // pydantic coverage: split aliases both ways, computed fields,
      // serializer-owned fields, nested + concretized-generic models,
      // enums, tuples, sets-as-lists and the shared stringly types
      FN({
        name: "pydantic_roundtrip",
        params: [
          {
            name: "order",
            kind: "P",
            required: true,
            schema: {
              type: "pydantic",
              name: "Order",
              in_fields: [
                { name: "orderId", required: true, schema: { type: "str" } },
                { name: "user_id", required: true, schema: { type: "int" } },
                { name: "color", required: true, schema: { type: "enum", values: ["red", "blue"] } },
                { name: "pair", required: true, schema: { type: "tuple", items: [{ type: "int" }, { type: "str" }] } },
                { name: "tags", required: true, schema: { type: "list", items: { type: "str" } } },
                { name: "ttl", required: false, schema: { type: "timedelta" } },
                { name: "ident", required: true, schema: { type: "uuid" } },
                { name: "price", required: true, schema: { type: "decimal" } },
                { name: "dest", required: true, schema: { type: "path" } },
                { name: "blob", required: true, schema: { type: "bytes" } },
                { name: "box", required: true, schema: { type: "pydantic", name: "IntBox", fields: [{ name: "item", required: true, schema: { type: "int" } }] } },
              ],
              out_fields: [
                { name: "orderId", required: true, schema: { type: "str" } },
                { name: "userId", required: true, schema: { type: "int" } },
                { name: "color", required: true, schema: { type: "enum", values: ["red", "blue"] } },
                { name: "pair", required: true, schema: { type: "tuple", items: [{ type: "int" }, { type: "str" }] } },
                { name: "tags", required: true, schema: { type: "list", items: { type: "str" } } },
                { name: "ttl", required: true, schema: { type: "timedelta" } },
                { name: "ident", required: true, schema: { type: "uuid" } },
                { name: "price", required: true, schema: { type: "decimal" } },
                { name: "dest", required: true, schema: { type: "path" } },
                { name: "blob", required: true, schema: { type: "str" }, serializer: true },
                { name: "box", required: true, schema: { type: "pydantic", name: "IntBox", fields: [{ name: "item", required: true, schema: { type: "int" } }] } },
                { name: "total", required: true, schema: { type: "int" }, computed: true },
              ],
            },
          },
        ],
        returns: {
          type: "pydantic",
          name: "Order",
          in_fields: [],
          out_fields: [],
        },
      }),
    ]),
    "fixture",
  );
  // fileURLToPath, not .pathname — Windows yields "/D:/..." otherwise.
  const root = fileURLToPath(new URL("../../../", import.meta.url));
  const dir = mkdtempSync(path.join(tmpdir(), "pylair-tsc-"));
  writeFileSync(path.join(dir, "fixture.ts"), ts);
  writeFileSync(
    path.join(dir, "tsconfig.json"),
    JSON.stringify({
      compilerOptions: {
        strict: true,
        noEmit: true,
        target: "es2022",
        module: "esnext",
        moduleResolution: "bundler",
        skipLibCheck: true,
        paths: { "@pylair/client": [`${root}packages/client/dist/index.d.ts`] },
      },
      files: ["fixture.ts"],
    }),
  );
  const tsc = `${root}packages/cli/node_modules/typescript/bin/tsc`;
  const r = execFileSync(process.execPath, [tsc, "-p", "tsconfig.json"], {
    cwd: dir,
    encoding: "utf-8",
    stdio: ["pipe", "pipe", "pipe"],
  });
  // execFileSync throws on non-zero exit; reaching here means it compiled.
  assert.equal(r.trim(), "");
});

// ---- golden: the emitted file is reviewable in-repo ------------------------

test("golden: emit(fixture) matches committed testapp stubs", () => {
  const manifest = JSON.parse(
    readFileSync(
      new URL("../golden/testapp.manifest.json", import.meta.url),
      "utf-8",
    ),
  );
  const golden = readFileSync(
    new URL("../golden/testapp.api.generated.ts", import.meta.url),
    "utf-8",
  );
  assert.equal(
    emit(manifest, "testapp"),
    golden.replace(/\r\n/g, "\n"), // tolerate autocrlf checkouts
    "emitter drift — regenerate golden: see golden/README",
  );
});

// ---- manifest loading (real python subprocess) ----------------------------

const hasPython = (() => {
  try {
    execFileSync(
      "python3",
      ["-c", "import sys;sys.exit(0 if sys.version_info>=(3,12) else 1)"],
      { stdio: "pipe" },
    );
    return true;
  } catch {
    return false;
  }
})();

test("loadManifest reads a manifest from a real module", { skip: !hasPython }, () => {
  const dir = mkdtempSync(path.join(tmpdir(), "tp-codegen-"));
  writeFileSync(
    path.join(dir, "appmod.py"),
    "from pylair import expose\n@expose\ndef hi(name: str) -> str:\n    return name\n",
  );
  const src = path.resolve("../../python/pylair/src");
  const m = loadManifest({
    python: "python3",
    module: "appmod",
    cwd: dir,
    env: { PYTHONPATH: src },
  });
  const fn = m.functions.find((f) => f.name === "hi");
  assert.equal(fn.params[0].schema.type, "str");
});

test("loadManifest surfaces module errors", { skip: !hasPython }, () => {
  assert.throws(
    () => loadManifest({ python: "python3", module: "no_such_xyz" }),
    /codegen manifest failed/,
  );
});

// ---- pydantic models (decision 0012) -----------------------------------------

const ORDER = {
  type: "pydantic",
  name: "Order",
  in_fields: [
    { name: "orderId", required: true, schema: { type: "str" } },
    { name: "user_id", required: true, schema: { type: "int" } },
    { name: "note", required: false, schema: { type: "str" } },
  ],
  out_fields: [
    { name: "orderId", required: true, schema: { type: "str" } },
    { name: "userId", required: true, schema: { type: "int" } },
    { name: "note", required: true, schema: { type: "str" } },
    { name: "total", required: true, schema: { type: "int" }, computed: true },
  ],
};

test("pydantic split shapes emit FooInput for params and Foo for results", () => {
  const ts = emit(
    M([
      FN({
        name: "place_order",
        params: [{ name: "order", kind: "P", required: true, schema: ORDER }],
        returns: ORDER,
      }),
    ]),
    "m",
  );
  assert.match(ts, /export interface OrderInput \{/);
  assert.match(ts, /user_id: number;/); // validation alias on input
  assert.match(ts, /note\?: string;/); // defaulted field optional on input
  assert.match(ts, /export interface Order \{/);
  assert.match(ts, /userId: number;/); // serialization alias on output
  assert.match(ts, /total: number; \/\/ computed field/);
  assert.match(ts, /placeOrder\(order: OrderInput/);
  assert.match(ts, /Promise<Order>/);
});

test("pydantic symmetric shapes emit one interface", () => {
  const sym = {
    type: "pydantic",
    name: "Sym",
    fields: [
      { name: "a", required: true, schema: { type: "int" } },
      { name: "b", required: false, schema: { type: "str" } },
    ],
  };
  const ts = emit(M([FN({ params: [{ name: "s", kind: "P", required: true, schema: sym }], returns: sym })]), "m");
  assert.match(ts, /export interface Sym \{/);
  assert.doesNotMatch(ts, /SymInput/);
});

test("serializer-owned fields typed from return annotation with comment", () => {
  const m = {
    type: "pydantic",
    name: "Hexed",
    in_fields: [{ name: "blob", required: true, schema: { type: "bytes" } }],
    out_fields: [{ name: "blob", required: true, schema: { type: "str" }, serializer: true }],
  };
  const ts = emit(M([FN({ returns: m })]), "m");
  assert.match(ts, /blob: string; \/\/ custom serializer owns this field's wire shape/);
  // input side keeps the validation type
  assert.match(ts, /blob: Uint8Array;/);
});

test("shared types map to their wire forms", () => {
  const ts = emit(
    M([
      FN({
        params: [
          { name: "td", kind: "P", required: true, schema: { type: "timedelta" } },
          { name: "u", kind: "P", required: true, schema: { type: "uuid" } },
          { name: "d", kind: "P", required: true, schema: { type: "decimal" } },
          { name: "p", kind: "P", required: true, schema: { type: "path" } },
        ],
      }),
    ]),
    "m",
  );
  assert.match(ts, /td: string, u: string, d: string, p: string/);
});

test("nested and generic-concretized models collect and emit", () => {
  const inner = {
    type: "pydantic",
    name: "IntBox",
    fields: [{ name: "item", required: true, schema: { type: "int" } }],
  };
  const outer = {
    type: "pydantic",
    name: "Outer",
    fields: [
      { name: "box", required: true, schema: inner },
      { name: "boxes", required: true, schema: { type: "list", items: inner } },
      { name: "maybe", required: false, schema: { type: "union", opts: [inner, { type: "none" }] } },
    ],
  };
  const ts = emit(M([FN({ returns: outer })]), "m");
  assert.match(ts, /export interface IntBox \{/);
  assert.match(ts, /box: IntBox;/);
  assert.match(ts, /boxes: IntBox\[\];/);
  assert.match(ts, /maybe\?: IntBox \| null;/);
});
