import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { mkdirSync, mkdtempSync, writeFileSync, readFileSync, existsSync, rmSync, readdirSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { parse as parseToml } from "smol-toml";

import { loadConfig } from "../dist/config.js";
import {
  parseWrapConfig,
  wrapWarnings,
  launcherArgs,
  runtimeConfig,
} from "../dist/wrapconfig.js";
import { commandModule, smokeImports, STAGED_APP } from "../dist/wrapbuild.js";
import { parseWrapArgs, parseBuildArgs } from "../dist/cli.js";
import { initWrap, inferFramework, requirementsToDependencies, wrapToml, NO_MANIFEST_HELP } from "../dist/wrap.js";
import { wrapChecks } from "../dist/doctor.js";
import { build } from "../dist/build.js";

const REPO = fileURLToPath(new URL("../../..", import.meta.url));
const CREATE_PYLAIR = join(REPO, "packages", "create-pylair", "dist", "index.js");
const tmp = (p) => mkdtempSync(join(tmpdir(), `pylair-wrap-${p}-`));

const CMD = { framework: "command", cmd: ["python", "-m", "myapp", "--port", "{port}"] };

describe("[tool.pylair.wrap] parsing", () => {
  test("a command config gets its defaults", () => {
    const w = parseWrapConfig(CMD);
    assert.deepEqual(w, {
      framework: "command",
      entry: undefined,
      cmd: ["python", "-m", "myapp", "--port", "{port}"],
      readyPath: undefined, // the launcher picks the framework's own default
      readyTimeout: 120,
      downloads: "ask",
    });
  });

  test("adapters take an entry, and every key can be set", () => {
    const w = parseWrapConfig({
      framework: "streamlit",
      entry: "pages/home.py",
      "ready-path": "/_stcore/health",
      "ready-timeout": 300,
      downloads: "downloads",
    });
    assert.equal(w.entry, "pages/home.py");
    assert.equal(w.readyPath, "/_stcore/health");
    assert.equal(w.readyTimeout, 300);
    assert.equal(w.downloads, "downloads");
  });

  const bad = [
    [undefined, /needs a \[tool\.pylair\.wrap\] table/],
    [[], /must be a table/],
    [{}, /framework is required/],
    [{ framework: "flask", cmd: ["x"] }, /framework must be one of "command", "gradio", "streamlit", got "flask"/],
    [{ framework: "command" }, /needs cmd = \[/],
    [{ framework: "command", cmd: [] }, /non-empty array of strings/],
    [{ framework: "command", cmd: ["a", 1] }, /non-empty array of strings/],
    [{ framework: "command", cmd: [" "] }, /cmd\[0\]/],
    [{ framework: "command", cmd: ["x"], entry: "app.py" }, /entry is not used by framework "command"/],
    [{ framework: "gradio" }, /needs entry = "app\.py"/],
    [{ framework: "gradio", entry: "app.py", cmd: ["x"] }, /cmd is only for framework "command"/],
    [{ framework: "gradio", entry: "../app.py" }, /entry must be a path inside the source dir/],
    [{ framework: "gradio", entry: "/abs/app.py" }, /entry must be a path inside the source dir/],
    [{ framework: "gradio", entry: "C:\\app.py" }, /entry must be a path inside the source dir/],
    [{ ...CMD, "ready-path": "health" }, /ready-path must be a path starting with "\/"/],
    [{ ...CMD, "ready-path": "/a b" }, /ready-path/],
    [{ ...CMD, "ready-timeout": 0 }, /ready-timeout must be a whole number of seconds from 1 to 3600/],
    [{ ...CMD, "ready-timeout": 3601 }, /ready-timeout/],
    [{ ...CMD, "ready-timeout": 1.5 }, /ready-timeout/],
    [{ ...CMD, "ready-timeout": "120" }, /ready-timeout/],
    [{ ...CMD, downloads: "never" }, /downloads must be "ask" .* or "downloads"/],
    [{ ...CMD, ready_path: "/" }, /unknown key "ready_path"; allowed: framework, entry, cmd, ready-path, ready-timeout, downloads/],
  ];
  for (const [input, re] of bad)
    test(`rejects ${JSON.stringify(input)}`, () => {
      assert.throws(() => parseWrapConfig(input, "p/pyproject.toml"), (e) => {
        assert.match(e.message, re);
        return true;
      });
    });

  test("errors name the file", () => {
    assert.throws(() => parseWrapConfig({}, "/x/pyproject.toml"), /\(\/x\/pyproject\.toml\)/);
  });

  test("warnings: no {port}, and a network-wide bind", () => {
    assert.deepEqual(wrapWarnings(parseWrapConfig(CMD)), []);
    const noPort = wrapWarnings(parseWrapConfig({ framework: "command", cmd: ["python", "-m", "x"] }));
    assert.match(noPort[0], /no "\{port\}"/);
    const wide = wrapWarnings(
      parseWrapConfig({ framework: "command", cmd: ["uvicorn", "a:b", "--host", "0.0.0.0", "--port", "{port}"] }),
    );
    assert.match(wide.join("\n"), /binds 0\.0\.0\.0.*reachable from the network/);
    assert.match(
      wrapWarnings(parseWrapConfig({ framework: "command", cmd: ["srv", "--host=0.0.0.0", "{port}"] })).join(""),
      /0\.0\.0\.0/,
    );
  });
});

describe("the launcher interface generated from config", () => {
  test("command: --framework command ... --cmd last, ready-path before it", () => {
    assert.deepEqual(launcherArgs(parseWrapConfig({ ...CMD, "ready-path": "/up" })), [
      "-m", "pylair.wrap", "--framework", "command", "--ready-path", "/up",
      "--cmd", "python", "-m", "myapp", "--port", "{port}",
    ]);
  });

  test("an unset ready-path is NOT passed: the launcher's per-framework default stands", () => {
    // gradio probes /config and streamlit /_stcore/health because GET / can
    // stall during hydration; forcing "/" here would undo that.
    assert.ok(!launcherArgs(parseWrapConfig(CMD)).includes("--ready-path"));
    assert.ok(!launcherArgs(parseWrapConfig({ framework: "gradio", entry: "app.py" })).includes("--ready-path"));
    assert.ok(!launcherArgs(parseWrapConfig({ framework: "streamlit", entry: "app.py" })).includes("--ready-path"));
  });

  test("an adapter passes --entry and no --cmd", () => {
    assert.deepEqual(
      launcherArgs(parseWrapConfig({ framework: "gradio", entry: "app.py", "ready-path": "/health" })),
      ["-m", "pylair.wrap", "--framework", "gradio", "--entry", "app.py", "--ready-path", "/health"],
    );
  });

  test("the command's own flags can never read as launcher options", () => {
    // `--cmd` is last, so everything after it is the server's argv even when
    // it contains launcher-looking flags.
    const args = launcherArgs(parseWrapConfig({ framework: "command", cmd: ["srv", "--ready-path", "/x", "--framework", "y"] }));
    const at = args.indexOf("--cmd");
    assert.deepEqual(args.slice(at + 1), ["srv", "--ready-path", "/x", "--framework", "y"]);
    assert.equal(args.filter((a) => a === "--framework").length, 2);
    assert.equal(args.indexOf("--framework"), 2);
  });

  test("the runtime file is what the plugin reads", () => {
    const rt = runtimeConfig(parseWrapConfig({ ...CMD, "ready-timeout": 45, downloads: "downloads" }), "app");
    assert.deepEqual(rt, {
      version: 1,
      source: "app",
      launcher: launcherArgs(parseWrapConfig(CMD)),
      readyTimeoutSecs: 45,
      downloads: "downloads",
    });
  });

  test("python -m <module> is what the smoke test imports", () => {
    assert.equal(commandModule(["python", "-m", "uvicorn", "a:b"]), "uvicorn");
    assert.equal(commandModule(["python3.12", "-m", "my.app"]), "my.app");
    assert.equal(commandModule(["C:\\Python\\python.exe", "-m", "x"]), "x");
    assert.equal(commandModule(["python", "app.py"]), undefined);
    assert.equal(commandModule(["uvicorn", "a:b"]), undefined);
    assert.equal(commandModule(["python", "-m", "bad;rm"]), undefined);
    // the launcher is always imported: pylair must be one of the app's dependencies
    assert.deepEqual(smokeImports(parseWrapConfig({ framework: "gradio", entry: "a.py" })), ["pylair.wrap", "gradio"]);
    assert.deepEqual(smokeImports(parseWrapConfig({ framework: "streamlit", entry: "a.py" })), ["pylair.wrap", "streamlit"]);
    assert.deepEqual(smokeImports(parseWrapConfig({ framework: "command", cmd: ["python", "-m", "uvicorn"] })), ["pylair.wrap", "uvicorn"]);
    assert.deepEqual(smokeImports(parseWrapConfig({ framework: "command", cmd: ["./server"] })), ["pylair.wrap"]);
  });
});

describe("loadConfig: mode", () => {
  function project(toml) {
    const d = tmp("cfg");
    writeFileSync(join(d, "pyproject.toml"), toml);
    return d;
  }

  test("no mode is bridge, exactly as before", () => {
    const d = project('[tool.pylair]\napp-module = "backend.app"\n');
    const cfg = loadConfig(d);
    assert.equal(cfg.mode, "bridge");
    assert.equal(cfg.wrap, undefined);
    assert.equal(cfg.appModule, "backend.app");
    assert.equal(cfg.source, "backend");
  });

  test("bridge still requires app-module, and ignores a stray wrap table", () => {
    assert.throws(() => loadConfig(project('[tool.pylair]\nmode = "bridge"\n')), /app-module is required/);
    const cfg = loadConfig(project('[tool.pylair]\napp-module = "a"\n[tool.pylair.wrap]\nframework = "nope"\n'));
    assert.equal(cfg.mode, "bridge");
  });

  test("wrap needs no app-module, defaults source to app, and validates the table", () => {
    const cfg = loadConfig(
      project('[tool.pylair]\nmode = "wrap"\n[tool.pylair.wrap]\nframework = "command"\ncmd = ["python", "-m", "x"]\n'),
    );
    assert.equal(cfg.mode, "wrap");
    assert.equal(cfg.source, "app");
    assert.equal(cfg.appModule, "");
    assert.equal(cfg.wrap.framework, "command");
    assert.throws(() => loadConfig(project('[tool.pylair]\nmode = "wrap"\n')), /needs a \[tool\.pylair\.wrap\] table/);
    assert.throws(
      () => loadConfig(project('[tool.pylair]\nmode = "wrap"\n[tool.pylair.wrap]\nframework = "command"\n')),
      /needs cmd/,
    );
  });

  test("an unknown mode names the choices", () => {
    assert.throws(() => loadConfig(project('[tool.pylair]\nmode = "hybrid"\napp-module = "a"\n')), /mode must be "bridge" or "wrap", got "hybrid"/);
  });
});

describe("pylair wrap: arguments and helpers", () => {
  test("parseWrapArgs", () => {
    assert.deepEqual(parseWrapArgs(["app.py", "--framework", "gradio", "--dir", "x"]), {
      entry: "app.py", framework: "gradio", dir: "x",
    });
    // everything after --cmd is the server command, flags and all
    assert.deepEqual(parseWrapArgs(["--source", "srv", "--cmd", "python", "-m", "x", "--port", "{port}"]), {
      source: "srv", cmd: ["python", "-m", "x", "--port", "{port}"],
    });
    assert.equal(parseWrapArgs(["--mode=wrap"]).mode, "wrap");
    assert.throws(() => parseWrapArgs(["--framework", "flask"]), /--framework must be one of/);
    assert.throws(() => parseWrapArgs(["--nope"]), /unknown option/);
    assert.throws(() => parseWrapArgs(["a.py", "b.py"]), /unexpected argument/);
    assert.throws(() => parseWrapArgs(["--cmd"]), /--cmd needs a command/);
  });

  test("--stage-only is ours, not tauri's", () => {
    const p = parseBuildArgs(["--stage-only", "--bundles", "nsis"]);
    assert.equal(p.stageOnly, true);
    assert.deepEqual(p.tauriArgs, ["--bundles", "nsis"]);
    assert.equal(parseBuildArgs([]).stageOnly, false);
  });

  test("the framework is inferred from the manifest, never from imports", () => {
    assert.equal(inferFramework('dependencies = ["gradio>=5"]'), "gradio");
    assert.equal(inferFramework("streamlit==1.51\npandas"), "streamlit");
    assert.equal(inferFramework("gradio\nstreamlit"), undefined);
    assert.equal(inferFramework("fastapi\nuvicorn"), undefined);
    assert.equal(inferFramework("my-gradio-helpers"), undefined);
  });

  test("requirements.txt translates only when it is plain", () => {
    assert.deepEqual(requirementsToDependencies("# c\ngradio>=5  # inline\n\nnumpy\n"), ["gradio>=5", "numpy"]);
    assert.equal(requirementsToDependencies("-r other.txt\n"), undefined);
    assert.equal(requirementsToDependencies("x==1 --hash=sha256:abc\n"), undefined);
  });

  test("the toml we write parses back to the same config", () => {
    const w = parseWrapConfig({ framework: "command", cmd: ['py"x', "a b", "{port}"], "ready-path": "/h", "ready-timeout": 9, downloads: "downloads" });
    const doc = parseToml(wrapToml(w, "srv"));
    assert.equal(doc.tool.pylair.mode, "wrap");
    assert.equal(doc.tool.pylair.source, "srv");
    assert.deepEqual(parseWrapConfig(doc.tool.pylair.wrap), w);
  });
});

describe("pylair wrap: initialising a project", () => {
  const env = { PYLAIR_CREATE_PYLAIR: CREATE_PYLAIR };
  const withEnv = async (fn) => {
    const prev = process.env.PYLAIR_CREATE_PYLAIR;
    process.env.PYLAIR_CREATE_PYLAIR = env.PYLAIR_CREATE_PYLAIR;
    try {
      return await fn();
    } finally {
      if (prev === undefined) delete process.env.PYLAIR_CREATE_PYLAIR;
      else process.env.PYLAIR_CREATE_PYLAIR = prev;
    }
  };
  const quiet = () => {};

  test("a bare app.py gets the helpful error and the uv init offer", async () => {
    const d = tmp("bare");
    writeFileSync(join(d, "app.py"), "print(1)\n");
    const logs = [];
    let asked = "";
    await assert.rejects(
      initWrap({ appDir: d, entry: "app.py", framework: "gradio", log: (m) => logs.push(m), ask: async (q) => ((asked = q), false) }),
      /dependency manifest is required/,
    );
    assert.match(asked, /uv init/);
    const text = logs.join("\n");
    assert.match(text, /uv init/);
    assert.match(text, /uv add/);
    assert.match(text, /does not guess them from your imports/);
    assert.equal(NO_MANIFEST_HELP(d).includes(d), true);
    // nothing was written
    assert.deepEqual(readdirSync(d), ["app.py"]);
  });

  test("with a manifest it writes the config and the window shell, and keeps what exists", async () => {
    const d = tmp("init");
    const userPyproject = '[project]\nname = "my-app"\nversion = "1.2.3"\ndependencies = ["gradio>=5"]\n';
    writeFileSync(join(d, "pyproject.toml"), userPyproject);
    writeFileSync(join(d, "uv.lock"), "# lock\n");
    writeFileSync(join(d, "app.py"), "import gradio\n");
    writeFileSync(join(d, "package.json"), '{"name":"mine"}\n');
    const logs = [];
    const r = await withEnv(() =>
      initWrap({ appDir: d, entry: "app.py", local: REPO, log: (m) => logs.push(m) }),
    );
    assert.equal(r.config.framework, "gradio"); // from the manifest
    assert.equal(r.source, ".");
    // the project's own files are untouched
    assert.equal(readFileSync(join(d, "app.py"), "utf-8"), "import gradio\n");
    assert.equal(readFileSync(join(d, "package.json"), "utf-8"), '{"name":"mine"}\n');
    assert.ok(r.kept.includes("package.json"));
    assert.match(logs.join("\n"), /package\.json already exists/);
    // the config is appended after the user's own text and validates
    const py = readFileSync(join(d, "pyproject.toml"), "utf-8");
    assert.ok(py.startsWith(userPyproject));
    const cfg = loadConfig(d);
    assert.equal(cfg.mode, "wrap");
    assert.equal(cfg.wrap.framework, "gradio");
    assert.equal(cfg.wrap.entry, "app.py");
    // the shell, with no sample app and no React
    for (const f of ["src-tauri/tauri.conf.json", "src-tauri/Cargo.toml", "src-tauri/capabilities/default.json", "src-tauri/src/lib.rs", "ui/index.html", "ui/restarting.html", ".gitignore"])
      assert.ok(existsSync(join(d, f)), `missing ${f}`);
    assert.ok(!existsSync(join(d, "app")), "the sample app must not be added to an existing project");
    assert.ok(!existsSync(join(d, "requirements.txt")));
    const conf = JSON.parse(readFileSync(join(d, "src-tauri/tauri.conf.json"), "utf-8"));
    assert.equal(conf.productName, "my-app");
  });

  test("running it twice keeps the config and rewrites nothing", async () => {
    const d = tmp("twice");
    writeFileSync(join(d, "pyproject.toml"), '[project]\nname = "x"\nversion = "0"\n');
    writeFileSync(join(d, "uv.lock"), "");
    writeFileSync(join(d, "srv.py"), "");
    await withEnv(() => initWrap({ appDir: d, framework: "command", cmd: ["python", "-m", "srv", "--port", "{port}"], local: REPO, log: quiet }));
    const first = readFileSync(join(d, "pyproject.toml"), "utf-8");
    const again = await withEnv(() => initWrap({ appDir: d, local: REPO, log: quiet }));
    assert.equal(readFileSync(join(d, "pyproject.toml"), "utf-8"), first);
    assert.equal(again.wrote.length, 0, "no template file should be rewritten");
    assert.equal(again.config.framework, "command");
  });

  test("a requirements.txt-only project is accepted with a warning, and its deps carry over", async () => {
    const d = tmp("reqs");
    writeFileSync(join(d, "requirements.txt"), "streamlit==1.51\n");
    writeFileSync(join(d, "app.py"), "");
    const logs = [];
    await withEnv(() => initWrap({ appDir: d, entry: "app.py", local: REPO, log: (m) => logs.push(m) }));
    assert.match(logs.join("\n"), /only requirements\.txt/);
    const doc = parseToml(readFileSync(join(d, "pyproject.toml"), "utf-8"));
    assert.deepEqual(doc.project.dependencies, ["streamlit==1.51"]);
    assert.equal(loadConfig(d).wrap.framework, "streamlit");
  });

  test("refuses a bridge app, an ambiguous framework, and a missing entry", async () => {
    const bridge = tmp("bridge");
    writeFileSync(join(bridge, "pyproject.toml"), '[tool.pylair]\nmode = "bridge"\napp-module = "a"\n');
    await assert.rejects(initWrap({ appDir: bridge, log: quiet }), /bridge-mode app/);

    const amb = tmp("amb");
    writeFileSync(join(amb, "pyproject.toml"), '[project]\nname = "a"\nversion = "0"\n');
    await assert.rejects(initWrap({ appDir: amb, entry: "app.py", log: quiet }), /pass --framework command\|gradio\|streamlit/);

    const noent = tmp("noent");
    writeFileSync(join(noent, "pyproject.toml"), '[project]\nname = "a"\nversion = "0"\n');
    await assert.rejects(
      withEnv(() => initWrap({ appDir: noent, entry: "nope.py", framework: "gradio", local: REPO, log: quiet })),
      /entry nope\.py not found/,
    );
    assert.ok(!readFileSync(join(noent, "pyproject.toml"), "utf-8").includes("tool.pylair"), "a refused init must not edit pyproject.toml");
  });

  test("until wrap mode is published, the scaffolder refuses and nothing is written", async () => {
    const d = tmp("gate");
    writeFileSync(join(d, "pyproject.toml"), '[project]\nname = "a"\nversion = "0"\n');
    writeFileSync(join(d, "app.py"), "");
    const before = readFileSync(join(d, "pyproject.toml"), "utf-8");
    await assert.rejects(
      withEnv(() => initWrap({ appDir: d, entry: "app.py", framework: "gradio", log: quiet })),
      /not in the published Pylair packages yet/,
    );
    assert.equal(readFileSync(join(d, "pyproject.toml"), "utf-8"), before);
    assert.deepEqual(readdirSync(d).sort(), ["app.py", "pyproject.toml"]);
  });

  test("the CLI command itself: exit codes and messages", () => {
    const cli = fileURLToPath(new URL("../dist/bin.js", import.meta.url));
    const run = (args, cwd) => spawnSync(process.execPath, [cli, ...args], { encoding: "utf-8", cwd, env: { ...process.env, PYLAIR_CREATE_PYLAIR: CREATE_PYLAIR } });
    const d = tmp("clicmd");
    writeFileSync(join(d, "app.py"), "");
    const bare = run(["wrap", "app.py", "--framework", "gradio", "--local", REPO], d);
    assert.equal(bare.status, 1);
    assert.match(bare.stdout + bare.stderr, /uv init/);
    assert.equal(run(["init"], d).status, 2);
    assert.match(run(["init", "--mode", "bridge"], d).stderr, /create-pylair/);
    assert.equal(run(["wrap", "--framework", "flask"], d).status, 2);
  });
});

describe("wrap scaffold: doctor and the no-IPC guarantee", () => {
  function scaffold(name) {
    const d = tmp("scaf");
    const r = spawnSync(process.execPath, [CREATE_PYLAIR, name, "--mode", "wrap", "--local", REPO], { cwd: d, encoding: "utf-8" });
    assert.equal(r.status, 0, r.stderr);
    return join(d, name);
  }
  const collect = () => {
    const lines = [];
    return { lines, add: (name, ok, detail) => lines.push({ name, ok, detail }) };
  };

  test("a fresh scaffold passes every static check", () => {
    const app = scaffold("sample");
    const cfg = loadConfig(app);
    const { lines, add } = collect();
    wrapChecks(app, cfg, add);
    // `framework importable` needs a venv (uv sync); everything else is static.
    const failing = lines.filter((l) => !l.ok && l.name !== "framework importable");
    assert.deepEqual(failing, []);
    assert.ok(lines.some((l) => l.name === "wrapped page gets no IPC" && l.ok));
    assert.ok(lines.some((l) => l.name === 'window "main" has create: false' && l.ok));
  });

  test("doctor flags the problems a wrap user can make", () => {
    const app = scaffold("broken");
    // a capability that grants a remote origin gives the wrapped page IPC
    writeFileSync(
      join(app, "src-tauri/capabilities/leak.json"),
      JSON.stringify({ identifier: "leak", windows: ["main"], remote: { urls: ["http://127.0.0.1:*"] }, permissions: ["core:default"] }),
    );
    // a window Tauri creates itself would skip the plugin's hooks
    const confPath = join(app, "src-tauri/tauri.conf.json");
    const conf = JSON.parse(readFileSync(confPath, "utf-8"));
    delete conf.app.windows[0].create;
    writeFileSync(confPath, JSON.stringify(conf));
    rmSync(join(app, "ui/restarting.html"));
    rmSync(join(app, "requirements.txt"));
    const { lines, add } = collect();
    wrapChecks(app, loadConfig(app), add);
    const bad = Object.fromEntries(lines.filter((l) => !l.ok).map((l) => [l.name, l.detail]));
    assert.match(bad["wrapped page gets no IPC"], /leak\.json grants a remote url/);
    assert.match(bad['window "main" has create: false'], /"create": false/);
    assert.match(bad["ui/restarting.html"], /restarting\.html/);
    assert.match(bad["dependency manifest"], /uv init/);
  });

  test("the scaffold is a wrap app: no React, local pages only, one-line lib.rs", () => {
    const app = scaffold("shape");
    assert.ok(!existsSync(join(app, "src")), "wrap template has no React src/");
    assert.ok(!existsSync(join(app, "vite.config.ts")));
    assert.ok(!existsSync(join(app, "index.html")));
    const pkg = JSON.parse(readFileSync(join(app, "package.json"), "utf-8"));
    assert.ok(!pkg.dependencies, "no runtime npm dependencies (no React, no client)");
    const lib = readFileSync(join(app, "src-tauri/src/lib.rs"), "utf-8");
    assert.match(lib, /tauri_plugin_pylair::Builder::wrap\(\)/);
    const caps = readFileSync(join(app, "src-tauri/capabilities/default.json"), "utf-8");
    assert.ok(!/"remote"/.test(caps.replace(/`remote`/g, "")), "no capability may grant a remote url");
    assert.match(caps, /pylair:allow-python-retry/);
    assert.match(caps, /core:event:deny-emit"/);
    // the starting page only reaches the plugin through the injected global
    assert.equal(JSON.parse(readFileSync(join(app, "src-tauri/tauri.conf.json"), "utf-8")).app.withGlobalTauri, true);
  });
});

// The whole path for a command-mode FastAPI sample: scaffold, lock with uv,
// stage the bundled interpreter and packages, prune, compile, and prove the
// framework still imports with the bundled interpreter. It needs uv and the
// network (the interpreter download), so it is skipped without uv.
const haveUv = spawnSync("uv", ["--version"]).status === 0;
describe("pylair build --stage-only on a command-mode FastAPI sample", { skip: !haveUv && "uv is not installed", timeout: 900_000 }, () => {
  let app;
  const sh = (cmd, args, cwd) => {
    const r = spawnSync(cmd, args, { cwd, encoding: "utf-8" });
    assert.equal(r.status, 0, `${cmd} ${args.join(" ")}: ${r.stderr || r.stdout}`);
  };

  test("scaffold, lock, build: staged app, runtime file, and a passing import check", async () => {
    const d = tmp("e2e");
    sh(process.execPath, [CREATE_PYLAIR, "fastapi-sample", "--mode", "wrap", "--local", REPO], d);
    app = join(d, "fastapi-sample");
    sh("uv", ["lock"], app);
    // @tauri-apps/cli is not installed here, so `build` must stop before it.
    await build({ appDir: app, stageOnly: true });

    const res = join(app, "src-tauri", "resources");
    assert.ok(existsSync(join(res, STAGED_APP, "main.py")), "the app is staged as app/");
    assert.ok(existsSync(join(res, "site-packages", "uvicorn")), "locked dependencies are bundled");
    const rt = JSON.parse(readFileSync(join(res, "pylair-wrap.json"), "utf-8"));
    assert.equal(rt.version, 1);
    assert.equal(rt.source, "app");
    assert.deepEqual(rt.launcher.slice(0, 4), ["-m", "pylair.wrap", "--framework", "command"]);
    assert.ok(rt.launcher.includes("uvicorn"));
    assert.equal(rt.readyTimeoutSecs, 120);
    // The server command resolves its module from the staged dir, as the plugin runs it.
    const py = join(res, "python", process.platform === "win32" ? "python.exe" : "bin/python3");
    const sep = process.platform === "win32" ? ";" : ":";
    const r = spawnSync(py, ["-c", "import main, uvicorn; print(main.app.title)"], {
      cwd: join(res, STAGED_APP),
      encoding: "utf-8",
      env: { ...process.env, PYTHONPATH: `${join(res, STAGED_APP)}${sep}${join(res, "site-packages")}`, PYTHONNOUSERSITE: "1" },
    });
    assert.equal(r.status, 0, r.stderr);
  });

  test("over-pruning the framework fails the build, naming it", async () => {
    // Same project, with the framework pruned out of the bundle.
    const pp = join(app, "pyproject.toml");
    writeFileSync(pp, readFileSync(pp, "utf-8").replace('python-version = "3.12"', 'python-version = "3.12"\nprune = ["site-packages/uvicorn"]'));
    await assert.rejects(build({ appDir: app, stageOnly: true, skipFetch: true }), /framework import check \(pylair\.wrap, uvicorn\)/);
  });
});
