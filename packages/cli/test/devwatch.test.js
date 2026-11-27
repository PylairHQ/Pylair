import { test, describe, after } from "node:test";
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, watch, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

import {
  BUILTIN_IGNORES,
  devSessionAlive,
  manualTriggerContent,
  startDevWatch,
  triggerContent,
  watchMatch,
} from "../dist/devwatch.js";
import { loadConfig } from "../dist/config.js";

const bin = fileURLToPath(new URL("../dist/bin.js", import.meta.url));
const dirs = [];
const tmp = () => {
  const d = mkdtempSync(join(tmpdir(), "pylair-devwatch-"));
  dirs.push(d);
  return d;
};
after(() => {
  for (const d of dirs) rmSync(d, { recursive: true, force: true });
});

const base = { include: ["**/*.py"], ignore: [] };

describe("watchMatch include/ignore", () => {
  test("built-in ignores always apply", () => {
    for (const p of [
      ".venv/lib/python3.12/site-packages/x.py",
      "venv/x.py",
      "pkg/__pycache__/x.py",
      "node_modules/x.py",
      ".git/hooks/x.py",
      ".#app.py",
      "app.py~",
      "app.sw.py.tmp",
      "a/file.swp",
      "4913",
      ".pylair/restart-trigger.py",
    ]) {
      assert.equal(watchMatch(p, base), false, `${p} must be ignored`);
    }
    assert.equal(watchMatch("app.py", base), true);
    assert.equal(watchMatch("pkg/mod/deep.py", base), true);
  });

  test("user ignores subtract, include controls the positive set", () => {
    const set = { include: ["**/*.py", "**/*.toml"], ignore: ["fixtures/**"] };
    assert.equal(watchMatch("pyproject.toml", set), true);
    assert.equal(watchMatch("app.py", set), true);
    assert.equal(watchMatch("fixtures/data.py", set), false);
    assert.equal(watchMatch("src/app.js", set), false);
  });

  test("a nested .venv is ignored even deep in the tree", () => {
    assert.equal(watchMatch("sub/project/.venv/x.py", base), false);
    assert.equal(watchMatch("sub/project/x.py", base), true);
  });
});

describe("trigger content", () => {
  test("file trigger carries <millis> <file>; manual carries the marker", () => {
    const f = triggerContent("app.py");
    assert.match(f, /^\d+ app\.py$/);
    const m = manualTriggerContent();
    assert.match(m, /^\d+ manual$/);
    assert.notEqual(m.split(" ")[1], "app.py");
  });
});

describe("watch config validation", () => {
  const writePyproject = (dir, watchTable) => {
    writeFileSync(
      join(dir, "pyproject.toml"),
      `[project]\nname = "x"\nversion = "0.1.0"\n[tool.pylair]\napp-module = "app"\n${watchTable}`,
    );
  };

  test("defaults when the table is absent", () => {
    const dir = tmp();
    writePyproject(dir, "");
    const cfg = loadConfig(dir);
    assert.deepEqual(cfg.watch, { include: ["**/*.py"], ignore: [] });
  });

  test("include and ignore parse", () => {
    const dir = tmp();
    writePyproject(dir, `[tool.pylair.watch]\ninclude = ["**/*.py", "**/*.toml"]\nignore = ["fixtures/**"]\n`);
    const cfg = loadConfig(dir);
    assert.deepEqual(cfg.watch, { include: ["**/*.py", "**/*.toml"], ignore: ["fixtures/**"] });
  });

  test("unknown keys error clearly", () => {
    const dir = tmp();
    writePyproject(dir, `[tool.pylair.watch]\nincludes = ["**/*.py"]\n`);
    assert.throws(() => loadConfig(dir), /unknown key "includes"/);
  });

  test("non-string globs error clearly", () => {
    const dir = tmp();
    writePyproject(dir, `[tool.pylair.watch]\ninclude = [1]\n`);
    assert.throws(() => loadConfig(dir), /array of glob strings/);
  });
});

describe("startDevWatch burst debounce", () => {
  test("a burst of saves produces exactly one trigger", async () => {
    const dir = tmp();
    const src = join(dir, "backend");
    mkdirSync(src);
    writeFileSync(join(src, "app.py"), "x = 1\n");
    const trigger = join(dir, "trigger");
    writeFileSync(trigger, "0");
    let callbacks = 0;
    const w = startDevWatch({
      srcDir: src,
      trigger,
      set: base,
      onTrigger: () => callbacks++,
    });

    // Event-based: resolve when the trigger file's content first matches
    // the expected format, rather than sleeping a fixed duration.
    const triggered = new Promise((resolve) => {
      const tw = watch(trigger, () => {
        const c = readFileSync(trigger, "utf-8");
        if (/^\d+ app\.py$/.test(c)) {
          tw.close();
          resolve(c);
        }
      });
    });

    // burst: five rapid saves
    for (let i = 0; i < 5; i++) writeFileSync(join(src, "app.py"), `x = ${i}\n`);
    const content = await triggered;
    assert.match(content, /^\d+ app\.py$/);

    // The debounce window is 150ms: after a full additional window any
    // second trigger would have fired. Counting writes on the trigger file
    // over that window must show exactly one callback total.
    await new Promise((r) => setTimeout(r, 300));
    assert.equal(callbacks, 1, "the burst must collapse into one trigger");
    w.close();
  });
});

describe("pylair restart without a dev session", () => {
  test("errors clearly", () => {
    const dir = tmp();
    mkdirSync(join(dir, ".pylair"), { recursive: true });
    const res = spawnSync(process.execPath, [bin, "restart", "--dir", dir], { encoding: "utf8" });
    assert.equal(res.status, 1);
    assert.match(res.stderr + res.stdout, /no `pylair dev` session/);
  });

  test("devSessionAlive false on a missing pid file", () => {
    const dir = tmp();
    assert.equal(devSessionAlive(dir).alive, false);
  });
});
