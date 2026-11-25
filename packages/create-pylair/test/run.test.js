import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { PassThrough } from "node:stream";
import { existsSync, mkdtempSync, readdirSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { after } from "node:test";

import { run } from "../dist/run.js";
import { stripAnsi } from "../dist/ui/style.js";
import { scaffolderVersion } from "../dist/version.js";

const FANCY = { fancy: true, color: true, unicode: true, animate: false, columns: 80 };
const FANCY_ASCII = { fancy: true, color: true, unicode: false, animate: false, columns: 80 };
const PLAIN = { fancy: false, color: false, unicode: false, animate: false, columns: 80 };

const dirs = [];
const tmp = () => {
  const d = mkdtempSync(join(tmpdir(), "create-pylair-run-"));
  dirs.push(d);
  return d;
};
after(() => {
  for (const d of dirs) rmSync(d, { recursive: true, force: true });
});

/** A fake terminal for run(): PassThrough stdin/stdout/stderr, stdin marked
 * as a TTY only when asked. caps is injected so tests decide the console
 * mode; env defaults to CI-less. */
function io({ tty = false, caps = PLAIN, env = {}, cwd, preflight = () => [] } = {}) {
  const input = new PassThrough();
  if (tty) input.isTTY = true;
  const output = new PassThrough();
  const error = new PassThrough();
  let text = "";
  let err = "";
  output.on("data", (d) => (text += String(d)));
  error.on("data", (d) => (err += String(d)));
  // preflight defaults to an empty list: the real probe answers differ
  // per machine (CI runners lack cargo/MSVC), so only tests that inject
  // missing tools see the warning block.
  return {
    input, output, error, env, cwd, caps, preflight,
    text: () => text,
    err: () => err,
    plain: () => stripAnsi(text),
  };
}

/** Feed `send` to stdin whenever the accumulated output first matches
 * `when`. Steps run in order; input written early could be eaten by the
 * wrong prompt, so each chunk waits for its prompt's label. */
function drive(io, steps) {
  const queue = [...steps];
  let acc = "";
  io.output.on("data", (d) => {
    acc += stripAnsi(String(d));
    while (queue.length && queue[0].when.test(acc)) {
      io.input.write(queue.shift().send);
    }
  });
}

const EXPECTED_BRIDGE =
  "Created my-app/ (bridge)\n\n" +
  "Next:\n" +
  "  cd my-app\n" +
  "  npm install         # or pnpm install\n" +
  "  uv sync             # python env (https://docs.astral.sh/uv/)\n" +
  "  npx pylair dev      # supervised dev: python hot reload + vite\n" +
  "                      # (pnpm users: pnpm pylair dev / pnpm pylair build)\n" +
  "  npx pylair build    # native installer with bundled python\n" +
  "  npx pylair doctor   # diagnose environment any time\n\n";

describe("non-interactive behaviour is unchanged", () => {
  test("a piped run never prompts and finishes with the exact block", async () => {
    const cwd = tmp();
    const p = io({ cwd });
    assert.equal(await run(["my-app"], p), 0);
    assert.equal(p.text(), EXPECTED_BRIDGE);
    assert.equal(p.err(), "");
    assert.ok(existsSync(join(cwd, "my-app", "pyproject.toml")));
    assert.ok(existsSync(join(cwd, "my-app", "src-tauri", "tauri.conf.json")));
  });

  test("no name and no TTY fails loudly, exactly as before", async () => {
    const p = io({ cwd: tmp() });
    assert.equal(await run([], p), 1);
    assert.match(p.err(), /app name required/);
    assert.equal(p.text(), "");
  });

  test("every prompt question is answered by a flag", async () => {
    const cwd = tmp();
    const p = io({ cwd });
    const code = await run(
      ["w-app", "--mode", "wrap", "--framework", "streamlit", "--identifier", "com.x.y"],
      p,
    );
    assert.equal(code, 0);
    const shown = p.plain();
    assert.doesNotMatch(shown, /project name|what are you building|what does the app run|choose/);
    assert.match(shown, /Created w-app\/ \(wrap, streamlit\)/);
    assert.ok(existsSync(join(cwd, "w-app", "app", "app.py")));
    assert.ok(!existsSync(join(cwd, "w-app", "app", "main.py")));
  });

  test("missing prerequisites warn on stderr; stdout stays byte-exact", async () => {
    const cwd = tmp();
    const p = io({
      cwd,
      preflight: () => [
        { name: "uv", ok: false, detail: "not found", fix: "install uv: https://docs.astral.sh/uv/" },
        { name: "native toolchain", ok: false, detail: "no MSVC C++ build tools found", fix: "install Build Tools" },
      ],
    });
    assert.equal(await run(["my-app"], p), 0);
    // Scripted callers see stable stdout whatever the machine lacks;
    // the warning is diagnostics, so it goes to stderr.
    assert.equal(p.text(), EXPECTED_BRIDGE);
    const warn = p.err();
    assert.match(warn, /Missing prerequisites/);
    assert.match(warn, /uv: not found - install uv: https:\/\/docs\.astral\.sh\/uv\//);
    assert.match(warn, /native toolchain: no MSVC C\+\+ build tools found - install Build Tools/);
    assert.ok(existsSync(join(cwd, "my-app", "pyproject.toml")));
  });

  test("a clean preflight prints no warning and keeps the exact outro", async () => {
    const p = io({ cwd: tmp() });
    assert.equal(await run(["my-app"], p), 0);
    assert.equal(p.text(), EXPECTED_BRIDGE);
  });

  test("--into prints only its JSON line: no wordmark, no prompts", async () => {
    const cwd = tmp();
    const p = io({ cwd, tty: true, caps: FANCY }); // even on a TTY --into stays machine-only
    const code = await run(
      ["--mode", "wrap", "--into", cwd, "--name", "existing"],
      p,
    );
    assert.equal(code, 0);
    const out = p.plain();
    assert.doesNotMatch(out, /desktop apps for python|project name|what are you building/);
    const last = out.trim().split(/\r?\n/).pop();
    const parsed = JSON.parse(last);
    assert.ok(Array.isArray(parsed.written));
  });

  test("CI set means no prompts even on a TTY", async () => {
    const p = io({ cwd: tmp(), tty: true, caps: FANCY, env: { CI: "true" } });
    assert.equal(await run([], p), 1);
    assert.match(p.err(), /app name required/);
    assert.doesNotMatch(p.plain(), /project name: /);
  });
});

describe("the wordmark, fancy mode only", () => {
  test("unicode consoles get the box-drawn art once, at the start", async () => {
    const cwd = tmp();
    const p = io({ cwd, tty: true, caps: FANCY });
    drive(p, [{ when: /project name/, send: "\x03" }]); // cancel: wordmark only proves it showed
    assert.equal(await run([], p), 130);
    const shown = p.plain();
    const hits = shown.split("desktop apps for python").length - 1;
    assert.equal(hits, 1, "the wordmark shows exactly once");
    assert.match(shown, /desktop apps for python/);
    assert.match(shown, new RegExp(`v${scaffolderVersion().replace(/\./g, "\\.")}`));
  });

  test("an ASCII console (PowerShell 5.1, non-UTF-8) gets one plain line", async () => {
    const cwd = tmp();
    const p = io({ cwd, caps: FANCY_ASCII }); // fancy but no unicode
    await run(["a-app"], p);
    const shown = p.text();
    assert.match(shown, new RegExp(`^pylair ${scaffolderVersion().replace(/\./g, "\\.")}\\r?\\n`, "m"));
    assert.doesNotMatch(shown, /desktop apps for python|┌/);
  });

  test("plain output shows no wordmark at all", async () => {
    const cwd = tmp();
    const p = io({ cwd, caps: PLAIN });
    await run(["a-app"], p);
    assert.doesNotMatch(p.text(), /desktop apps for python|^pylair \d/m);
  });
});

describe("interactive prompts fill only the missing answers", () => {
  test("a bare run on a TTY asks name, then mode; bridge is the first choice", async () => {
    const cwd = tmp();
    const p = io({ cwd, tty: true, caps: FANCY });
    drive(p, [
      { when: /project name/, send: "p-app\n" },
      { when: /what are you building\?/, send: "\r" }, // first option: bridge
      { when: /add auto-update support\?/, send: "\r" }, // default: no
    ]);
    assert.equal(await run([], p), 0);
    assert.ok(existsSync(join(cwd, "p-app", "src", "App.tsx")));
    assert.match(p.plain(), /Created p-app\/ \(bridge\)/);
  });

  test("a given name skips its prompt but still asks the mode", async () => {
    const cwd = tmp();
    const p = io({ cwd, tty: true, caps: FANCY });
    drive(p, [
      { when: /what are you building\?/, send: "\r" },
      { when: /add auto-update support\?/, send: "\r" },
    ]);
    assert.equal(await run(["named-app"], p), 0);
    const shown = p.plain();
    assert.doesNotMatch(shown, /project name: /);
    assert.match(shown, /Created named-app\/ \(bridge\)/);
  });

  test("wrap via the menu asks the framework and scaffolds the variant", async () => {
    const cwd = tmp();
    const p = io({ cwd, tty: true, caps: FANCY });
    drive(p, [
      { when: /what are you building\?/, send: "\x1b[B\r" }, // wrap
      { when: /what does the app run\?/, send: "\x1b[B\r" }, // gradio
      { when: /add auto-update support\?/, send: "\r" },
    ]);
    assert.equal(await run(["w-app"], p), 0);
    const py = readFileSync(join(cwd, "w-app", "pyproject.toml"), "utf8");
    assert.match(py, /framework = "gradio"/);
    assert.match(py, /entry = "app\.py"/);
    assert.match(py, /"gradio>=5,<6"/);
    assert.ok(existsSync(join(cwd, "w-app", "app", "app.py")));
    assert.ok(!existsSync(join(cwd, "w-app", "app", "main.py")));
    assert.match(p.plain(), /Created w-app\/ \(wrap, gradio\)/);
  });

  test("an invalid name typed at the prompt is rejected and re-asked", async () => {
    const cwd = tmp();
    const p = io({ cwd, tty: true, caps: FANCY });
    drive(p, [
      { when: /project name/, send: "Bad Name\n" },
      { when: /use lowercase/, send: "good-app\n" },
      { when: /what are you building\?/, send: "\r" },
      { when: /add auto-update support\?/, send: "\r" },
    ]);
    assert.equal(await run([], p), 0);
    assert.ok(existsSync(join(cwd, "good-app")));
  });

  test("answering yes to auto-update injects the updater plumbing", async () => {
    const cwd = tmp();
    const p = io({ cwd, tty: true, caps: FANCY });
    drive(p, [
      { when: /what are you building\?/, send: "\r" },
      { when: /add auto-update support\?/, send: "\x1b[B\r" }, // second option: yes
    ]);
    assert.equal(await run(["up-app"], p), 0);
    const conf = JSON.parse(readFileSync(join(cwd, "up-app/src-tauri/tauri.conf.json"), "utf8"));
    assert.equal(conf.plugins?.updater?.pubkey, "<generated by pylair keys init>");
    assert.match(readFileSync(join(cwd, "up-app/src-tauri/Cargo.toml"), "utf8"), /tauri-plugin-updater/);
    assert.match(p.plain(), /npx pylair keys init/);
  });
});

describe("the finished output matches the console", () => {
  test("fancy: resolved prompt lines and a styled outro", async () => {
    const cwd = tmp();
    const p = io({ cwd, tty: true, caps: FANCY });
    drive(p, [
      { when: /project name/, send: "f-app\n" },
      { when: /what are you building\?/, send: "\r" }, // first option: bridge
      { when: /add auto-update support\?/, send: "\r" },
    ]);
    assert.equal(await run([], p), 0);
    const shown = p.plain();
    // every answered prompt resolves the same way: mark, label, answer
    assert.match(shown, /✔ project name: f-app/);
    assert.match(shown, /✔ what are you building\? a new UI that calls Python/);
    assert.doesNotMatch(shown, /\?:/, "a label ending in '?' gains no colon");
    assert.match(shown, /✔ Created f-app\/ \(bridge\)/);
    // styled outro: commands in accent, comments muted, a bold Next header
    const raw = p.text();
    assert.match(raw, /\u001b\[36m *npm install/);
    assert.match(raw, /\u001b\[90m# or pnpm install\u001b\[39m/);
    assert.match(raw, /\u001b\[1mNext:\u001b\[22m/);
  });

  test("fancy ASCII: the same flow in the [ok] symbol set, no non-ASCII bytes", async () => {
    const cwd = tmp();
    const p = io({ cwd, tty: true, caps: FANCY_ASCII });
    drive(p, [
      { when: /project name/, send: "a-app\n" },
      { when: /what are you building\?/, send: "\r" },
      { when: /add auto-update support\?/, send: "\r" },
    ]);
    assert.equal(await run([], p), 0);
    const shown = p.plain();
    assert.match(shown, /\[ok\] project name: a-app/);
    assert.match(shown, /\[ok\] what are you building\? a new UI that calls Python/);
    assert.match(shown, /\[ok\] Created a-app\/ \(bridge\)/);
    for (const ch of p.text())
      assert.ok(ch.codePointAt(0) < 128, `non-ASCII byte in ASCII output: ${JSON.stringify(ch)}`);
  });

  test("plain TTY: numbered select, resolved lines, and the legacy outro verbatim", async () => {
    const cwd = tmp();
    const p = io({ cwd, tty: true, caps: PLAIN });
    drive(p, [
      { when: /project name/, send: "p-app\n" },
      { when: /choose 1-2/, send: "1\n" }, // first option: bridge
      { when: /add auto-update support\?/, send: "1\n" }, // numbered select: no
    ]);
    assert.equal(await run([], p), 0);
    const shown = p.plain();
    assert.match(shown, /1\) a new UI that calls Python/);
    // askText: no resolved line in plain mode - the typed answer is already on screen
    assert.equal(p.text().split("project name: p-app").length - 1, 1);
    assert.match(shown, /\[ok\] what are you building\? a new UI that calls Python/);
    const tail =
      "Created p-app/ (bridge)\n\n" +
      "Next:\n" +
      "  cd p-app\n" +
      "  npm install         # or pnpm install\n" +
      "  uv sync             # python env (https://docs.astral.sh/uv/)\n" +
      "  npx pylair dev      # supervised dev: python hot reload + vite\n" +
      "                      # (pnpm users: pnpm pylair dev / pnpm pylair build)\n" +
      "  npx pylair build    # native installer with bundled python\n" +
      "  npx pylair doctor   # diagnose environment any time\n\n";
    assert.ok(p.text().endsWith(tail), "plain mode keeps the block byte for byte");
  });
});

describe("Ctrl+C at any prompt exits cleanly and writes nothing", () => {
  test("cancelled at the name prompt", async () => {
    const cwd = tmp();
    const p = io({ cwd, tty: true, caps: FANCY });
    drive(p, [{ when: /project name/, send: "\x03" }]);
    assert.equal(await run([], p), 130);
    assert.match(p.plain(), /cancelled\./);
    assert.deepEqual(readdirSync(cwd), []);
  });

  test("cancelled at the mode select", async () => {
    const cwd = tmp();
    const p = io({ cwd, tty: true, caps: FANCY });
    drive(p, [{ when: /what are you building\?/, send: "\x03" }]);
    assert.equal(await run(["my-app"], p), 130);
    assert.match(p.plain(), /cancelled\./);
    assert.deepEqual(readdirSync(cwd), []);
  });

  test("cancelled at the framework select", async () => {
    const cwd = tmp();
    const p = io({ cwd, tty: true, caps: FANCY });
    drive(p, [{ when: /what does the app run\?/, send: "\x03" }]);
    assert.equal(await run(["w-app", "--mode", "wrap"], p), 130);
    assert.match(p.plain(), /cancelled\./);
    assert.deepEqual(readdirSync(cwd), []);
  });
});

describe("flag edge cases", () => {
  test("--framework alone implies wrap mode", async () => {
    const cwd = tmp();
    const p = io({ cwd });
    assert.equal(await run(["g-app", "--framework", "gradio"], p), 0);
    assert.match(readFileSync(join(cwd, "g-app", "pyproject.toml"), "utf8"), /mode = "wrap"/);
    assert.match(p.plain(), /Created g-app\/ \(wrap, gradio\)/);
  });

  test("--framework with --mode bridge is an error", async () => {
    const p = io({ cwd: tmp() });
    assert.equal(await run(["x-app", "--mode", "bridge", "--framework", "gradio"], p), 1);
    assert.match(p.err(), /--framework is for wrap mode/);
  });

  test("an unknown --framework names the choices", async () => {
    const p = io({ cwd: tmp() });
    assert.equal(await run(["x-app", "--framework", "flask"], p), 1);
    assert.match(p.err(), /--framework must be "command", "gradio" or "streamlit"/);
  });

  test("--mode wrap without --framework scaffolds the command sample", async () => {
    const cwd = tmp();
    const p = io({ cwd });
    assert.equal(await run(["c-app", "--mode", "wrap"], p), 0);
    const py = readFileSync(join(cwd, "c-app", "pyproject.toml"), "utf8");
    assert.match(py, /framework = "command"/);
    assert.match(py, /cmd = \["python", "-m", "uvicorn"/);
    assert.ok(existsSync(join(cwd, "c-app", "app", "main.py")));
    assert.match(p.plain(), /Created c-app\/ \(wrap, command\)/);
  });
});
