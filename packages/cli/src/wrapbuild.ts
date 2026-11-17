/** Wrap-mode pieces of `pylair dev`, `pylair build` and `pylair doctor`:
 * what to import to prove the framework survived pruning, the runtime file
 * the plugin reads, and what to stage.
 */

import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import type { PylairConfig } from "./config.js";
import { runtimeConfig, type WrapConfig } from "./wrapconfig.js";

/** The directory the app is staged into inside the bundle, whatever the
 * source dir is called. Fixed so `tauri.conf.json` never needs editing when
 * `source` changes (`"resources/app": "app/"`). */
export const STAGED_APP = "app";

/** Top-level names never staged when the source dir is the project root
 * (`source = "."`): the project's own tooling, not the app. */
export const ROOT_SOURCE_EXCLUDES = ["src-tauri", "node_modules", "ui", "dist", "target"];

/** `python -m uvicorn ...` style: the module a `command` runs, if it is run
 * with the interpreter's `-m`. */
export function commandModule(cmd: string[] | undefined): string | undefined {
  if (!cmd || cmd.length < 3) return undefined;
  const prog = cmd[0].replace(/\\/g, "/").split("/").pop()!.toLowerCase().replace(/\.exe$/, "");
  if (!/^python(\d+(\.\d+)*)?$/.test(prog)) return undefined;
  if (cmd[1] !== "-m") return undefined;
  const mod = cmd[2];
  return /^[A-Za-z_][A-Za-z0-9_.]*$/.test(mod) ? mod : undefined;
}

/** What to import to show the framework is present and intact: gradio and
 * streamlit by name; for `command`, the `-m` module when there is one. */
export function smokeImports(w: WrapConfig): string[] {
  if (w.framework === "gradio") return ["gradio"];
  if (w.framework === "streamlit") return ["streamlit"];
  const m = commandModule(w.cmd);
  return m ? [m] : [];
}

/** Run the imports with `python`. Returns the failure text, or undefined
 * when every import worked (or there was nothing to import). */
export function runSmokeImports(
  python: string,
  mods: string[],
  env: Record<string, string>,
  cwd: string,
): string | undefined {
  if (!mods.length) return undefined;
  const code = mods.map((m) => `import ${m}`).join("; ") + `; print("imports ok")`;
  const r = spawnSync(python, ["-c", code], { encoding: "utf-8", cwd, env: { ...process.env, ...env } });
  if (r.error) return `could not run ${python}: ${r.error.message}`;
  if (r.status === 0) return undefined;
  return (r.stderr || r.stdout || `exit ${r.status}`).trim().split(/\r?\n/).slice(-3).join(" | ");
}

/** Write the file the plugin reads at startup. */
export function writeRuntimeFile(path: string, cfg: PylairConfig, source: string): void {
  if (!cfg.wrap) throw new Error("not a wrap-mode app");
  mkdirSync(dirname(path), { recursive: true });
  writeFileSync(path, JSON.stringify(runtimeConfig(cfg.wrap, source), null, 2) + "\n");
}

/** `tauri dev` validates every `bundle.resources` path eagerly, but they
 * only exist after `pylair build`: make placeholders (a dir, or an empty
 * object for a `.json` file). `pylair build` replaces them. */
export function ensureDevResources(appDir: string, keys: string[]): void {
  for (const rel of keys) {
    const p = join(appDir, "src-tauri", rel);
    if (existsSync(p)) continue;
    if (rel.endsWith(".json")) {
      mkdirSync(dirname(p), { recursive: true });
      writeFileSync(p, "{}\n");
    } else {
      mkdirSync(p, { recursive: true });
    }
  }
}

/** Is `source` the project root itself? */
export function sourceIsRoot(appDir: string, source: string): boolean {
  return resolve(appDir, source) === resolve(appDir);
}
