/** `pylair dev`: supervised dev mode:
 *   uv sync, codegen, watch the python source dir; on change,
 *   re-run codegen and touch the restart trigger (the supervisor
 *   respawns the runtime within ~250ms); run `tauri dev`.
 */

import { spawn } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { platform } from "node:os";
import { join } from "node:path";
import { runTool } from "./build/util.js";
import {
  BUILTIN_IGNORES,
  pollStatusFile,
  startDevWatch,
  writeManualTrigger,
} from "./devwatch.js";
import { importRoot, loadConfig, tauriCliEntry, type PylairConfig } from "./config.js";
import { prepareDevBinaries } from "./build/binaries.js";
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

/** The dev-only codegen status file the inspector reads (docs/inspector.md):
 * written after every codegen attempt, so a broken stub regeneration is a
 * banner and a timeline entry instead of scrollback. The app finds it
 * through PYLAIR_CODEGEN_STATUS - dev-only by construction: the variable
 * only ever reaches the dev child. */
export function codegenStatusPath(appDir: string): string {
  return join(appDir, ".pylair", "codegen-status.json");
}

/** Write the inspector-visible codegen status. Exported for tests. */
export function writeCodegenStatus(appDir: string, status: Record<string, unknown>): void {
  try {
    mkdirSync(join(appDir, ".pylair"), { recursive: true });
    writeFileSync(
      codegenStatusPath(appDir),
      JSON.stringify({ at: Date.now(), ...status }),
    );
  } catch {
    // The status file is a nicety, never a reason to break codegen itself.
  }
}

function runCodegen(appDir: string, cfgModule: string, out: string, source: string): boolean {
  const python = venvPython(appDir);
  const env = codegenEnv(appDir, source);
  try {
    const manifest = loadManifest({ python, module: cfgModule, env });
    const ts = emit(manifest, cfgModule);
    const prev = existsSync(out) ? readFileSync(out, "utf-8") : null;
    writeCodegenStatus(appDir, { ok: true, functions: manifest.functions?.length ?? 0 });
    if (prev !== ts) {
      writeFileSync(out, ts);
      ui.info(`pylair: codegen wrote ${out} (${manifest.functions?.length ?? 0} functions)`);
    }
    return true;
  } catch (e) {
    const message = (e as Error).message;
    writeCodegenStatus(appDir, { ok: false, error: message });
    ui.warn(`pylair: codegen failed (${message}); keeping last stubs`, { bare: true });
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

/** Reload visibility (DX item 10): one line per event, never spam during
 * a debounce burst. The CLI writes the trigger; the supervisor (debug
 * builds only, PYLAIR_RUNTIME_STATUS) writes runtime-status.json at every
 * state transition; this bridge prints the pair. */
export function runtimeStatusPath(appDir: string): string {
  return join(appDir, ".pylair", "runtime-status.json");
}

interface ReloadTracker {
  since: number | null;
  seenReadyFor: number;
}

/** Wire the trigger callback and the status-file poll into the terminal
 * lines. Shared by bridge and wrap - wrap's "ready" is the port answering,
 * which is the same supervisor state. */
export function reloadVisibility(appDir: string): {
  onTrigger: (label: string) => void;
  close: () => void;
} {
  const tracker: ReloadTracker = { since: null, seenReadyFor: -1 };
  const poller = pollStatusFile(runtimeStatusPath(appDir), (st) => {
    if (tracker.since === null) return; // initial startup is not a reload
    const gen = st.generation ?? -1;
    const ts = st.ts ?? 0;
    if (st.state === "ready" && ts >= tracker.since && gen !== tracker.seenReadyFor) {
      tracker.seenReadyFor = gen;
      const ms = ts - tracker.since;
      tracker.since = null;
      ui.info(`reload: ready in ${ms}ms (generation ${gen})`);
    } else if (st.state === "exited" && ts >= tracker.since) {
      ui.warn(
        `reload: Python exited during startup (code ${st.exit_code ?? "?"}), see the error above; fix and save to retry`,
        { bare: true },
      );
    } else if (st.state === "failed" && ts >= tracker.since) {
      ui.warn(`reload: Python failed: ${st.detail ?? "see the error above"}`, { bare: true });
    } else if (st.state === "breaker_open" && ts >= tracker.since) {
      ui.warn(
        "reload: crash-loop breaker is open - fix and save, or press r / run `pylair restart`",
        { bare: true },
      );
    }
  });
  return {
    onTrigger: (label) => {
      tracker.since = Date.now();
      ui.info(`reload: ${label}, restarting Python`);
    },
    close: () => poller.close(),
  };
}

/** External restart visibility: the supervisor restarts on ANY trigger
 * content, whoever wrote it - `pylair restart` from a second terminal
 * must narrate in this one. Poll the trigger file; content the dev
 * process did not write itself is an external trigger. */
export function watchExternalTriggers(
  trigger: string,
  ownWrites: Set<string>,
  onExternal: (label: string) => void,
): { close: () => void } {
  let last = "";
  try {
    last = readFileSync(trigger, "utf-8");
  } catch {
    // created momentarily
  }
  const timer = setInterval(() => {
    let content: string;
    try {
      content = readFileSync(trigger, "utf-8");
    } catch {
      return;
    }
    if (content === last) return;
    last = content;
    if (ownWrites.has(content)) return; // we narrated this one already
    const token = content.trim().split(/\s+/)[1];
    if (!token) return;
    onExternal(token === "manual" ? "manual restart requested" : `${token} changed`);
  }, 200);
  return { close: () => clearInterval(timer) };
}

/** Line-buffered stdin listener for the `r` key: manual restart without
 * editing a file (the main use: the breaker is open and the bug is fixed).
 * Never raw mode - tauri dev inherits stdin. Skipped entirely when stdin
 * is not a TTY (CI, pipes). */
function armRestartKey(trigger: string, ownWrites: Set<string>, onManual: () => void): void {
  if (!process.stdin.isTTY) return;
  import("node:readline").then((rl) => {
    const iface = rl.createInterface({ input: process.stdin, terminal: false });
    iface.on("line", (line) => {
      if (line.trim() !== "r") return; // line-buffered: every other key is ignored
      ownWrites.add(writeManualTrigger(trigger));
      onManual();
    });
  });
}

/** The pid file `pylair restart` uses to prove a dev session is live. */
function writeDevPidFile(appDir: string): void {
  try {
    writeFileSync(join(appDir, ".pylair", "dev.pid"), String(process.pid));
  } catch {
    // a nicety, like the status files
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
      [
        "watching",
        `${cfg.source} (${cfg.watch.include.join(", ")}); ignoring ${BUILTIN_IGNORES.length} built-in patterns` +
          (cfg.watch.ignore.length ? ` + ${cfg.watch.ignore.length} custom` : ""),
      ],
      ["restart", process.stdin.isTTY ? "press r + Enter, or `pylair restart` from another terminal" : "`pylair restart` from another terminal"],
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
  // the machine: it compiles the whole Rust/Tauri tree, so say so now -
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

  const vis = reloadVisibility(appDir);
  const ownWrites = new Set<string>();
  startDevWatch({
    srcDir,
    trigger,
    set: cfg.watch,
    onWrite: (c) => ownWrites.add(c),
    onTrigger: (file) => vis.onTrigger(`${file} changed`),
  });
  watchExternalTriggers(trigger, ownWrites, vis.onTrigger);
  armRestartKey(trigger, ownWrites, () => vis.onTrigger("manual restart requested"));
  writeDevPidFile(appDir);

  const wrapBinariesDir = await prepareDevBinaries(appDir, cfg.binaries);
  const wrapBinariesEnv: Record<string, string> = {};
  if (wrapBinariesDir) wrapBinariesEnv.PYLAIR_BINARIES_DIR = wrapBinariesDir;

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
      PYLAIR_RUNTIME_STATUS: runtimeStatusPath(appDir),
      PYLAIR_DEV: "1",
      ...wrapBinariesEnv,
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
  // only - the env flags reach the plugin through the dev child. The
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

  // Watch python source; debounce bursts (saves, formatter runs). The
  // trigger content carries the changed file after the timestamp - the
  // inspector's dev-reload timeline entry names it (docs/inspector.md).
  const vis = reloadVisibility(appDir);
  const ownWrites = new Set<string>();
  startDevWatch({
    srcDir,
    trigger,
    set: cfg.watch,
    onWrite: (c) => ownWrites.add(c),
    onTrigger: (file) => {
      vis.onTrigger(`${file} changed`);
      const ok = runCodegen(appDir, cfg.appModule, stubs, cfg.source);
      if (!ok) ui.warn("codegen: failed, see inspector", { bare: true });
    },
  });
  watchExternalTriggers(trigger, ownWrites, vis.onTrigger);
  armRestartKey(trigger, ownWrites, () => vis.onTrigger("manual restart requested"));
  writeDevPidFile(appDir);

  // External binaries (0014 sec. 3): dev resolution into .pylair/binaries
  // (declared path, else a hash-verified cached download), manifest written
  // in the same schema release reads. The dir reaches the plugin through
  // the dev child; in debug builds it resolves the map from there.
  const binariesDir = await prepareDevBinaries(appDir, cfg.binaries);
  const binariesEnv: Record<string, string> = {};
  if (binariesDir) binariesEnv.PYLAIR_BINARIES_DIR = binariesDir;

  const env = {
    ...process.env,
    PYLAIR_APP_MODULE: cfg.appModule,
    PYLAIR_PYTHON: venvPython(appDir),
    PYLAIR_PYTHONPATH: importRoot(appDir, cfg.source),
    PYLAIR_RESTART_TRIGGER: trigger,
    PYLAIR_CODEGEN_STATUS: codegenStatusPath(appDir),
    PYLAIR_RUNTIME_STATUS: runtimeStatusPath(appDir),
    PYLAIR_DEV: "1",
    ...binariesEnv,
    ...inspectEnv,
  };
  // tauri.conf.json maps resources/* paths that only exist after
  // `pylair build` stages them; `tauri dev` validates them eagerly and
  // fails the build on any missing one. Create empty dirs for dev -
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
    // Conf unreadable - `tauri dev` will surface the real error itself.
  }

  devHeader(appDir, cfg);
  // Run the app's installed tauri CLI via node - no npx/npx.cmd shell
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
