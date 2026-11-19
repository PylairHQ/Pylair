#!/usr/bin/env node
/**
 * pylair CLI. `codegen` is implemented (M2, docs/codegen.md);
 * dev/build/doctor arrive in M4.
 */

import { spawnSync } from "node:child_process";
import { writeFileSync, readFileSync, existsSync } from "node:fs";
import { join, resolve } from "node:path";
import { build } from "./build.js";
import { emit } from "./codegen.js";
import { dev } from "./dev.js";
import { doctor } from "./doctor.js";
import { loadManifest } from "./manifest.js";
import { initUi, ui } from "./ui/index.js";
import { initWrap } from "./wrap.js";
import { WRAP_FRAMEWORKS, type WrapFramework } from "./wrapconfig.js";
import { cliVersion } from "./version.js";

function usage(): void {
  ui.line(`pylair: Pylair CLI

Usage: pylair <command> [options]

Options for every command:
  --plain               Plain text: no colour, symbols, spinner or folded output
                        (the default when output is not a terminal, in CI, with
                        NO_COLOR set, or TERM=dumb; FORCE_COLOR=1 forces colour)

Commands:
  dev       Run the app with supervised Python + hot reload
  build     Produce a native installer with bundled Python
  doctor    Diagnose environment and app config
  codegen   Generate typed TypeScript stubs from @expose signatures
  wrap      Turn an existing Python web app into a desktop app (wrap mode)
  init      pylair init --mode wrap  (the same as \`pylair wrap\`)

wrap options:
  [entry]               The app's entry file (gradio / streamlit), e.g. app.py
  --framework <name>    command | gradio | streamlit (default: from your
                        dependencies, or command when --cmd is given)
  --cmd <argv...>       Server command for framework command; {port} is the
                        port Pylair picks. Everything after --cmd is the command.
  --source <dir>        Directory holding the app (default: the entry's, else app)
  --dir <path>          Project directory (default: cwd)
  --local <repo>        Scaffold from a Pylair checkout (needed until wrap ships)
  Needs a dependency manifest (pyproject.toml, ideally with uv.lock). A bare
  app.py gets an error that offers \`uv init\`; imports are never guessed.

codegen options:
  --module, -m <name>   Python module exposing @expose functions (required)
  --out, -o <file>      Output file (default: src/api.generated.ts)
  --python <path>       Interpreter to inspect the module
                        (default: $PYTHON, then python3)
  --check               Exit 1 if the output is stale; don't write

build options:
  --dir <path>          App directory (default: cwd)
  --bundles <list>      Comma-separated bundle kinds passed to tauri build
  --skip-fetch          Reuse already-staged resources/python
  --stage-only          Stage and verify the bundle, stop before tauri build
  --verbose             Stream uv and tauri output instead of folding it into
                        one progress line (also passed on to tauri build)
  Any other flags are forwarded to tauri build, including the value
  after a --flag; everything after -- is forwarded verbatim.

  Builds are native-only: run on each target OS (no cross-compile).
  Unsigned by default: set signing env vars for distribution builds.
`);
}

function argValue(argv: string[], ...flags: string[]): string | undefined {
  for (const f of flags) {
    const i = argv.indexOf(f);
    if (i !== -1) return argv[i + 1];
  }
  return undefined;
}

/** The app directory a command works on, as an absolute path: `--dir`
 * resolved against the current directory, or the current directory.
 * Commands spawn tools with `cwd` set to this directory while also passing
 * paths built from it, so a relative value would be resolved twice. */
export function resolveAppDir(dir: string | undefined, cwd: string = process.cwd()): string {
  return resolve(cwd, dir ?? ".");
}

function findPython(): string {
  if (process.env.PYTHON) return process.env.PYTHON;
  // Prefer the app venv (uv sync guarantees >=3.12); system python3 may
  // be older than tomllib needs, or absent entirely on Windows.
  const venv = join(
    process.cwd(),
    ".venv",
    process.platform === "win32" ? "Scripts/python.exe" : "bin/python",
  );
  if (existsSync(venv)) return venv;
  for (const cand of ["python3", "python"]) {
    const res = spawnProbe(cand);
    if (res) return cand;
  }
  return "python3"; // loadManifest will produce the friendly ENOENT error
}

function spawnProbe(cmd: string): boolean {
  const { status, error } = spawnSync(cmd, ["-c", "import sys;sys.exit(0)"]);
  return !error && status === 0;
}

/** Split `pylair build` args into ours and tauri's. Pass-through flags
 * forward `--flag`, `--flag value` (when the value doesn't start with
 * `-`) and `--flag=value`; everything after a bare `--` forwards
 * verbatim. Exported for tests. */
export function parseBuildArgs(rest: string[]): {
  dir?: string;
  bundles?: string;
  skipFetch: boolean;
  stageOnly: boolean;
  verbose: boolean;
  tauriArgs: string[];
} {
  let dir: string | undefined;
  let bundles: string | undefined;
  let skipFetch = false;
  let stageOnly = false;
  let verbose = false;
  const tauriArgs: string[] = [];
  for (let i = 0; i < rest.length; i++) {
    const a = rest[i];
    if (a === "--") {
      tauriArgs.push(...rest.slice(i + 1));
      break;
    }
    const eq = a.indexOf("=");
    const key = eq === -1 ? a : a.slice(0, eq);
    const inline = eq === -1 ? undefined : a.slice(eq + 1);
    if (key === "--dir") {
      dir = inline ?? rest[++i];
      continue;
    }
    if (key === "--bundles") {
      bundles = inline ?? rest[++i];
      continue;
    }
    if (key === "--skip-fetch") {
      skipFetch = true;
      continue;
    }
    if (key === "--stage-only") {
      stageOnly = true;
      continue;
    }
    if (a === "--plain") continue; // ours (see stripPlain), never tauri's
    if (a === "--verbose") verbose = true; // ours too, and still forwarded to tauri
    if (a.startsWith("--")) {
      tauriArgs.push(a);
      if (inline === undefined && i + 1 < rest.length && !rest[i + 1].startsWith("-"))
        tauriArgs.push(rest[++i]);
    }
  }
  if (bundles !== undefined) tauriArgs.push("--bundles", bundles);
  return { dir, bundles, skipFetch, stageOnly, verbose, tauriArgs };
}

/** Split `pylair wrap` / `pylair init` args. Everything after `--cmd` is the
 * server command, verbatim. Exported for tests. */
export function parseWrapArgs(rest: string[]): {
  dir?: string;
  entry?: string;
  framework?: WrapFramework;
  cmd?: string[];
  source?: string;
  local?: string;
  mode?: string;
} {
  const out: ReturnType<typeof parseWrapArgs> = {};
  for (let i = 0; i < rest.length; i++) {
    const a = rest[i];
    if (a === "--cmd") {
      out.cmd = rest.slice(i + 1);
      break;
    }
    const eq = a.indexOf("=");
    const key = a.startsWith("--") && eq !== -1 ? a.slice(0, eq) : a;
    const inline = a.startsWith("--") && eq !== -1 ? a.slice(eq + 1) : undefined;
    const value = () => inline ?? rest[++i];
    if (key === "--dir") out.dir = value();
    else if (key === "--framework") {
      const f = value();
      if (!(WRAP_FRAMEWORKS as readonly string[]).includes(f))
        throw new Error(`--framework must be one of ${WRAP_FRAMEWORKS.join(", ")}, got '${f}'`);
      out.framework = f as WrapFramework;
    } else if (key === "--source") out.source = value();
    else if (key === "--local") out.local = value();
    else if (key === "--mode") out.mode = value();
    else if (a.startsWith("-")) throw new Error(`unknown option '${a}' (see pylair --help)`);
    else if (out.entry === undefined) out.entry = a;
    else throw new Error(`unexpected argument '${a}'`);
  }
  if (out.cmd && out.cmd.length === 0) throw new Error("--cmd needs a command");
  return out;
}

function codegen(argv: string[]): number {
  const module = argValue(argv, "--module", "-m");
  if (!module) {
    ui.error("pylair codegen: --module is required");
    return 2;
  }
  const out = argValue(argv, "--out", "-o") ?? "src/api.generated.ts";
  const python = argValue(argv, "--python") ?? findPython();
  const check = argv.includes("--check");

  const manifest = loadManifest({ python, module });
  const ts = emit(manifest, module);

  if (check) {
    const stale = !existsSync(out) || readFileSync(out, "utf-8") !== ts;
    if (stale) {
      ui.error(`pylair codegen: ${out} is stale; re-run codegen`);
      return 1;
    }
    return 0;
  }
  writeFileSync(out, ts);
  const n = manifest.functions?.length ?? 0;
  ui.ok(`pylair codegen: wrote ${out} (${n} functions)`);
  return 0;
}

/** Drop the global `--plain` flag. It only counts before `--cmd` or a bare
 * `--`: after those the arguments belong to the wrapped server command or to
 * tauri, verbatim. */
export function stripPlain(argv: string[]): { argv: string[]; plain: boolean } {
  const stop = argv.findIndex((a) => a === "--cmd" || a === "--");
  const head = stop === -1 ? argv : argv.slice(0, stop);
  const tail = stop === -1 ? [] : argv.slice(stop);
  const plain = head.includes("--plain");
  return { argv: [...head.filter((a) => a !== "--plain"), ...tail], plain };
}

/** The one place a failed command is reported: `pylair <command>: message`. */
function failed(command: string, e: unknown): number {
  ui.error(`pylair ${command}: ${(e as Error).message}`);
  return 1;
}

export function main(rawArgv: string[]): number | Promise<number> {
  const { argv, plain } = stripPlain(rawArgv);
  initUi(plain ? ["--plain"] : []);
  const [command, ...rest] = argv;
  if (!command || command === "--help" || command === "-h") {
    usage();
    return command ? 0 : 1;
  }
  if (command === "--version" || command === "-v") {
    console.log(cliVersion());
    return 0;
  }
  if (command === "wrap" || command === "init") {
    let p: ReturnType<typeof parseWrapArgs>;
    try {
      p = parseWrapArgs(rest);
      if (command === "init" && p.mode !== "wrap")
        throw new Error(
          p.mode === "bridge"
            ? "bridge apps are scaffolded with `npx create-pylair <name>`; `pylair init` only supports --mode wrap"
            : "pass --mode wrap",
        );
    } catch (e) {
      ui.error(`pylair ${command}: ${(e as Error).message}`);
      return 2;
    }
    return initWrap({
      appDir: resolveAppDir(p.dir),
      entry: p.entry,
      framework: p.framework,
      cmd: p.cmd,
      source: p.source,
      local: p.local,
    }).then(
      () => 0,
      (e) => failed(command, e),
    );
  }
  if (command === "codegen") {
    try {
      return codegen(rest);
    } catch (e) {
      return failed("codegen", e);
    }
  }
  if (command === "build") {
    const { dir, skipFetch, stageOnly, verbose, tauriArgs } = parseBuildArgs(rest);
    return build({
      appDir: resolveAppDir(dir),
      tauriArgs,
      skipFetch,
      stageOnly,
      verbose,
    }).then(
      () => 0,
      (e) => failed("build", e),
    );
  }
  if (command === "dev") {
    const dir = resolveAppDir(argValue(rest, "--dir"));
    return dev({ appDir: dir }).then(
      (code) => code,
      (e) => failed("dev", e),
    );
  }
  if (command === "doctor") {
    const dir = resolveAppDir(argValue(rest, "--dir"));
    return doctor(dir).then((code) => code);
  }
  ui.error(`pylair: unknown command '${command}'. Run 'pylair --help'.`);
  return 1;
}

// The executable entry is bin.ts; importing this module must not run
// the CLI (tests import parseBuildArgs/main directly).
