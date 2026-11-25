/** `pylair dev`: supervised dev mode:
 *   uv sync, codegen, watch the python source dir; on change,
 *   re-run codegen and touch the restart trigger (the supervisor
 *   respawns the runtime within ~250ms); run `tauri dev`.
 */

import { spawn } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, watch, writeFileSync } from "node:fs";
import { platform } from "node:os";
import { join } from "node:path";
import { runTool } from "./build/util.js";
import { importRoot, loadConfig, tauriCliEntry, type PylairConfig } from "./config.js";
import { emit } from "./codegen.js";
import { loadManifest } from "./manifest.js";
import { formatMissing, missingTools, preflightChecks } from "./preflight.js";
import { ui } from "./ui/index.js";
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
      ui.info(`pylair: codegen wrote ${out} (${manifest.functions?.length ?? 0} functions)`);
      return true;
    }
  } catch (e) {
    ui.warn(`pylair: codegen failed (${(e as Error).message}); keeping last stubs`, { bare: true });
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

/** The dev server URL from tauri.conf.json (`build.devUrl`), if there is one. */
function devUrl(appDir: string): string | undefined {
  try {
    const t = JSON.parse(readFileSync(join(appDir, "src-tauri", "tauri.conf.json"), "utf-8"));
    const u = t?.build?.devUrl ?? t?.build?.devPath;
    return typeof u === "string" ? u : undefined;
  } catch {
    return undefined;
  }
}

/** The short header printed as `tauri dev` is started: what is running and
 * how to stop it. (Printed at launch, not when the window appears: tauri dev
 * owns the terminal from here and does not tell us.) */
function devHeader(appDir: string, cfg: PylairConfig): void {
  const wrap = cfg.mode === "wrap";
  const url = devUrl(appDir);
  const stop = "close the window or press Ctrl+C to stop";
  ui.header(
    "pylair dev",
    [
      ["mode", wrap ? `wrap (${cfg.wrap!.framework}), the server restarts when a .py file changes` : "bridge, Python restarts when a .py file changes"],
      [wrap ? "server" : "app", wrap ? "started by the app on a free local port" : (url ?? "the window loads the app bundled by tauri dev")],
      ["watching", cfg.source],
      ["stop", stop],
    ],
    [
      wrap
        ? "pylair: watching python source; starting tauri dev (wrap mode)"
        : "pylair: watching python source; starting tauri dev",
      `pylair: ${stop}`,
    ],
  );
  // A missing src-tauri/target means this is the first `tauri dev` on
  // the machine: it compiles the whole Rust/Tauri tree, so say so now —
  // a silent multi-minute build reads as a hang.
  if (!existsSync(join(appDir, "src-tauri", "target")))
    ui.info(
      "pylair: first build compiles the Rust side - it can take several minutes; later runs reuse the cache and start in seconds",
    );
}

/** Wrap mode: no codegen and no @expose surface. The plugin reads the
 * runtime file named by PYLAIR_WRAP_CONFIG, starts the launcher with the
 * project venv, and shows the server in the window. A change to a .py file
 * restarts the server (new port, the window follows). */
async function devWrap(
  appDir: string,
  cfg: PylairConfig,
  inspectEnv: Record<string, string>,
): Promise<number> {
  for (const w of wrapWarnings(cfg.wrap!)) ui.warn(w, { plain: `pylair: warning: ${w}` });
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
  devHeader(appDir, cfg);
  const child = spawn(process.execPath, [tauriCliEntry(appDir), "dev"], {
    cwd: appDir,
    stdio: "inherit",
    env: {
      ...process.env,
      PYLAIR_WRAP_CONFIG: runtime,
      PYLAIR_PYTHON: venvPython(appDir),
      PYLAIR_RESTART_TRIGGER: trigger,
      PYLAIR_DEV: "1",
      ...inspectEnv,
    },
  });
  return new Promise((resolve) => {
    child.on("exit", (code) => resolve(code ?? 1));
    for (const sig of ["SIGINT", "SIGTERM"] as const)
      process.on(sig, () => child.kill(sig));
  });
}

export async function dev(opts: {
  appDir: string;
  inspect?: boolean;
  inspectPayloads?: boolean;
  skipPreflight?: boolean;
}): Promise<number> {
  const { appDir } = opts;
  // Inspector (docs/inspector.md): dev-only, compiled into debug builds
  // only — the env flags reach the plugin through the dev child. The
  // window opens once the app is up; payloads are a second opt-in.
  const inspectEnv: Record<string, string> = {};
  if (opts.inspect) {
    inspectEnv.PYLAIR_INSPECT = "1";
    ui.info("pylair: inspector window will open (PYLAIR_INSPECT=1)");
  }
  if (opts.inspectPayloads) {
    inspectEnv.PYLAIR_INSPECT = "1";
    inspectEnv.PYLAIR_INSPECT_PAYLOADS = "1";
    if (!opts.inspect)
      ui.info("pylair: inspector window will open (payload previews on)");
  }
  const cfg = loadConfig(appDir);

  // Preflight BEFORE the build: a missing toolchain otherwise fails
  // minutes into `tauri dev`'s Rust compile. `pylair doctor` diagnoses
  // the same checks; --skip-preflight bypasses the gate.
  if (!opts.skipPreflight) {
    const missing = missingTools(preflightChecks());
    if (missing.length) {
      ui.error(
        `pylair dev: missing prerequisites (${missing.length}) - install them, or pass --skip-preflight:`,
      );
      for (const l of formatMissing(missing)) ui.error(`  ${l}`);
      return 1;
    }
  }

  // Sync so the venv exists before codegen/dev need the interpreter.
  try {
    await ui.step("uv sync", (st) => runTool(ui, "uv", ["sync"], appDir, { step: st }));
  } catch (e) {
    const m = (e as Error).message;
    throw new Error(
      m.includes("failed to start")
        ? `could not run uv (${m.split("failed to start: ")[1] ?? m}); is uv on PATH?`
        : "uv sync failed; is pyproject.toml valid?",
    );
  }

  if (cfg.mode === "wrap") return devWrap(appDir, cfg, inspectEnv);

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
    ...inspectEnv,
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

  devHeader(appDir, cfg);
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
