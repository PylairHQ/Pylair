// Contract test: generated interfaces vs the real runtime. For a module
// exercising every pydantic shape, run codegen on the REAL schema, call
// through the real spawned runtime, and assert the result object matches
// the generated OUTPUT interface exactly (required keys present, no
// undeclared keys, value JSON types consistent with declared TS types),
// and that params built to the INPUT interface are accepted.
import assert from "node:assert/strict";
import { spawn, spawnSync, execFileSync } from "node:child_process";
import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { test } from "node:test";
import { emit } from "../dist/codegen.js";
import { loadManifest } from "../dist/manifest.js";

const src = path.resolve("../../python/pylair/src");

const hasPydantic = (() => {
  try {
    execFileSync("python3", ["-c", "import pydantic, sys; sys.exit(0 if sys.version_info >= (3, 12) else 1)"], { stdio: "pipe" });
    return true;
  } catch {
    return false;
  }
})();

const APP = `
import datetime, enum, uuid
from decimal import Decimal
from pathlib import Path
from typing import Generic, TypeVar
from pydantic import BaseModel, Field, computed_field, field_serializer
from pylair import expose

class Color(enum.Enum):
    RED = "red"
    BLUE = "blue"

T = TypeVar("T")

class IntBox(BaseModel, Generic[T]):
    item: T

class Sym(BaseModel):
    a: int
    b: str = "x"

class Order(BaseModel):
    order_id: str = Field(alias="orderId")
    user_ident: int = Field(validation_alias="user_id", serialization_alias="userId")
    color: Color
    pair: tuple[int, str]
    tags: set[str]
    ttl: datetime.timedelta
    ident: uuid.UUID
    price: Decimal
    dest: Path
    blob: bytes
    box: IntBox[int]
    note: str = "default-note"

    @field_serializer("blob")
    def ser_blob(self, v) -> str:
        return v.hex()

    @computed_field
    @property
    def total(self) -> int:
        return self.user_ident * 2

@expose
def place_order(order: Order) -> Order:
    return order

@expose
def echo_sym(s: Sym) -> Sym:
    return s
`;

// input params built to the INPUT interface (wire-encoded: bytes as $b64)
const INPUT_ORDER = {
  orderId: "o-1",
  user_id: 21,
  color: "red",
  pair: [1, "a"],
  tags: ["x", "y"],
  ttl: "PT1H30M",
  ident: "12345678-1234-5678-1234-567812345678",
  price: "3.14",
  dest: "/tmp/x",
  blob: { $b64: Buffer.from([0xff, 0x00]).toString("base64") },
  box: { item: 5 },
};

/** parse `export interface Name { key?: type; }` blocks out of generated TS */
function parseInterfaces(ts) {
  const out = {};
  for (const m of ts.matchAll(/export interface (\w+) \{\n((?:.+\n)+?)\}/g)) {
    const fields = {};
    for (const line of m[2].trim().split("\n")) {
      const fm = line.trim().match(/^("?[\w$]+"?)(\?)?:\s*([^;/]+)/);
      if (fm) fields[fm[1].replace(/"/g, "")] = { optional: !!fm[2], type: fm[3].trim() };
    }
    out[m[1]] = fields;
  }
  return out;
}

function valueMatchesType(value, tsType, interfaces) {
  if (tsType === "string") return typeof value === "string";
  if (tsType === "number") return typeof value === "number";
  if (tsType === "boolean") return typeof value === "boolean";
  if (tsType === "null") return value === null;
  if (tsType === "unknown") return true;
  if (tsType === "Uint8Array") return value instanceof Uint8Array || (value && typeof value === "object" && "$b64" in value);
  if (tsType.endsWith("[]")) {
    if (!Array.isArray(value)) return false;
    const inner = tsType.slice(0, -2).replace(/^\(|\)$/g, "");
    return value.every((v) => valueMatchesType(v, inner, interfaces));
  }
  if (/^\[.+\]$/.test(tsType)) {
    // fixed tuple [A, B]
    const parts = tsType.slice(1, -1).split(/,\s*/);
    return Array.isArray(value) && value.length === parts.length && parts.every((p, i) => valueMatchesType(value[i], p, interfaces));
  }
  if (tsType.includes(" | ")) return tsType.split(" | ").some((t) => valueMatchesType(value, t, interfaces));
  if (/^".*"$/.test(tsType)) return value === JSON.parse(tsType);
  if (interfaces[tsType]) {
    if (!value || typeof value !== "object") return false;
    // recurse: nested interface keys and types are part of the contract
    assertMatchesInterface(value, interfaces[tsType], interfaces, tsType);
    return true;
  }
  return true; // Record<>/unmodelled — presence already checked
}

/** assert obj matches the interface exactly: keys, presence, types */
function assertMatchesInterface(obj, iface, interfaces, label) {
  const declared = Object.keys(iface);
  for (const key of declared) {
    if (!iface[key].optional) assert.ok(key in obj, `${label}: required key ${key} missing`);
  }
  for (const key of Object.keys(obj)) {
    assert.ok(key in iface, `${label}: undeclared key ${key} in result`);
    const v = obj[key];
    assert.ok(
      valueMatchesType(v, iface[key].type, interfaces),
      `${label}: ${key} value ${JSON.stringify(v)} doesn't match ${iface[key].type}`,
    );
  }
}

/** minimal length-prefixed-frame client for the real runtime */
function spawnRuntime(module, cwd, env) {
  const proc = spawn("python3", ["-m", "pylair", module], {
    cwd,
    env: { ...process.env, PYTHONPATH: `${src}${path.delimiter}${cwd}`, ...env },
    stdio: ["pipe", "pipe", "pipe"],
  });
  let buf = Buffer.alloc(0);
  const waiting = [];
  proc.stdout.on("data", (chunk) => {
    buf = Buffer.concat([buf, chunk]);
    while (buf.length >= 4) {
      const n = buf.readUInt32BE(0);
      if (buf.length < 4 + n) break;
      const msg = JSON.parse(buf.subarray(4, 4 + n).toString());
      buf = buf.subarray(4 + n);
      const w = waiting.shift();
      if (w) w(msg);
    }
  });
  return {
    proc,
    next: () => new Promise((res) => waiting.push(res)),
    send(msg) {
      const body = Buffer.from(JSON.stringify(msg));
      const hdr = Buffer.alloc(4);
      hdr.writeUInt32BE(body.length);
      proc.stdin.write(Buffer.concat([hdr, body]));
    },
    kill() {
      proc.kill();
    },
  };
}

test("contract: generated interfaces match real runtime behavior", { skip: !hasPydantic, timeout: 60000 }, async () => {
  const dir = mkdtempSync(path.join(tmpdir(), "pylair-contract-"));
  writeFileSync(path.join(dir, "contractapp.py"), APP);

  // 1. the REAL schema for the real module
  const manifest = loadManifest({ python: "python3", module: "contractapp", cwd: dir, env: { PYTHONPATH: `${src}${path.delimiter}${dir}` } });
  assert.ok(!manifest.error, `manifest error: ${manifest.error}`);

  // 2. generated TS from it — interfaces are the contract under test
  const ts = emit(manifest, "contractapp");
  const interfaces = parseInterfaces(ts);
  assert.ok(interfaces.OrderInput, "expected split OrderInput interface");
  assert.ok(interfaces.Order, "expected Order output interface");
  assert.ok(interfaces.Sym, "expected symmetric Sym interface");
  const intBoxName = Object.keys(interfaces).find((n) => n.startsWith("IntBox"));
  assert.ok(intBoxName, "expected nested generic IntBox interface, got: " + Object.keys(interfaces));

  // 3. real runtime round-trips
  const rt = spawnRuntime("contractapp", dir, {});
  try {
    const hello = await rt.next();
    assert.ok(hello.params.exposes.some((e) => e.name === "place_order"));

    // split-alias model: params per the INPUT interface accepted,
    // result matches the OUTPUT interface exactly
    rt.send({ id: 1, method: "call", params: { fn: "place_order", task: 1, input: { order: INPUT_ORDER } } });
    const rep = await rt.next();
    assert.ok(!rep.error, `call failed: ${JSON.stringify(rep.error)}`);
    assertMatchesInterface(rep.result, interfaces.Order, interfaces, "Order result");
    // spot checks: the contract-critical transformations
    assert.ok("userId" in rep.result, "serialization alias present on output");
    assert.ok(!("user_id" in rep.result), "validation alias absent on output");
    assert.ok("orderId" in rep.result, "symmetric alias present");
    assert.equal(rep.result.total, 42, "computed field present and correct");
    assert.equal(rep.result.blob, "ff00", "serializer-owned blob is the hex string");
    assert.equal(typeof rep.result.ttl, "string", "timedelta is a string");
    assert.equal(typeof rep.result.ident, "string", "uuid is a string");
    assert.equal(typeof rep.result.price, "string", "decimal is a string");
    assert.equal(typeof rep.result.dest, "string", "path is a string");
    assert.equal(rep.result.color, "red", "enum by value");
    assert.deepEqual(rep.result.pair, [1, "a"], "tuple round-trips");
    assert.equal(rep.result.note, "default-note", "default applied");

    // negative check: the INPUT interface's key name is the one the
    // runtime requires — the python field name must be rejected
    const badOrder = { ...INPUT_ORDER };
    delete badOrder.user_id;
    badOrder.user_ident = 21;
    rt.send({ id: 3, method: "call", params: { fn: "place_order", task: 3, input: { order: badOrder } } });
    const rep3 = await rt.next();
    assert.ok(rep3.error, "python field name user_ident must be rejected — input interface uses user_id");

    // symmetric model: input keys per the single interface accepted, output matches it
    rt.send({ id: 2, method: "call", params: { fn: "echo_sym", task: 2, input: { s: { a: 3 } } } });
    const rep2 = await rt.next();
    assert.ok(!rep2.error, `call failed: ${JSON.stringify(rep2.error)}`);
    assertMatchesInterface(rep2.result, interfaces.Sym, interfaces, "Sym result");
    assert.equal(rep2.result.b, "x", "default applied");
  } finally {
    rt.kill();
  }
});
