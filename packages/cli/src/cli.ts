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
import { keysInit } from "./keys.js";
import { loadManifest } from "./manifest.js";
import { initUi, ui } from "./ui/index.js";
import { interactive, PromptCancelled } from "./ui/prompts.js";
import { updateManifestCommand } from "./update.js";
import { upgradeCheck } from "./upgrade.js";
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
  keys      Update signing keys (pylair keys init)
  update    Auto-update artifacts (pylair update manifest)
  upgrade   Version alignment (pylair upgrade --check [--online])

dev options:
  --dir <path>          App directory (default: cwd)
  --inspect             Open the dev inspector window (docs/inspector.md)
  --inspect-payloads    Also show call payload previews in the inspector
  --skip-preflight      Skip the toolchain check before the Rust build

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
  --sign                Also produce signed updater artifacts (requires
                        TAURI_SIGNING_PRIVATE_KEY in the environment)
  --skip-fetch          Reuse already-staged resources/python
  --stage-only          Stage and verify the bundle, stop before tauri build
  --verbose             Stream uv and tauri output instead of folding it into
                        one progress line (also passed on to tauri build)
  Any other flags are forwarded to tauri build, including the value
  after a --flag; everything after -- is forwarded verbatim.

  Builds are native-only: run on each target OS (no cross-compile).
  Unsigned by default: set signing env vars for distribution builds.

keys options:
  pylair keys init      Generate the update signing keypair: private key to
                        --key-path (never inside the project), pubkey into
                        tauri.conf.json
    --dir <path>        App directory (default: cwd)
    --key-path <file>   Where the private key is written (must be outside
                        the project or git-ignored; prompted on a TTY)
    --password <pw>     Private-key password (default: the env var
                        TAURI_SIGNING_PRIVATE_KEY_PASSWORD, else prompted)
    --no-password       Generate an unprotected private key (warned;
                        never implied by a failed attempt)
    --force             Overwrite an existing pubkey/key (rotation needs the
                        OLD key - see docs/updates.md)

update options:
  pylair update manifest   Scan the bundle output for signed updater
                           artifacts and write latest.json
    --dir <path>        App directory (default: cwd)
    --base-url <url>    https directory hosting manifest + artifacts
                        (default: derived from plugins.updater.endpoints[0])
    --channel <name>    Write latest-<name>.json instead of latest.json
    --notes <text>      Release notes for the manifest
    --out <file>        Manifest path (default: <target>/release/bundle/)
    --insecure-dev      Allow an http:// base url (local dev/CI only)
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
  sign: boolean;
  skipFetch: boolean;
  stageOnly: boolean;
  verbose: boolean;
  tauriArgs: string[];
} {
  let dir: string | undefined;
  let bundles: string | undefined;
  let sign = false;
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
    if (key === "--sign") {
      sign = true;
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
  return { dir, bundles, sign, skipFetch, stageOnly, verbose, tauriArgs };
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
      (e) =>
        e instanceof PromptCancelled
          ? (ui.line("cancelled."), 130)
          : failed(command, e),
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
    const { dir, sign, skipFetch, stageOnly, verbose, tauriArgs } = parseBuildArgs(rest);
    return build({
      appDir: resolveAppDir(dir),
      tauriArgs,
      sign,
      skipFetch,
      stageOnly,
      verbose,
    }).then(
      () => 0,
      (e) => failed("build", e),
    );
  }
  if (command === "keys") {
    const [sub, ...subrest] = rest;
    if (sub !== "init") {
      ui.error(`pylair keys: unknown subcommand '${sub ?? ""}' (only 'init')`);
      return 2;
    }
    const force = subrest.includes("--force");
    const noPassword = subrest.includes("--no-password");
    const password = argValue(subrest, "--password");
    const canPrompt = interactive(process.stdin, process.env);
    return keysInit({
      appDir: resolveAppDir(argValue(subrest, "--dir")),
      keyPath: argValue(subrest, "--key-path"),
      password,
      noPassword,
      force,
      prompt: canPrompt ? { input: process.stdin, output: process.stdout, caps: ui.caps } : undefined,
    }).then(
      (code) => code,
      (e) =>
        e instanceof PromptCancelled
          ? (ui.line("cancelled."), 130)
          : failed("keys init", e),
    );
  }
  if (command === "update") {
    const [sub, ...subrest] = rest;
    if (sub !== "manifest") {
      ui.error(`pylair update: unknown subcommand '${sub ?? ""}' (only 'manifest')`);
      return 2;
    }
    try {
      return updateManifestCommand({
        appDir: resolveAppDir(argValue(subrest, "--dir")),
        baseUrl: argValue(subrest, "--base-url"),
        channel: argValue(subrest, "--channel"),
        notes: argValue(subrest, "--notes"),
        out: argValue(subrest, "--out"),
        insecureDev: subrest.includes("--insecure-dev"),
      });
    } catch (e) {
      return failed("update manifest", e);
    }
  }
  if (command === "dev") {
    const dir = resolveAppDir(argValue(rest, "--dir"));
    return dev({
      appDir: dir,
      inspect: rest.includes("--inspect"),
      inspectPayloads: rest.includes("--inspect-payloads"),
      skipPreflight: rest.includes("--skip-preflight"),
    }).then(
      (code) => code,
      (e) => failed("dev", e),
    );
  }
  if (command === "doctor") {
    const dir = resolveAppDir(argValue(rest, "--dir"));
    return doctor(dir).then((code) => code);
  }
  if (command === "upgrade") {
    if (!rest.includes("--check")) {
      ui.error("pylair upgrade: pass --check (read-only). Automatic edits are not implemented yet.");
      return 2;
    }
    const dir = resolveAppDir(argValue(rest, "--dir"));
    return upgradeCheck({ appDir: dir, online: rest.includes("--online") }).then(
      (code) => code,
      (e) => failed("upgrade", e),
    );
  }
  ui.error(`pylair: unknown command '${command}'. Run 'pylair --help'.`);
  return 1;
}

// The executable entry is bin.ts; importing this module must not run
// the CLI (tests import parseBuildArgs/main directly).
