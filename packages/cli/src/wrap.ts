/** `pylair wrap` (alias `pylair init --mode wrap`): turn an existing Python
 * web app into a Pylair wrap-mode app, in place.
 *
 *   pylair wrap [entry.py] [--framework command|gradio|streamlit]
 *               [--cmd <argv...>] [--source <dir>] [--dir <path>]
 *
 * Dependencies must come from a manifest (docs/wrap-mode-brief.md, decision
 * 3): `pyproject.toml` (ideally with `uv.lock`), or `requirements.txt` as
 * the warned fallback. A bare `app.py` gets an error that offers `uv init`;
 * Pylair does not infer dependencies from imports.
 *
 * What it writes: `[tool.pylair]` / `[tool.pylair.wrap]` in pyproject.toml,
 * and the wrap template's `src-tauri/`, `ui/` and `package.json`, never
 * overwriting a file that exists.
 */

import { spawnSync } from "node:child_process";
import { appendFileSync, existsSync, readFileSync, writeFileSync } from "node:fs";
import { basename, join } from "node:path";
import { parse as parseToml } from "smol-toml";
import { cliVersion } from "./version.js";
import { parseWrapConfig, type WrapConfig, type WrapFramework } from "./wrapconfig.js";

export interface InitWrapOptions {
  appDir: string;
  /** Entry file for gradio / streamlit (relative to `source`). */
  entry?: string;
  framework?: WrapFramework;
  /** Server argv for `command`. */
  cmd?: string[];
  /** Directory holding the app (relative to appDir). Default: where `entry`
   * lives, else "app". */
  source?: string;
  /** Point the scaffold's dependencies at a Pylair checkout. */
  local?: string;
  /** Ask a yes/no question (the `uv init` offer). Default: prompt on a TTY,
   * answer no otherwise. */
  ask?: (question: string) => Promise<boolean>;
  log?: (msg: string) => void;
}

export interface InitWrapResult {
  config: WrapConfig;
  source: string;
  wrote: string[];
  kept: string[];
}

export const NO_MANIFEST_HELP = (dir: string): string =>
  `no dependency manifest in ${dir}.\n` +
  `Pylair bundles your dependencies into the installer, so they must come from a manifest. ` +
  `It does not guess them from your imports.\n` +
  `  uv init            # creates pyproject.toml\n` +
  `  uv add <packages>  # records your dependencies and creates uv.lock\n` +
  `then run \`pylair wrap\` again. (A requirements.txt also works, but pyproject.toml with uv.lock is preferred.)`;

function mentions(text: string, pkg: string): boolean {
  return new RegExp(`(^|[^A-Za-z0-9_-])${pkg}([^A-Za-z0-9_-]|$)`, "im").test(text);
}

/** Which framework the manifest says the app uses, if it says. */
export function inferFramework(manifestText: string): WrapFramework | undefined {
  const g = mentions(manifestText, "gradio");
  const s = mentions(manifestText, "streamlit");
  if (g && !s) return "gradio";
  if (s && !g) return "streamlit";
  return undefined;
}

/** The requirement lines of a requirements.txt as pyproject dependencies,
 * or undefined when it uses anything pip-specific (-r, -e, --hash, urls with
 * options) that does not translate one to one. */
export function requirementsToDependencies(text: string): string[] | undefined {
  const deps: string[] = [];
  for (const raw of text.split(/\r?\n/)) {
    const line = raw.replace(/\s+#.*$/, "").trim();
    if (!line || line.startsWith("#")) continue;
    if (line.startsWith("-") || line.includes("--hash")) return undefined;
    deps.push(line);
  }
  return deps;
}

function tomlString(s: string): string {
  return JSON.stringify(s); // a JSON string is a valid TOML basic string
}

export function wrapToml(w: WrapConfig, source: string): string {
  const lines = [`[tool.pylair]`, `mode = "wrap"`, `source = ${tomlString(source)}`, ``, `[tool.pylair.wrap]`];
  lines.push(`framework = ${tomlString(w.framework)}`);
  if (w.entry !== undefined) lines.push(`entry = ${tomlString(w.entry)}`);
  if (w.cmd) lines.push(`cmd = [${w.cmd.map(tomlString).join(", ")}]`);
  if (w.readyPath !== undefined) lines.push(`ready-path = ${tomlString(w.readyPath)}`);
  lines.push(`ready-timeout = ${w.readyTimeout}`, `downloads = ${tomlString(w.downloads)}`);
  return lines.join("\n") + "\n";
}

async function promptYesNo(question: string): Promise<boolean> {
  if (!process.stdin.isTTY) return false;
  const { createInterface } = await import("node:readline/promises");
  const rl = createInterface({ input: process.stdin, output: process.stdout });
  try {
    return /^y(es)?$/i.test((await rl.question(`${question} [y/N] `)).trim());
  } finally {
    rl.close();
  }
}

export async function initWrap(opts: InitWrapOptions): Promise<InitWrapResult> {
  const { appDir } = opts;
  const log = opts.log ?? ((m: string) => console.log(m));
  const ask = opts.ask ?? promptYesNo;
  const pyproject = join(appDir, "pyproject.toml");
  const requirements = join(appDir, "requirements.txt");

  // A project that already is a wrap app only needs its missing files.
  let existing: Record<string, unknown> | undefined;
  if (existsSync(pyproject)) {
    try {
      existing = (parseToml(readFileSync(pyproject, "utf-8")) as { tool?: { pylair?: Record<string, unknown> } }).tool?.pylair;
    } catch (e) {
      throw new Error(`could not parse ${pyproject}: ${(e as Error).message}`);
    }
  }
  if (existing && existing.mode === "bridge")
    throw new Error(`${pyproject} is a bridge-mode app ([tool.pylair] mode = "bridge"); not converting it`);

  // 1. A manifest, or the helpful error. A bare app.py is the common case.
  if (!existsSync(pyproject) && !existsSync(requirements)) {
    log(`pylair wrap: ${NO_MANIFEST_HELP(appDir)}`);
    if (await ask("Run `uv init --bare` here now?")) {
      const r = spawnSync("uv", ["init", "--bare"], { cwd: appDir, stdio: "inherit" });
      if (r.error || r.status !== 0)
        throw new Error("`uv init` failed or uv is not installed (https://docs.astral.sh/uv/)");
      log("pylair wrap: created pyproject.toml. Add your dependencies with `uv add <packages>`, then run `pylair wrap` again.");
    }
    throw new Error("a dependency manifest is required before wrapping an app");
  }

  const manifestText =
    (existsSync(pyproject) ? readFileSync(pyproject, "utf-8") : "") +
    "\n" +
    (existsSync(requirements) ? readFileSync(requirements, "utf-8") : "");

  // 2. The config: kept if the project already has one, else derived.
  let config: WrapConfig;
  let source: string;
  let pendingToml: string | undefined;
  if (existing?.mode === "wrap") {
    config = parseWrapConfig((existing as { wrap?: unknown }).wrap, pyproject);
    source = String(existing.source ?? "app");
    log("pylair wrap: [tool.pylair] mode = \"wrap\" already present; keeping it");
  } else {
    if (existing && Object.keys(existing).length)
      throw new Error(`${pyproject} already has a [tool.pylair] table without mode = "wrap"; edit it by hand`);
    const framework = opts.framework ?? (opts.cmd ? "command" : inferFramework(manifestText));
    if (!framework)
      throw new Error(
        `cannot tell which framework to run: pass --framework command|gradio|streamlit ` +
          `(and --cmd <argv...> for command, or an entry file for gradio/streamlit)`,
      );
    const entryPath = opts.entry?.replace(/\\/g, "/");
    source = opts.source ?? (entryPath && entryPath.includes("/") ? entryPath.slice(0, entryPath.lastIndexOf("/")) : entryPath ? "." : "app");
    const entryInSource =
      entryPath && source !== "." && entryPath.startsWith(source + "/") ? entryPath.slice(source.length + 1) : entryPath;
    config = parseWrapConfig(
      {
        framework,
        ...(framework === "command" ? { cmd: opts.cmd } : { entry: entryInSource }),
      },
      pyproject,
    );
    if (entryPath && !existsSync(join(appDir, source, entryInSource ?? entryPath)))
      throw new Error(`entry ${entryPath} not found under ${join(appDir, source)}`);

    pendingToml = wrapToml(config, source);
  }

  // 3. The window shell, from the scaffolder that owns the wrap template
  // (so `pylair wrap` and `create-pylair --mode wrap` can never drift).
  // It goes first: if it refuses (wrap mode not published yet), nothing in
  // the project has been touched.
  const name = readName(pyproject) ?? (basename(appDir).toLowerCase().replace(/[^a-z0-9-]/g, "-") || "app");
  const shell = runScaffolder(appDir, name, opts.local);

  // 4. Only now write the config.
  if (pendingToml !== undefined) {
    if (!existsSync(pyproject)) {
      // `pylair dev` runs `uv sync`, which reads pyproject dependencies, so
      // carry the requirements over when they translate cleanly.
      const deps = requirementsToDependencies(readFileSync(requirements, "utf-8"));
      writeFileSync(
        pyproject,
        `[project]\nname = ${tomlString(name)}\nversion = "0.1.0"\nrequires-python = ">=3.12"\n` +
          `dependencies = [${(deps ?? []).map(tomlString).join(", ")}]\n\n`,
      );
      log(
        "warning: no pyproject.toml, only requirements.txt: created a minimal pyproject.toml for the Pylair config. " +
          (deps
            ? "Your requirements were copied into its dependencies so `pylair dev` can `uv sync` them. "
            : "requirements.txt uses pip options that do not translate (-r, -e, --hash): add your dependencies to pyproject.toml with `uv add` so `pylair dev` sees them. ") +
          "Without uv.lock the bundled environment can drift from `pylair dev`'s; prefer `uv add` + uv.lock.",
      );
    } else {
      const cur = readFileSync(pyproject, "utf-8");
      if (cur.length && !cur.endsWith("\n")) appendFileSync(pyproject, "\n");
      appendFileSync(pyproject, "\n");
    }
    appendFileSync(pyproject, pendingToml);
  }

  // The supervisor starts `python -m pylair.wrap`, so the app's own
  // environment must contain pylair. `pylair doctor` and `pylair build`
  // check it; say so now rather than at the first run.
  if (!mentions(manifestText, "pylair"))
    log(
      "pylair wrap: add Pylair to your dependencies, it runs your server: " +
        (opts.local ? `uv add "pylair @ file://<repo>/python/pylair"` : "uv add pylair"),
    );
  if (shell.kept.includes("package.json"))
    log(
      "pylair wrap: package.json already exists; add these to devDependencies yourself: " +
        `"@tauri-apps/cli": "^2", "@pylair/cli": "${cliVersion()}"`,
    );
  log(
    `pylair wrap: ${name} is a wrap-mode app (framework ${config.framework}, source ${source}).\n` +
      `next: npm install && uv sync && npx pylair dev`,
  );
  return { config, source, wrote: shell.written, kept: shell.kept };
}

/** Run `create-pylair --mode wrap --into <dir>`. From a Pylair checkout
 * (`--local`) that is the checkout's own build; otherwise the scaffolder
 * published at this CLI's exact version, so templates and pins always match.
 * `PYLAIR_CREATE_PYLAIR` overrides the script (tests). */
export function runScaffolder(
  appDir: string,
  name: string,
  local?: string,
): { written: string[]; kept: string[] } {
  const args = ["--mode", "wrap", "--into", appDir, "--name", name];
  if (local) args.push("--local", local);
  const override = process.env.PYLAIR_CREATE_PYLAIR;
  const script = override ?? (local ? join(local, "packages", "create-pylair", "dist", "index.js") : undefined);
  const r = script
    ? spawnSync(process.execPath, [script, ...args], { encoding: "utf-8" })
    : spawnSync(
        process.platform === "win32" ? "npx.cmd" : "npx",
        ["--yes", `create-pylair@${cliVersion()}`, ...args],
        { encoding: "utf-8", shell: process.platform === "win32" },
      );
  if (r.error) throw new Error(`could not run the scaffolder: ${r.error.message}`);
  if (r.status !== 0) throw new Error((r.stderr || r.stdout || "the scaffolder failed").trim());
  const last = r.stdout.trim().split(/\r?\n/).pop() ?? "";
  try {
    return JSON.parse(last) as { written: string[]; kept: string[] };
  } catch {
    throw new Error(`unexpected scaffolder output: ${r.stdout.slice(0, 200)}`);
  }
}

function readName(pyproject: string): string | undefined {
  try {
    const doc = parseToml(readFileSync(pyproject, "utf-8")) as { project?: { name?: string } };
    const n = doc.project?.name;
    return n && /^[a-z][a-z0-9-]*$/.test(n) ? n : undefined;
  } catch {
    return undefined;
  }
}
