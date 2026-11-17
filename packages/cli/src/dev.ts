/** `pylair dev` — supervised dev mode:
 *   uv sync → codegen → watch the python source dir → on change,
 *   re-run codegen and touch the restart trigger (the supervisor
 *   respawns the runtime within ~250ms) → run `tauri dev`.
 */

import { spawn, spawnSync } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, watch, writeFileSync } from "node:fs";
import { platform } from "node:os";
import { join } from "node:path";
import { importRoot, loadConfig, tauriCliEntry, type PylairConfig } from "./config.js";
import { emit } from "./codegen.js";
import { loadManifest } from "./manifest.js";
import { wrapWarnings } from "./wrapconfig.js";
import { ensureDevResources, writeRuntimeFile } from "./wrapbuild.js";

const isWin = platform() === "win32";

export function venvPython(appDir: string): string {
  const p = join(
    appDir,
    ".venv",
    isWin ? "Scripts/python.exe" : "bin/python",
  );
  return p;
}

/** Env for `python -m pylair.codegen`: the app module imports from the
 * source dir's import root, same rule as PYLAIR_PYTHONPATH below and the
 * build-time staged-import check. Exported for tests. */
export function codegenEnv(appDir: string, source: string): Record<string, string> {
  return {
    ...process.env,
    PYTHONPATH: importRoot(appDir, source),
  } as Record<string, string>;
}

function runCodegen(appDir: string, cfgModule: string, out: string, source: string): boolean {
  const python = venvPython(appDir);
  const env = codegenEnv(appDir, source);
  try {
    const manifest = loadManifest({ python, module: cfgModule, env });
    const ts = emit(manifest, cfgModule);
    const prev = existsSync(out) ? readFileSync(out, "utf-8") : null;
    if (prev !== ts) {
      writeFileSync(out, ts);
      console.log(`pylair: codegen → ${out} (${manifest.functions?.length ?? 0} functions)`);
      return true;
    }
  } catch (e) {
    console.warn(`pylair: codegen failed (${(e as Error).message}) — keeping last stubs`);
  }
  return false;
}

/** `bundle.resources` keys from tauri.conf.json (map or list form). */
function resourceKeys(appDir: string): string[] {
  try {
    const tconf = JSON.parse(
      readFileSync(join(appDir, "src-tauri", "tauri.conf.json"), "utf-8"),
    );
    const res = tconf?.bundle?.resources;
    return Array.isArray(res) ? res : Object.keys(res ?? {});
  } catch {
    return []; // unreadable: `tauri dev` will surface the real error itself
  }
}

/** Wrap mode: no codegen and no @expose surface. The plugin reads the
 * runtime file named by PYLAIR_WRAP_CONFIG, starts the launcher with the
 * project venv, and shows the server in the window. A change to a .py file
 * restarts the server (new port, the window follows). */
async function devWrap(appDir: string, cfg: PylairConfig): Promise<number> {
  for (const w of wrapWarnings(cfg.wrap!)) console.warn(`pylair: warning: ${w}`);
  const srcDir = join(appDir, cfg.source);
  if (!existsSync(srcDir))
    throw new Error(`source dir ${srcDir} not found ([tool.pylair] source)`);
  const stateDir = join(appDir, ".pylair");
  mkdirSync(stateDir, { recursive: true });
  const trigger = join(stateDir, "restart-trigger");
  writeFileSync(trigger, String(Date.now()));
  const runtime = join(stateDir, "pylair-wrap.json");
  // Absolute source: the plugin runs the server in it, not in a staged copy.
  writeRuntimeFile(runtime, cfg, srcDir);

  let timer: ReturnType<typeof setTimeout> | null = null;
  watch(srcDir, { recursive: true }, (_ev, file) => {
    if (!file?.endsWith(".py")) return;
    if (timer) clearTimeout(timer);
    timer = setTimeout(() => writeFileSync(trigger, String(Date.now())), 150);
  });

  ensureDevResources(appDir, resourceKeys(appDir));
  console.log("pylair: watching python source; starting tauri dev (wrap mode)");
  const child = spawn(process.execPath, [tauriCliEntry(appDir), "dev"], {
    cwd: appDir,
    stdio: "inherit",
    env: {
      ...process.env,
      PYLAIR_WRAP_CONFIG: runtime,
      PYLAIR_PYTHON: venvPython(appDir),
      PYLAIR_RESTART_TRIGGER: trigger,
      PYLAIR_DEV: "1",
    },
  });
  return new Promise((resolve) => {
    child.on("exit", (code) => resolve(code ?? 1));
    for (const sig of ["SIGINT", "SIGTERM"] as const)
      process.on(sig, () => child.kill(sig));
  });
}

export async function dev(opts: { appDir: string }): Promise<number> {
  const { appDir } = opts;
  const cfg = loadConfig(appDir);

  // Sync so the venv exists before codegen/dev need the interpreter.
  const sync = spawnSync("uv", ["sync"], { cwd: appDir, stdio: "inherit" });
  if (sync.error)
    throw new Error(`could not run uv: ${sync.error.message} — is uv on PATH?`);
  if (sync.status !== 0)
    throw new Error("uv sync failed — is pyproject.toml valid?");

  if (cfg.mode === "wrap") return devWrap(appDir, cfg);

  const srcDir = join(appDir, cfg.source);
  const stateDir = join(appDir, ".pylair");
  mkdirSync(stateDir, { recursive: true });
  const trigger = join(stateDir, "restart-trigger");
  writeFileSync(trigger, String(Date.now()));

  const stubs = join(appDir, "src", "api.generated.ts");
  mkdirSync(join(appDir, "src"), { recursive: true });
  runCodegen(appDir, cfg.appModule, stubs, cfg.source);

  // Watch python source; debounce bursts (saves, formatter runs).
  let timer: ReturnType<typeof setTimeout> | null = null;
  let changed = false;
  watch(srcDir, { recursive: true }, (_ev, file) => {
    if (!file?.endsWith(".py")) return;
    changed = true;
    if (timer) clearTimeout(timer);
    timer = setTimeout(() => {
      if (!changed) return;
      changed = false;
      writeFileSync(trigger, String(Date.now()));
      runCodegen(appDir, cfg.appModule, stubs, cfg.source);
    }, 150);
  });

  const env = {
    ...process.env,
    PYLAIR_APP_MODULE: cfg.appModule,
    PYLAIR_PYTHON: venvPython(appDir),
    PYLAIR_PYTHONPATH: importRoot(appDir, cfg.source),
    PYLAIR_RESTART_TRIGGER: trigger,
    PYLAIR_DEV: "1",
  };
  // tauri.conf.json maps resources/* paths that only exist after
  // `pylair build` stages them; `tauri dev` validates them eagerly and
  // fails the build on any missing one. Create empty dirs for dev —
  // `pylair build` replaces them with real staging anyway.
  try {
    const tconf = JSON.parse(
      readFileSync(join(appDir, "src-tauri", "tauri.conf.json"), "utf-8"),
    );
    const res = tconf?.bundle?.resources;
    const keys: string[] = Array.isArray(res) ? res : Object.keys(res ?? {});
    for (const rel of keys) {
      const p = join(appDir, "src-tauri", rel);
      if (!existsSync(p)) mkdirSync(p, { recursive: true });
    }
  } catch {
    // Conf unreadable — `tauri dev` will surface the real error itself.
  }

  console.log("pylair: watching python source; starting tauri dev");
  // Run the app's installed tauri CLI via node — no npx/npx.cmd shell
  // shim (which also trips Node's DEP0190 shell+args warning on Windows).
  const child = spawn(process.execPath, [tauriCliEntry(appDir), "dev"], {
    cwd: appDir,
    stdio: "inherit",
    env,
  });
  return new Promise((resolve) => {
    child.on("exit", (code) => resolve(code ?? 1));
    for (const sig of ["SIGINT", "SIGTERM"] as const)
      process.on(sig, () => child.kill(sig));
  });
}
