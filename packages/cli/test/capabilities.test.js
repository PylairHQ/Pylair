import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import {
  assertCapabilities,
  checkCapabilities,
  formatFinding,
  loadCapabilities,
  patternMatches,
} from "../dist/capabilities.js";

/** An app dir with the given capability files: { "default.json": {...} | "raw text" }. */
function app(files) {
  const dir = mkdtempSync(join(tmpdir(), "pylair-caps-"));
  const caps = join(dir, "src-tauri", "capabilities");
  mkdirSync(caps, { recursive: true });
  for (const [name, body] of Object.entries(files))
    writeFileSync(join(caps, name), typeof body === "string" ? body : JSON.stringify(body, null, 2));
  return dir;
}

function run(files, exposed) {
  const dir = app(files);
  try {
    const { capabilities, findings } = loadCapabilities(dir);
    return { capabilities, findings: [...findings, ...checkCapabilities(capabilities, exposed)] };
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
}

const cap = (identifier, windows, permissions) => ({ identifier, windows, permissions });
const N = (...names) => names.map((name) => ({ name }));
const errors = (r) => r.findings.filter((f) => f.level === "error");
const warns = (r) => r.findings.filter((f) => f.level === "warn");

describe("patternMatches (same rules as the plugin)", () => {
  test("exact, trailing star, literal star elsewhere", () => {
    assert.ok(patternMatches("add", "add"));
    assert.ok(!patternMatches("add", "add2"));
    assert.ok(patternMatches("x_*", "x_foo"));
    assert.ok(patternMatches("x_*", "x_"));
    assert.ok(!patternMatches("x_*", "x"));
    assert.ok(patternMatches("*", "anything"));
    assert.ok(!patternMatches("a*b", "axb"));
    assert.ok(patternMatches("a*b", "a*b"));
    assert.ok(!patternMatches("*x", "ax"));
    assert.ok(patternMatches("", ""));
    assert.ok(!patternMatches("", "a"));
  });
});

describe("loadCapabilities", () => {
  test("reads json and toml files, names the file and the capability", () => {
    const r = run({
      "a.json": cap("a-cap", ["main"], ["pylair:default"]),
      "b.toml": 'identifier = "b-cap"\nwindows = ["win-b"]\npermissions = ["pylair:default"]\n',
    });
    assert.deepEqual(
      r.capabilities.map((c) => [c.file, c.identifier, c.windows]).sort(),
      [
        ["src-tauri/capabilities/a.json", "a-cap", ["main"]],
        ["src-tauri/capabilities/b.toml", "b-cap", ["win-b"]],
      ],
    );
    assert.deepEqual(r.findings, []);
  });

  test("a file that does not parse is an error naming the file", () => {
    const r = run({ "bad.json": "{ not json" });
    assert.equal(errors(r).length, 1);
    assert.match(errors(r)[0].file, /bad\.json$/);
  });

  test("an app with no capabilities directory has nothing to check", () => {
    const dir = mkdtempSync(join(tmpdir(), "pylair-caps-"));
    try {
      assert.deepEqual(loadCapabilities(dir), { capabilities: [], findings: [] });
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  });
});

describe("checkCapabilities: clean configurations", () => {
  test("the shipped default capability has no findings", () => {
    const r = run({
      "default.json": cap("default", ["main"], ["core:default", "pylair:default", "pylair:allow-python-selftest-report"]),
    });
    assert.deepEqual(r.findings, []);
  });

  test("the recommended set-level deny has no findings", () => {
    const r = run({
      "default.json": cap("default", ["main"], [
        "core:default",
        { identifier: "pylair:default", deny: N("admin_*") },
      ]),
    });
    assert.deepEqual(r.findings, []);
  });

  test("an allow list on both call commands has no findings", () => {
    const r = run({
      "reports.json": cap("reports", ["reports"], [
        { identifier: "pylair:allow-python-call", allow: N("report_*") },
        { identifier: "pylair:allow-python-call-stream", allow: N("report_*") },
        "pylair:allow-python-state",
      ]),
    });
    assert.deepEqual(r.findings, []);
  });
});

describe("checkCapabilities: allow: [] is an error", () => {
  test("on pylair:default", () => {
    const r = run({
      "x.json": cap("x-cap", ["main"], [{ identifier: "pylair:default", allow: [] }]),
    });
    const e = errors(r);
    assert.equal(e.length, 1);
    assert.match(e[0].file, /x\.json$/);
    assert.equal(e[0].capability, "x-cap");
    assert.match(e[0].message, /pylair:default/);
    assert.match(e[0].message, /allow: \[\]/);
    assert.match(e[0].message, /everything/);
  });

  test("on an individual pylair permission", () => {
    const r = run({
      "x.json": cap("x-cap", ["main"], [
        { identifier: "pylair:allow-python-call", allow: [] },
        { identifier: "pylair:allow-python-call-stream", allow: [] },
      ]),
    });
    assert.equal(errors(r).filter((f) => /allow: \[\]/.test(f.message)).length, 2);
  });

  test("a non-pylair permission with an empty allow is not this check's business", () => {
    const r = run({
      "x.json": cap("x-cap", ["main"], ["pylair:default", { identifier: "fs:allow-read-file", allow: [] }]),
    });
    assert.deepEqual(r.findings, []);
  });

  test("an empty deny list is harmless", () => {
    const r = run({
      "x.json": cap("x-cap", ["main"], [{ identifier: "pylair:default", deny: [] }]),
    });
    assert.deepEqual(r.findings, []);
  });
});

describe("checkCapabilities: both call commands must share a scope", () => {
  test("scope on python-call only is an error naming window and command", () => {
    const r = run({
      "x.json": cap("x-cap", ["main"], [
        "pylair:default",
        { identifier: "pylair:allow-python-call", deny: N("spy") },
      ]),
    });
    const e = errors(r);
    assert.equal(e.length, 1);
    assert.equal(e[0].capability, "x-cap");
    assert.match(e[0].message, /main/);
    assert.match(e[0].message, /python-call\b/);
    assert.match(e[0].message, /python-call-stream/);
  });

  test("scope on python-call-stream only is an error (the reverse)", () => {
    const r = run({
      "x.json": cap("x-cap", ["main"], [
        "pylair:default",
        { identifier: "pylair:allow-python-call-stream", deny: N("spy") },
      ]),
    });
    assert.equal(errors(r).length, 1);
  });

  test("scope on a command the window is not granted is fine", () => {
    const r = run({
      "x.json": cap("x-cap", ["main"], [{ identifier: "pylair:allow-python-call", allow: N("add") }]),
    });
    assert.deepEqual(r.findings, []);
  });

  test("two capabilities on one window can cover one command each", () => {
    const r = run({
      "a.json": cap("a", ["main"], ["pylair:default", { identifier: "pylair:allow-python-call", deny: N("spy") }]),
      "b.json": cap("b", ["main"], [{ identifier: "pylair:allow-python-call-stream", deny: N("spy") }]),
    });
    assert.deepEqual(errors(r), []);
  });

  test("windows are judged separately", () => {
    const r = run({
      "a.json": cap("a", ["one"], ["pylair:default", { identifier: "pylair:allow-python-call", deny: N("spy") }]),
      "b.json": cap("b", ["two"], ["pylair:default"]),
    });
    const e = errors(r);
    assert.equal(e.length, 1);
    assert.match(e[0].message, /one/);
    assert.doesNotMatch(e[0].message, /two/);
  });

  test("a wildcard window pattern is checked against its own text", () => {
    const r = run({
      "a.json": cap("a", ["win-*"], ["pylair:default", { identifier: "pylair:allow-python-call", deny: N("spy") }]),
    });
    assert.equal(errors(r).length, 1);
  });

  test("different scopes on the two commands are a warning", () => {
    const r = run({
      "x.json": cap("x-cap", ["main"], [
        "pylair:default",
        { identifier: "pylair:allow-python-call", deny: N("spy") },
        { identifier: "pylair:allow-python-call-stream", deny: N("other") },
      ]),
    });
    assert.deepEqual(errors(r), []);
    assert.equal(warns(r).length, 1);
    assert.match(warns(r)[0].message, /different/);
  });
});

describe("checkCapabilities: malformed entries", () => {
  test("a scope entry with an unknown key is an error (the plugin would refuse every call)", () => {
    const r = run({
      "x.json": cap("x-cap", ["main"], [{ identifier: "pylair:default", deny: [{ nmae: "add" }] }]),
    });
    const e = errors(r);
    assert.equal(e.length, 1);
    assert.match(e[0].message, /nmae/);
    assert.equal(e[0].capability, "x-cap");
  });

  test("a scope entry without a string name is an error", () => {
    const r = run({
      "x.json": cap("x-cap", ["main"], [{ identifier: "pylair:default", deny: [{ name: 3 }, "add"] }]),
    });
    assert.equal(errors(r).length, 2);
  });
});

describe("checkCapabilities: patterns that match nothing", () => {
  const files = {
    "x.json": cap("x-cap", ["main"], [
      { identifier: "pylair:default", allow: N("add", "typo_*"), deny: N("gone") },
    ]),
  };

  test("warns for each pattern that matches no exposed function, naming it", () => {
    const r = run(files, ["add", "counter"]);
    const w = warns(r);
    assert.equal(w.length, 2);
    assert.ok(w.some((f) => /typo_\*/.test(f.message)));
    assert.ok(w.some((f) => /gone/.test(f.message)));
    assert.ok(!w.some((f) => /"add"/.test(f.message)));
    assert.equal(w[0].capability, "x-cap");
    assert.match(w[0].file, /x\.json$/);
  });

  test("a star matches everything and an empty exposes list matches nothing", () => {
    const star = { "x.json": cap("x", ["main"], [{ identifier: "pylair:default", deny: N("*") }]) };
    assert.deepEqual(warns(run(star, ["add"])), []);
    assert.equal(warns(run(star, [])).length, 1);
  });

  test("without exposed names there is no pattern warning", () => {
    assert.deepEqual(warns(run(files)), []);
  });
});

describe("formatFinding / assertCapabilities", () => {
  test("a finding renders level, file, capability and message", () => {
    const line = formatFinding({
      level: "error",
      file: "src-tauri/capabilities/x.json",
      capability: "x-cap",
      message: "boom",
    });
    assert.match(line, /^error: /);
    assert.match(line, /src-tauri\/capabilities\/x\.json/);
    assert.match(line, /x-cap/);
    assert.match(line, /boom/);
  });

  test("assertCapabilities throws listing every error, and returns warnings otherwise", () => {
    const bad = app({
      "x.json": cap("x-cap", ["main"], [
        { identifier: "pylair:default", allow: [] },
        { identifier: "pylair:allow-python-call", deny: N("a") },
      ]),
    });
    try {
      assert.throws(() => assertCapabilities(bad), (e) => {
        assert.match(e.message, /allow: \[\]/);
        assert.match(e.message, /python-call-stream/);
        assert.match(e.message, /x\.json/);
        return true;
      });
    } finally {
      rmSync(bad, { recursive: true, force: true });
    }
    const good = app({ "default.json": cap("default", ["main"], ["pylair:default"]) });
    try {
      assert.deepEqual(assertCapabilities(good), []);
    } finally {
      rmSync(good, { recursive: true, force: true });
    }
  });
});
