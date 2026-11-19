import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, readdirSync, readFileSync, statSync, utimesSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, relative } from "node:path";
import { fileURLToPath } from "node:url";
import ts from "typescript";
import { findArtifacts } from "../dist/build/artifacts.js";
import { runTool } from "../dist/build/util.js";
import { detectCaps, formatDuration, formatSize, stripAnsi, symbolsFor, truncate, Ui } from "../dist/ui/index.js";
import { stripPlain, parseBuildArgs } from "../dist/cli.js";

const cli = fileURLToPath(new URL("../dist/bin.js", import.meta.url));
const srcDir = fileURLToPath(new URL("../src/", import.meta.url));

/** Capabilities for a described terminal. */
const caps = (over = {}) =>
  detectCaps({
    env: {},
    platform: "linux",
    stdoutIsTTY: true,
    argv: [],
    columns: 100,
    ...over,
  });

describe("capability detection: fancy or plain", () => {
  test("an interactive terminal is fancy and animates", () => {
    const c = caps({ env: { TERM: "xterm-256color", LANG: "en_US.UTF-8" } });
    assert.equal(c.fancy, true);
    assert.equal(c.color, true);
    assert.equal(c.animate, true);
  });

  test("a pipe or a file is plain", () => {
    const c = caps({ stdoutIsTTY: false, env: { LANG: "en_US.UTF-8" } });
    assert.deepEqual([c.fancy, c.color, c.animate, c.unicode], [false, false, false, false]);
  });

  test("CI is plain, even on a TTY", () => {
    assert.equal(caps({ env: { CI: "true" } }).fancy, false);
    assert.equal(caps({ env: { CI: "1" } }).fancy, false);
  });

  test("NO_COLOR is plain", () => {
    assert.equal(caps({ env: { NO_COLOR: "1" } }).fancy, false);
  });

  test("TERM=dumb is plain", () => {
    assert.equal(caps({ env: { TERM: "dumb" } }).fancy, false);
  });

  test("--plain is plain, and wins over FORCE_COLOR", () => {
    assert.equal(caps({ argv: ["--plain"] }).fancy, false);
    assert.equal(caps({ argv: ["--plain"], env: { FORCE_COLOR: "1" } }).fancy, false);
  });

  test("FORCE_COLOR turns colour on without a TTY, in CI, and with NO_COLOR", () => {
    const piped = caps({ stdoutIsTTY: false, env: { FORCE_COLOR: "1", LANG: "C.UTF-8" } });
    assert.equal(piped.fancy, true);
    assert.equal(piped.color, true);
    assert.equal(piped.animate, false, "no live redraw without a TTY");
    assert.equal(caps({ env: { FORCE_COLOR: "1", CI: "true" } }).fancy, true);
    assert.equal(caps({ env: { FORCE_COLOR: "2", NO_COLOR: "1" } }).fancy, true);
  });

  test("FORCE_COLOR=0 or false turns colour off even on a TTY; empty means unset", () => {
    assert.equal(caps({ env: { FORCE_COLOR: "0" } }).fancy, false);
    assert.equal(caps({ env: { FORCE_COLOR: "false" } }).fancy, false);
    assert.equal(caps({ env: { FORCE_COLOR: "" } }).fancy, true);
  });

  test("TERM=dumb with FORCE_COLOR is coloured but never animated", () => {
    const c = caps({ env: { TERM: "dumb", FORCE_COLOR: "1" } });
    assert.equal(c.fancy, true);
    assert.equal(c.animate, false);
  });

  test("the width defaults to 80 when unknown", () => {
    assert.equal(caps({ columns: undefined }).columns, 80);
    assert.equal(caps({ columns: 0 }).columns, 80);
    assert.equal(caps({ columns: 132 }).columns, 132);
  });
});

describe("capability detection: Unicode or ASCII", () => {
  const win = (env, codePage) =>
    caps({ platform: "win32", env, codePage: codePage === undefined ? undefined : () => codePage });

  test("Windows Terminal (WT_SESSION) is Unicode whatever the code page", () => {
    assert.equal(win({ WT_SESSION: "abc" }, 437).unicode, true);
    assert.equal(win({ WT_SESSION: "abc" }, 850).unicode, true);
    assert.equal(win({ WT_SESSION: "abc" }).unicode, true);
  });

  test("the classic console (PowerShell 5.1, cmd) is ASCII on an OEM code page", () => {
    assert.equal(win({}, 437).unicode, false);
    assert.equal(win({}, 850).unicode, false);
    assert.equal(win({}, 1252).unicode, false);
  });

  test("the classic console is Unicode only once switched to UTF-8 (65001)", () => {
    assert.equal(win({}, 65001).unicode, true);
  });

  test("an unreadable code page is ASCII: the safe answer", () => {
    assert.equal(win({}).unicode, false);
    assert.equal(win({}, undefined).unicode, false);
  });

  test("the code page is only asked when it can matter", () => {
    let asked = 0;
    const probe = () => (asked++, 437);
    caps({ platform: "win32", env: { WT_SESSION: "x" }, codePage: probe }); // short-circuit
    caps({ platform: "win32", stdoutIsTTY: false, codePage: probe }); // plain
    caps({ platform: "linux", codePage: probe }); // not Windows
    assert.equal(asked, 0);
  });

  test("Windows plain output is ASCII even in Windows Terminal", () => {
    const c = win({ WT_SESSION: "x", CI: "1" }, 65001);
    assert.equal(c.fancy, false);
    assert.equal(c.unicode, false);
  });

  test("POSIX: UTF-8 locale is Unicode, the C locale and the Linux console are not", () => {
    assert.equal(caps({ env: { LANG: "en_GB.UTF-8" } }).unicode, true);
    assert.equal(caps({ env: { LC_ALL: "C.utf8" } }).unicode, true);
    assert.equal(caps({ env: { LANG: "C" } }).unicode, false);
    assert.equal(caps({ env: {} }).unicode, false);
    assert.equal(caps({ env: { TERM: "linux", LANG: "en_GB.UTF-8" } }).unicode, false);
    assert.equal(caps({ platform: "darwin", env: {} }).unicode, true);
  });
});

describe("symbols", () => {
  test("ASCII set is the documented one", () => {
    const s = symbolsFor({ unicode: false });
    assert.deepEqual([s.ok, s.fail, s.warn, s.skip, s.bullet], ["[ok]", "[x]", "[!]", "-", "-"]);
    for (const v of [...Object.values(s).flat()]) assert.match(String(v), /^[\x20-\x7e]+$/, `non-ASCII: ${v}`);
  });

  test("the Unicode set has a distinct glyph per meaning", () => {
    const s = symbolsFor({ unicode: true });
    assert.equal(new Set([s.ok, s.fail, s.warn, s.skip, s.bullet]).size, 5);
  });
});

/** A Ui writing into arrays, with a clock the test drives. */
function harness(c) {
  const out = [];
  const err = [];
  let now = 0;
  const timers = [];
  const clock = {
    now: () => now,
    setInterval: (fn) => (timers.push(fn), timers.length),
    clearInterval: () => {},
  };
  const ui = new Ui(c, { out: (s) => out.push(s), err: (s) => err.push(s) }, clock);
  return { ui, out, err, advance: (ms) => (now += ms), tick: () => timers.forEach((f) => f()), text: () => out.join(""), errText: () => err.join("") };
}

const plainCaps = () => caps({ stdoutIsTTY: false });
const ESC = /\u001b/;
const BRAILLE = /[⠀-⣿]/;

describe("plain output is stable text", () => {
  test("a whole build-shaped run has no ANSI, no spinner frames, no non-ASCII", async () => {
    const h = harness(plainCaps());
    h.ui.header("pylair build", [["app", "demo"]], ["building demo 1.0.0 for linux/x64 (x86_64-unknown-linux-gnu)"]);
    h.ui.setStepWidth(12);
    await h.ui.step("bundle python", async (s) => {
      s.detail("cached");
      h.advance(2100);
    });
    await assert.rejects(
      h.ui.step("smoke test", () => {
        h.advance(300);
        throw new Error("import failed");
      }),
      /import failed/,
    );
    h.ui.ok("done");
    h.ui.warn("warning: something odd");
    h.ui.error("pylair build: import failed");
    h.ui.check(true, "uv", 4, "uv 0.5");
    h.ui.check(false, "venv", 4, "missing", { file: "pyproject.toml", fix: "run `uv sync`" });
    h.ui.summary("fail", "1 problem(s) found");
    for (const s of [h.text(), h.errText()]) {
      assert.ok(!ESC.test(s), `ANSI in: ${JSON.stringify(s)}`);
      assert.ok(!BRAILLE.test(s), "spinner frame");
      assert.match(s, /^[\x09\x0a\x0d\x20-\x7e]*$/, `non-ASCII in: ${JSON.stringify(s)}`);
      assert.ok(!s.includes("\r"), "no carriage-return redraws");
    }
  });

  test("steps print a start line and a timed result line", async () => {
    const h = harness(plainCaps());
    await h.ui.step("prune", () => h.advance(1500));
    assert.equal(h.text(), "-> prune\n[ok] prune (1.5s)\n");
  });

  test("a failed step says so, and rethrows", async () => {
    const h = harness(plainCaps());
    await assert.rejects(h.ui.step("compile", () => { throw new Error("boom"); }), /boom/);
    assert.match(h.text(), /\[x\] compile \(0\.0s\)/);
  });

  test("the doctor line keeps its legacy shape exactly", () => {
    const h = harness(plainCaps());
    h.ui.check(true, "uv", 10, "uv 0.5.1");
    h.ui.check(false, "venv", 10, "no venv");
    assert.equal(h.text(), " ok  uv          uv 0.5.1\nFAIL venv        no venv\n");
  });

  test("a failed check lists its file and fix underneath", () => {
    const h = harness(plainCaps());
    h.ui.check(false, "venv", 4, "no venv", { file: ".venv", fix: "run `uv sync`" });
    assert.equal(h.text(), "FAIL venv  no venv\n     > file  .venv\n     > fix   run `uv sync`\n");
  });

  test("a passing check shows no hint", () => {
    const h = harness(plainCaps());
    h.ui.check(true, "uv", 4, "ok", { fix: "never shown" });
    assert.ok(!h.text().includes("never shown"));
  });

  test("warnings and errors keep today's wording and channel", () => {
    const h = harness(plainCaps());
    h.ui.warn("warning: no uv.lock");
    h.ui.warn("no prefix here", { bare: true });
    h.ui.warn("w", { plain: "pylair: warning: w" });
    h.ui.error("pylair build: nope");
    assert.equal(h.errText(), "warning: no uv.lock\nno prefix here\npylair: warning: w\npylair build: nope\n");
    assert.equal(h.text(), "");
  });

  test("a header prints only the legacy lines", () => {
    const h = harness(plainCaps());
    h.ui.header("pylair build", [["app", "x"]], ["building x"]);
    assert.equal(h.text(), "building x\n");
    const d = harness(plainCaps());
    d.ui.header("pylair doctor", [["app", "x"]], []);
    assert.equal(d.text(), "");
  });

  test("status is a line of its own in plain, a step detail when fancy", async () => {
    const p = harness(plainCaps());
    await p.ui.step("bundle python", () => p.ui.status("using cached x.tar.gz"));
    assert.match(p.text(), /^-> bundle python\nusing cached x\.tar\.gz\n\[ok\] bundle python/);
    const f = harness(caps({ stdoutIsTTY: false, env: { FORCE_COLOR: "1" } }));
    await f.ui.step("bundle python", () => f.ui.status("using cached x.tar.gz"));
    assert.match(stripAnsi(f.text()), /bundle python +0\.0s +using cached x\.tar\.gz/);
  });
});

describe("fancy output", () => {
  test("ASCII fancy (classic Windows console) uses [ok] / [x] and stays ASCII", async () => {
    const h = harness(caps({ platform: "win32", env: {}, codePage: () => 437 }));
    await h.ui.step("prune", () => h.advance(2100));
    await assert.rejects(h.ui.step("smoke test", () => { throw new Error("x"); }));
    h.ui.check(false, "venv", 4, "gone", { file: ".venv", fix: "run `uv sync`" });
    h.ui.warn("careful");
    const all = stripAnsi(h.text() + h.errText());
    assert.match(all, /\[ok\] prune +2\.1s/);
    assert.match(all, /\[x\] +smoke test/);
    assert.match(all, /\[!\] careful/);
    assert.match(all, /> fix +run `uv sync`/);
    assert.match(all, /^[\x09\x0a\x0d\x20-\x7e]*$/, `non-ASCII: ${JSON.stringify(all)}`);
  });

  test("colour is meaning only: green ok, red fail, yellow warn", async () => {
    const h = harness(caps({ env: { LANG: "en_GB.UTF-8" } }));
    await h.ui.step("prune", () => {});
    assert.match(h.text(), /\u001b\[32m✔\u001b\[39m/);
    await assert.rejects(h.ui.step("x", () => { throw new Error("e"); }));
    assert.match(h.text(), /\u001b\[31m✖\u001b\[39m/);
    h.ui.warn("careful");
    assert.match(h.errText(), /\u001b\[33m▲/);
  });

  test("a live step redraws one line, then replaces it with the result", async () => {
    const h = harness(caps({ env: { LANG: "en_GB.UTF-8" } }));
    const done = h.ui.step("install deps", async (s) => {
      s.progress("Resolved 12 packages");
      h.tick();
      h.advance(1200);
    });
    await done;
    const t = h.text();
    assert.match(t, /\u001b\[\?25l/, "cursor hidden while the spinner runs");
    assert.match(t, /\u001b\[\?25h/, "cursor restored after");
    assert.ok(BRAILLE.test(t), "spinner frame drawn");
    assert.match(stripAnsi(t), /Resolved 12 packages/);
    assert.match(stripAnsi(t), /✔ install deps +1\.2s/);
  });

  test("a message printed mid-step does not corrupt the spinner line", async () => {
    const h = harness(caps({ env: { LANG: "en_GB.UTF-8" } }));
    await h.ui.step("stage app", () => h.ui.warn("skipped a secret"));
    const t = h.text() + h.errText();
    assert.ok(t.indexOf("\r\u001b[2K") < t.indexOf("skipped a secret"), "line cleared before the warning");
  });

  test("the step detail is cut to the terminal width", async () => {
    const h = harness(caps({ columns: 50, env: { LANG: "en_GB.UTF-8" } }));
    await h.ui.step("bundle python", () => h.ui.status("x".repeat(200)));
    // The live line is redrawn with \r; only the last redraw is on screen.
    const shown = stripAnsi(h.text()).split(/[\r\n]/).filter((l) => l.includes("bundle python") && l.includes("x"));
    assert.equal(shown.length, 1);
    assert.ok(shown[0].length <= 50, `line is ${shown[0].length} wide`);
    assert.ok(shown[0].endsWith("…"), "cut with an ellipsis");
  });
});

describe("formatting helpers", () => {
  test("durations", () => {
    assert.equal(formatDuration(0), "0.0s");
    assert.equal(formatDuration(450), "0.5s");
    assert.equal(formatDuration(2100), "2.1s");
    assert.equal(formatDuration(59_900), "59.9s");
    assert.equal(formatDuration(125_000), "2m 05s");
  });

  test("sizes", () => {
    assert.equal(formatSize(0), "0 B");
    assert.equal(formatSize(1023), "1023 B");
    assert.equal(formatSize(1536), "1.5 KiB");
    assert.equal(formatSize(5 * 2 ** 20), "5.0 MiB");
    assert.equal(formatSize(123 * 2 ** 20), "123 MiB");
    assert.equal(formatSize(3 * 2 ** 30), "3.0 GiB");
  });

  test("stripAnsi and truncate", () => {
    assert.equal(stripAnsi("\u001b[32mok\u001b[39m \u001b[1mgo\u001b[22m"), "ok go");
    assert.equal(truncate("abcdefghij", 6, "..."), "abc...");
    assert.equal(truncate("abc", 6, "..."), "abc");
    assert.equal(truncate("abcdefghij", 6, "…"), "abcde…");
  });

  test("aligned rows", () => {
    const h = harness(plainCaps());
    h.ui.rows([["app", "demo"], ["platform", "linux/x64"]]);
    assert.equal(h.text(), "  app       demo\n  platform  linux/x64\n");
  });
});

describe("--plain on the command line", () => {
  test("is dropped before the command is parsed, and only before --cmd or --", () => {
    assert.deepEqual(stripPlain(["--plain", "doctor"]), { argv: ["doctor"], plain: true });
    assert.deepEqual(stripPlain(["doctor", "--plain", "--dir", "x"]), { argv: ["doctor", "--dir", "x"], plain: true });
    assert.deepEqual(stripPlain(["doctor"]), { argv: ["doctor"], plain: false });
    assert.deepEqual(stripPlain(["wrap", "--cmd", "srv", "--plain"]), { argv: ["wrap", "--cmd", "srv", "--plain"], plain: false });
    assert.deepEqual(stripPlain(["build", "--", "--plain"]), { argv: ["build", "--", "--plain"], plain: false });
  });

  test("build: --verbose is ours and still forwarded to tauri, --plain is never forwarded", () => {
    const p = parseBuildArgs(["--verbose", "--plain", "--stage-only"]);
    assert.equal(p.verbose, true);
    assert.equal(p.stageOnly, true);
    assert.deepEqual(p.tauriArgs, ["--verbose"]);
    assert.equal(parseBuildArgs([]).verbose, false);
  });

  test("the real CLI: piped output has no ANSI and no non-ASCII, with or without --plain", () => {
    const app = mkdtempSync(join(tmpdir(), "pylair-ui-"));
    for (const args of [["doctor", "--dir", app], ["--plain", "doctor", "--dir", app], ["--help"], ["nope"]]) {
      const r = spawnSync(process.execPath, [cli, ...args], { encoding: "utf8", env: { ...process.env, FORCE_COLOR: "", NO_COLOR: "", CI: "" } });
      for (const s of [r.stdout, r.stderr]) {
        assert.ok(!ESC.test(s), `${args.join(" ")}: ANSI in ${JSON.stringify(s)}`);
        assert.match(s, /^[\x09\x0a\x0d\x20-\x7e]*$/, `${args.join(" ")}: non-ASCII in ${JSON.stringify(s)}`);
      }
    }
  });

  test("the real CLI: FORCE_COLOR=1 gives colour even when piped, --plain still wins", () => {
    const app = mkdtempSync(join(tmpdir(), "pylair-ui-"));
    const run = (args) => spawnSync(process.execPath, [cli, ...args], { encoding: "utf8", env: { ...process.env, FORCE_COLOR: "1", CI: "", NO_COLOR: "" } });
    assert.ok(ESC.test(run(["doctor", "--dir", app]).stdout), "colour forced on");
    assert.ok(!ESC.test(run(["--plain", "doctor", "--dir", app]).stdout), "--plain wins");
  });

  test("doctor exits 1 and lists the file and the fix for a failure (plain)", () => {
    const app = mkdtempSync(join(tmpdir(), "pylair-ui-"));
    const r = spawnSync(process.execPath, [cli, "doctor", "--dir", app], { encoding: "utf8", env: { ...process.env, CI: "", NO_COLOR: "", FORCE_COLOR: "" } });
    assert.equal(r.status, 1);
    assert.match(r.stdout, /^FAIL app dir +.* has no pyproject\.toml$/m);
    assert.match(r.stdout, /^ +> file +pyproject\.toml$/m);
    assert.match(r.stdout, /^ +> fix +run this inside a pylair app/m);
    assert.match(r.stdout, /\n\n\d+ problem\(s\) found\n$/);
  });
});

describe("folded tool output", () => {
  const noisy = ["-e", "for (let i = 1; i <= 3; i++) console.log('line ' + i); console.error('warn from tool'); process.exit(Number(process.env.EXIT || 0))"];

  test("fancy: nothing is streamed, the live line shows the last output line", async () => {
    const h = harness(caps({ env: { LANG: "en_GB.UTF-8" } }));
    const seen = [];
    await h.ui.step("install deps", async (s) => {
      const orig = s.progress;
      s.progress = (t) => (seen.push(t), orig(t));
      await runTool(h.ui, process.execPath, noisy, process.cwd(), { step: s });
    });
    assert.ok(seen.includes("line 3"), JSON.stringify(seen));
    assert.ok(!stripAnsi(h.text()).includes("line 1\n"), "output is folded, not printed");
    assert.equal(h.errText(), "");
  });

  test("fancy: a failing tool dumps its whole log, then the step fails", async () => {
    const h = harness(caps({ stdoutIsTTY: false, env: { FORCE_COLOR: "1" } }));
    process.env.EXIT = "3";
    try {
      await assert.rejects(
        h.ui.step("build installer", (s) => runTool(h.ui, process.execPath, noisy, process.cwd(), { step: s })),
        /exited 3/,
      );
    } finally {
      delete process.env.EXIT;
    }
    assert.match(h.errText(), /line 1[\s\S]*line 2[\s\S]*line 3[\s\S]*warn from tool/);
    assert.match(stripAnsi(h.text()), /build installer/);
  });

  test("plain and --verbose stream through (nothing captured, nothing dumped)", async () => {
    for (const [c, verbose] of [[plainCaps(), false], [caps({ env: { LANG: "en_GB.UTF-8" } }), true]]) {
      const h = harness(c);
      await h.ui.step("x", (s) => runTool(h.ui, process.execPath, ["-e", "0"], process.cwd(), { step: s, verbose }));
      assert.equal(h.errText(), "");
    }
  });

  test("a tool that cannot start is reported as such", async () => {
    const h = harness(plainCaps());
    await assert.rejects(runTool(h.ui, "pylair-no-such-tool", [], process.cwd()), /failed to start/);
  });
});

describe("build artifacts", () => {
  test("lists this build's installers with sizes, newest build only", () => {
    const tauri = mkdtempSync(join(tmpdir(), "pylair-art-"));
    const bundle = join(tauri, "target", "release", "bundle");
    mkdirSync(join(bundle, "nsis"), { recursive: true });
    mkdirSync(join(bundle, "msi"), { recursive: true });
    mkdirSync(join(bundle, "macos", "Demo.app", "Contents"), { recursive: true });
    writeFileSync(join(bundle, "nsis", "Demo_1.0.0_x64-setup.exe"), "x".repeat(1000));
    writeFileSync(join(bundle, "nsis", "Demo_1.0.0_x64-setup.exe.sig"), "sig");
    writeFileSync(join(bundle, "msi", "Demo_1.0.0_x64.msi"), "y".repeat(2000));
    writeFileSync(join(bundle, "macos", "Demo.app", "Contents", "bin"), "z".repeat(500));
    const old = join(bundle, "nsis", "Old_0.9.0_x64-setup.exe");
    writeFileSync(old, "o");
    utimesSync(old, new Date(Date.now() - 3_600_000), new Date(Date.now() - 3_600_000));

    const found = findArtifacts(tauri, Date.now() - 60_000);
    const names = found.map((a) => relative(bundle, a.path).replace(/\\/g, "/"));
    assert.deepEqual(names, ["macos/Demo.app", "msi/Demo_1.0.0_x64.msi", "nsis/Demo_1.0.0_x64-setup.exe"]);
    assert.deepEqual(found.map((a) => a.bytes), [500, 2000, 1000]);
  });

  test("honours CARGO_TARGET_DIR and --debug; nothing built means nothing listed", () => {
    const tauri = mkdtempSync(join(tmpdir(), "pylair-art-"));
    assert.deepEqual(findArtifacts(tauri, 0), []);
    const target = mkdtempSync(join(tmpdir(), "pylair-tgt-"));
    mkdirSync(join(target, "debug", "bundle", "deb"), { recursive: true });
    writeFileSync(join(target, "debug", "bundle", "deb", "demo_1.0.0_amd64.deb"), "d");
    assert.equal(findArtifacts(tauri, 0, { debug: true, env: { CARGO_TARGET_DIR: target } }).length, 1);
    assert.equal(findArtifacts(tauri, 0, { debug: false, env: { CARGO_TARGET_DIR: target } }).length, 0);
  });
});

// The Windows PowerShell 5.1 mojibake ("ΓÇö" for an em dash) came from printing
// UTF-8 punctuation to a console on an OEM code page. Nothing the CLI prints
// may contain a non-ASCII character except the symbols module, which only
// emits them where the console is known to render them.
describe("no non-ASCII in anything the CLI prints", () => {
  /** Files whose non-ASCII strings are written to disk, not printed, or are the
   * gated symbol sets. Each is on the list for a reason. */
  const ALLOWED = new Map([
    ["ui/style.ts", "the gated Unicode symbol set"],
    ["codegen.ts", "the generated-file header; changing it would make `codegen --check` call every project stale"],
    ["build/notices.ts", "INDEX.txt lines, written into the bundle (never printed)"],
  ]);

  const files = [];
  (function walk(d) {
    for (const e of readdirSync(d)) {
      const p = join(d, e);
      if (statSync(p).isDirectory()) walk(p);
      else if (p.endsWith(".ts")) files.push(p);
    }
  })(srcDir);

  test("string literals and templates are ASCII (comments are free to say anything)", () => {
    const bad = [];
    for (const file of files) {
      const rel = relative(srcDir, file).replace(/\\/g, "/");
      if (ALLOWED.has(rel)) continue;
      const sf = ts.createSourceFile(file, readFileSync(file, "utf8"), ts.ScriptTarget.Latest, true);
      const visit = (n) => {
        const text =
          ts.isStringLiteral(n) || ts.isNoSubstitutionTemplateLiteral(n) || ts.isTemplateHead(n) || ts.isTemplateMiddle(n) || ts.isTemplateTail(n)
            ? n.text
            : undefined;
        if (text !== undefined && /[^\x00-\x7f]/.test(text)) {
          const { line } = sf.getLineAndCharacterOfPosition(n.getStart());
          bad.push(`${rel}:${line + 1}: ${JSON.stringify(text.slice(0, 60))}`);
        }
        ts.forEachChild(n, visit);
      };
      visit(sf);
    }
    assert.deepEqual(bad, []);
  });

  test("the allow-list is still needed (an entry with no non-ASCII string is dead weight)", () => {
    for (const [rel] of ALLOWED) {
      const src = readFileSync(join(srcDir, rel), "utf8");
      assert.ok(/[^\x00-\x7f]/.test(src), `${rel} no longer needs its exemption`);
    }
  });
});
