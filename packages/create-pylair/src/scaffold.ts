/** Scaffolding core, shared by the `create-pylair` binary and `pylair wrap`
 * (which scaffolds the wrap template into an existing project).
 *
 * Importing this module has no side effects; `index.ts` is the CLI.
 */

import { cpSync, existsSync, mkdirSync, readFileSync, readdirSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

export const TEMPLATES = resolve(fileURLToPath(import.meta.url), "../../templates");
const BINARY = new Set([".png", ".ico", ".icns"]);

/** The registry versions a scaffolded app pins. Bump together with the
 * manifests and the template `Cargo.lock` (AGENTS.md, "Scaffold pins"). */
export const PINS = {
  py: "pylair==0.1.0rc12",
  js: "0.1.0-rc.26",
  cli: "0.1.0-rc.26",
  rs: `tauri-plugin-pylair = "=0.1.0-rc.20"`,
} as const;

/**
 * Wrap mode needs the crate (`Builder::wrap`) and the `pylair.wrap`
 * launcher. The rc.21 train published them (crate `0.1.0-rc.16`,
 * `pylair==0.1.0rc11`), so this is true and registry-mode wrap
 * scaffolds work without `--local`. The flag stays for future
 * template features that outrun the published versions.
 */
export const WRAP_IN_PUBLISHED_VERSIONS = true;

export function wrapUnavailable(local: string | undefined): string | undefined {
  if (local || WRAP_IN_PUBLISHED_VERSIONS) return undefined;
  return (
    "wrap mode is not in the published Pylair packages yet. " +
    "Scaffold against a checkout instead: --local /path/to/pylair-repo"
  );
}

export const WRAP_FRAMEWORKS = ["command", "gradio", "streamlit"] as const;
export type WrapFramework = (typeof WRAP_FRAMEWORKS)[number];

/** What a wrap-mode app needs per framework: the extra dependencies
 * (pyproject and requirements.txt), the `[tool.pylair.wrap]` config line,
 * and the sample app file for non-command frameworks (templates/wrap/
 * variants/<sample> is written to app/app.py instead of app/main.py). The
 * pins match scripts/wrap-acceptance/prepare.mjs, which turns the same
 * scaffold into the acceptance app. */
export interface WrapApp {
  deps: string[];
  reqs: string[];
  run: string;
  sample?: string;
}
export const WRAP_APPS: Record<WrapFramework, WrapApp> = {
  command: {
    deps: ['"fastapi>=0.115"', '"uvicorn>=0.30"'],
    reqs: ["fastapi>=0.115", "uvicorn>=0.30"],
    run: 'cmd = ["python", "-m", "uvicorn", "main:app", "--host", "127.0.0.1", "--port", "{port}"]',
  },
  gradio: {
    deps: ['"gradio>=5,<6"'],
    reqs: ["gradio>=5,<6"],
    run: 'entry = "app.py"',
    sample: "gradio.py",
  },
  streamlit: {
    deps: ['"streamlit>=1,<2"'],
    reqs: ["streamlit>=1,<2"],
    run: 'entry = "app.py"',
    sample: "streamlit.py",
  },
};

export function substitutions(
  name: string,
  identifier: string,
  local?: string,
  framework: WrapFramework = "command",
): Record<string, string> {
  const snake = name.replace(/-/g, "_").replace(/\W/g, "_");
  const subs: Record<string, string> = {
    __APP_NAME__: name,
    __APP_ID__: identifier,
    __APP_SNAKE__: snake,
  };
  if (local) {
    const root = resolve(local);
    const unixPath = (p: string) => p.replace(/\\/g, "/");
    const py = unixPath(join(root, "python/pylair"));
    subs.__PYLAIR_PY__ = `pylair @ file://${py.startsWith("/") ? py : `/${py}`}`;
    subs.__PYLAIR_JS_PKG__ = "@pylair/client";
    subs.__PYLAIR_JS__ = `file:${unixPath(join(root, "packages/client"))}`;
    subs.__PYLAIR_RS__ = `tauri-plugin-pylair = { path = "${unixPath(join(root, "crates/tauri-plugin-pylair"))}" }`;
    subs.__PYLAIR_CLI__ = `file:${unixPath(join(root, "packages/cli"))}`;
  } else {
    subs.__PYLAIR_PY__ = PINS.py;
    subs.__PYLAIR_JS_PKG__ = "@pylair/client";
    subs.__PYLAIR_JS__ = PINS.js;
    subs.__PYLAIR_CLI__ = PINS.cli;
    subs.__PYLAIR_RS__ = PINS.rs;
  }
  const app = WRAP_APPS[framework];
  subs.__WRAP_FRAMEWORK__ = framework;
  subs.__WRAP_RUN__ = app.run;
  subs.__WRAP_DEPS__ = [`"${subs.__PYLAIR_PY__}"`, ...app.deps].join(", ");
  subs.__WRAP_REQS__ = [subs.__PYLAIR_PY__, ...app.reqs].join("\n");
  return subs;
}

function fill(text: string, subs: Record<string, string>): string {
  let out = text;
  for (const [k, v] of Object.entries(subs)) out = out.split(k).join(v);
  return out;
}

export interface CopyOptions {
  /** Never overwrite a file that already exists (initialising a project
   * that has its own files). */
  keepExisting?: boolean;
  /** Leave out any template entry whose `/`-separated relative path (in
   * the output, `.tmpl` and `_gitignore` already renamed) this accepts. */
  skip?: (rel: string) => boolean;
}

export interface CopyResult {
  /** Files written, relative to the destination, `/`-separated. */
  written: string[];
  /** Files left alone because they already existed (`keepExisting`). */
  kept: string[];
}

/** Copy a template tree, substituting placeholders. */
export function copyTree(
  src: string,
  dst: string,
  subs: Record<string, string>,
  opts: CopyOptions = {},
  rel = "",
): CopyResult {
  const out: CopyResult = { written: [], kept: [] };
  mkdirSync(dst, { recursive: true });
  for (const e of readdirSync(src, { withFileTypes: true })) {
    const s = join(src, e.name);
    const isTmpl = e.name.endsWith(".tmpl");
    // npm strips .gitignore from published tarballs: ship it as
    // _gitignore (create-vite convention) and rename on scaffold.
    const name = e.name === "_gitignore" ? ".gitignore" : e.name;
    const outName = isTmpl ? name.slice(0, -5) : name;
    const d = join(dst, outName);
    const r = rel ? `${rel}/${outName}` : outName;
    if (opts.skip?.(r)) continue;
    if (e.isDirectory()) {
      const sub = copyTree(s, d, subs, opts, r);
      out.written.push(...sub.written);
      out.kept.push(...sub.kept);
    } else if (opts.keepExisting && existsSync(d)) {
      out.kept.push(r);
    } else {
      if (isTmpl || !BINARY.has(e.name.slice(e.name.lastIndexOf(".")))) {
        writeFileSync(d, fill(readFileSync(s, "utf-8"), subs));
      } else {
        cpSync(s, d);
      }
      out.written.push(r);
    }
  }
  return out;
}
