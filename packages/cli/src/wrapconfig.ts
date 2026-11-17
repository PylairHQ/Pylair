/** `[tool.pylair.wrap]` parsing and validation (wrap mode, W2).
 *
 * Wrap mode shows an existing Python web UI in the window. The config here
 * is the single source of truth: `pylair dev` and `pylair build` turn it
 * into the launcher command line (`launcherArgs`) and the runtime file the
 * plugin reads (`runtimeConfig`).
 */

export type Mode = "bridge" | "wrap";
export const MODES: readonly Mode[] = ["bridge", "wrap"];

export type WrapFramework = "command" | "gradio" | "streamlit";
export const WRAP_FRAMEWORKS: readonly WrapFramework[] = ["command", "gradio", "streamlit"];

/** Where a download from the wrapped page goes: a native Save As dialog
 * (default) or straight into the user's Downloads folder. */
export type DownloadMode = "ask" | "downloads";

export interface WrapConfig {
  framework: WrapFramework;
  /** gradio / streamlit: the app's entry file, relative to `source`. */
  entry?: string;
  /** command: argv of the server; `{port}` is replaced by the launcher. */
  cmd?: string[];
  /** Path probed for readiness and health. Unset means the launcher's own
   * per-framework default ("/" for command, /config for gradio,
   * /_stcore/health for streamlit), which is picked so a slow first page is
   * never a probe miss; setting it here overrides that. */
  readyPath?: string;
  /** Seconds the supervisor waits for the server to answer (default 120). */
  readyTimeout: number;
  downloads: DownloadMode;
}

const WRAP_KEYS = ["framework", "entry", "cmd", "ready-path", "ready-timeout", "downloads"];

function err(pyproject: string, msg: string): Error {
  return new Error(`[tool.pylair.wrap] ${msg} (${pyproject})`);
}

/** Validate a raw `[tool.pylair.wrap]` table. Every message names the key
 * and what is allowed: this is the first thing a new wrap user reads. */
export function parseWrapConfig(raw: unknown, pyproject = "pyproject.toml"): WrapConfig {
  if (raw === undefined || raw === null)
    throw new Error(
      `mode = "wrap" needs a [tool.pylair.wrap] table in ${pyproject}, e.g.\n` +
        `  [tool.pylair.wrap]\n` +
        `  framework = "command"\n` +
        `  cmd = ["python", "-m", "myapp", "--port", "{port}"]`,
    );
  if (typeof raw !== "object" || Array.isArray(raw)) throw err(pyproject, "must be a table");
  const t = raw as Record<string, unknown>;

  const unknown = Object.keys(t).filter((k) => !WRAP_KEYS.includes(k));
  if (unknown.length)
    throw err(
      pyproject,
      `unknown key${unknown.length > 1 ? "s" : ""} ${unknown.map((k) => `"${k}"`).join(", ")}; ` +
        `allowed: ${WRAP_KEYS.join(", ")}`,
    );

  const fw = t.framework;
  if (fw === undefined)
    throw err(pyproject, `framework is required (one of ${WRAP_FRAMEWORKS.join(", ")})`);
  if (typeof fw !== "string" || !(WRAP_FRAMEWORKS as readonly string[]).includes(fw))
    throw err(
      pyproject,
      `framework must be one of ${WRAP_FRAMEWORKS.map((f) => `"${f}"`).join(", ")}, got ${JSON.stringify(fw)}`,
    );
  const framework = fw as WrapFramework;

  let entry: string | undefined;
  if (t.entry !== undefined) {
    if (typeof t.entry !== "string" || !t.entry.trim())
      throw err(pyproject, "entry must be a non-empty string");
    entry = t.entry;
    const norm = entry.replace(/\\/g, "/");
    if (norm.startsWith("/") || /^[A-Za-z]:/.test(norm) || norm.split("/").includes(".."))
      throw err(
        pyproject,
        `entry must be a path inside the source dir (no absolute path, no ".."): ${JSON.stringify(entry)}`,
      );
  }

  let cmd: string[] | undefined;
  if (t.cmd !== undefined) {
    if (!Array.isArray(t.cmd) || t.cmd.length === 0 || t.cmd.some((a) => typeof a !== "string"))
      throw err(pyproject, "cmd must be a non-empty array of strings (the server's argv)");
    cmd = t.cmd as string[];
    if (!cmd[0].trim()) throw err(pyproject, "cmd[0] (the program) must not be empty");
  }

  if (framework === "command") {
    if (!cmd)
      throw err(pyproject, `framework "command" needs cmd = ["python", "-m", "myapp", "--port", "{port}"]`);
    if (entry !== undefined) throw err(pyproject, `entry is not used by framework "command" (use cmd)`);
  } else {
    if (entry === undefined)
      throw err(pyproject, `framework "${framework}" needs entry = "app.py" (the file to run)`);
    if (cmd !== undefined)
      throw err(pyproject, `cmd is only for framework "command"; "${framework}" runs entry`);
  }

  const readyPath = t["ready-path"];
  if (
    readyPath !== undefined &&
    (typeof readyPath !== "string" || !readyPath.startsWith("/") || /\s/.test(readyPath))
  )
    throw err(
      pyproject,
      `ready-path must be a path starting with "/" and without spaces, got ${JSON.stringify(readyPath)}`,
    );

  const readyTimeout = t["ready-timeout"] ?? 120;
  if (
    typeof readyTimeout !== "number" ||
    !Number.isInteger(readyTimeout) ||
    readyTimeout < 1 ||
    readyTimeout > 3600
  )
    throw err(
      pyproject,
      `ready-timeout must be a whole number of seconds from 1 to 3600, got ${JSON.stringify(readyTimeout)}`,
    );

  const downloads = t.downloads ?? "ask";
  if (downloads !== "ask" && downloads !== "downloads")
    throw err(
      pyproject,
      `downloads must be "ask" (Save As dialog) or "downloads" (straight to Downloads), got ${JSON.stringify(downloads)}`,
    );

  return { framework, entry, cmd, readyPath, readyTimeout, downloads };
}

/** Findings about a valid wrap config that do not stop a build (printed by
 * `pylair doctor`, `dev` and `build`). */
export function wrapWarnings(w: WrapConfig): string[] {
  const out: string[] = [];
  if (w.framework === "command" && w.cmd) {
    if (!w.cmd.some((a) => a.includes("{port}")))
      out.push(
        `cmd has no "{port}": the server must read $PYLAIR_PORT, otherwise it listens ` +
          `somewhere the window cannot reach`,
      );
    const bad = w.cmd.find((a) => a === "0.0.0.0" || a === "::" || a.endsWith("=0.0.0.0"));
    if (bad)
      out.push(
        `cmd binds ${bad}: the server would be reachable from the network. ` +
          `Bind 127.0.0.1 (docs/security.md section 9)`,
      );
  }
  return out;
}

/** The launcher command line for this config, minus the interpreter:
 * `python -m pylair.wrap --framework F [--entry E] [--cmd ...] --ready-path P`.
 * Launcher options come before `--cmd`, so the command's own arguments can
 * never be mistaken for launcher options (docs/protocol-v1.md section 4.1). */
export function launcherArgs(w: WrapConfig): string[] {
  const a = ["-m", "pylair.wrap", "--framework", w.framework];
  if (w.entry !== undefined) a.push("--entry", w.entry);
  if (w.readyPath !== undefined) a.push("--ready-path", w.readyPath);
  if (w.cmd) a.push("--cmd", ...w.cmd);
  return a;
}

/** What the plugin reads at startup: `<resources>/pylair-wrap.json` in a
 * packaged app, the file `PYLAIR_WRAP_CONFIG` names under `pylair dev`.
 * `source` is the staged source dir, which the server runs in. */
export interface WrapRuntime {
  version: 1;
  source: string;
  launcher: string[];
  readyTimeoutSecs: number;
  downloads: DownloadMode;
}

export function runtimeConfig(w: WrapConfig, source: string): WrapRuntime {
  return {
    version: 1,
    source,
    launcher: launcherArgs(w),
    readyTimeoutSecs: w.readyTimeout,
    downloads: w.downloads,
  };
}
