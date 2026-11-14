#!/usr/bin/env node
/**
 * pylair CLI. `codegen` is implemented (M2, docs/codegen.md);
 * dev/build/doctor arrive in M4.
 */

import { spawnSync } from "node:child_process";
import { writeFileSync, readFileSync, existsSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { build } from "./build.js";
import { emit } from "./codegen.js";
import { dev } from "./dev.js";
import { doctor } from "./doctor.js";
import { loadManifest } from "./manifest.js";

const COMMANDS = ["dev", "build", "doctor", "codegen"] as const;

function usage(): void {
  console.log(`pylair — Pylair CLI

Usage: pylair <command> [options]

Commands:
  dev       Run the app with supervised Python + hot reload
  build     Produce a native installer with bundled Python
  doctor    Diagnose environment and app config
  codegen   Generate typed TypeScript stubs from @expose signatures

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
  Any other flags are forwarded to tauri build, including the value
  after a --flag; everything after -- is forwarded verbatim.

  Builds are native-only: run on each target OS (no cross-compile).
  Unsigned by default — set signing env vars for distribution builds.
`);
}

function argValue(argv: string[], ...flags: string[]): string | undefined {
  for (const f of flags) {
    const i = argv.indexOf(f);
    if (i !== -1) return argv[i + 1];
  }
  return undefined;
}

function findPython(): string {
  if (process.env.PYTHON) return process.env.PYTHON;
  // Prefer the app venv (uv sync guarantees >=3.12) — system python3 may
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
  tauriArgs: string[];
} {
  let dir: string | undefined;
  let bundles: string | undefined;
  let skipFetch = false;
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
    if (a.startsWith("--")) {
      tauriArgs.push(a);
      if (inline === undefined && i + 1 < rest.length && !rest[i + 1].startsWith("-"))
        tauriArgs.push(rest[++i]);
    }
  }
  if (bundles !== undefined) tauriArgs.push("--bundles", bundles);
  return { dir, bundles, skipFetch, tauriArgs };
}

function codegen(argv: string[]): number {
  const module = argValue(argv, "--module", "-m");
  if (!module) {
    console.error("pylair codegen: --module is required");
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
      console.error(`pylair codegen: ${out} is stale — re-run codegen`);
      return 1;
    }
    return 0;
  }
  writeFileSync(out, ts);
  const n = manifest.functions?.length ?? 0;
  console.log(`pylair codegen: wrote ${out} (${n} functions)`);
  return 0;
}

/** The CLI's own version, read from package.json relative to this built
 * file (dist/cli.js -> ../package.json), so it works from any cwd and
 * from the published tarball. Falls back to 0.0.0 if unreadable. */
function cliVersion(): string {
  try {
    const pkg = JSON.parse(
      readFileSync(fileURLToPath(new URL("../package.json", import.meta.url)), "utf-8"),
    );
    return String(pkg.version ?? "0.0.0");
  } catch {
    return "0.0.0";
  }
}

export function main(argv: string[]): number | Promise<number> {
  const [command, ...rest] = argv;
  if (!command || command === "--help" || command === "-h") {
    usage();
    return command ? 0 : 1;
  }
  if (command === "--version" || command === "-v") {
    console.log(cliVersion());
    return 0;
  }
  if (command === "codegen") {
    try {
      return codegen(rest);
    } catch (e) {
      console.error(`pylair codegen: ${(e as Error).message}`);
      return 1;
    }
  }
  if (command === "build") {
    const { dir, skipFetch, tauriArgs } = parseBuildArgs(rest);
    return build({
      appDir: dir ?? process.cwd(),
      tauriArgs,
      skipFetch,
    }).then(
      () => 0,
      (e) => {
        console.error(`pylair build: ${(e as Error).message}`);
        return 1;
      },
    );
  }
  if (command === "dev") {
    const dir = argValue(rest, "--dir") ?? process.cwd();
    return dev({ appDir: dir }).then(
      (code) => code,
      (e) => {
        console.error(`pylair dev: ${(e as Error).message}`);
        return 1;
      },
    );
  }
  if (command === "doctor") {
    const dir = argValue(rest, "--dir") ?? process.cwd();
    return doctor(dir).then((code) => code);
  }
  console.error(`pylair: unknown command '${command}'. Run 'pylair --help'.`);
  return 1;
}

// The executable entry is bin.ts — importing this module must not run
// the CLI (tests import parseBuildArgs/main directly).
