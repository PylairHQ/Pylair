#!/usr/bin/env node
/** create-pylair — scaffold a new Pylair app.
 *
 *   npx create-pylair my-app [--mode bridge|wrap] [--identifier com.example.app] [--local /path/to/pylair-repo]
 *   npx create-pylair --mode wrap --into <existing-dir> [--name my-app]   (what `pylair wrap` runs)
 *
 * --mode wrap scaffolds a wrap-mode app: an existing Python web UI shown in
 * the window, with no React app (docs/wrap-mode-brief.md).
 *
 * --local points all three pylair deps (cargo crate, pypi package, npm
 * package) at a local checkout — used by this repo's own acceptance test
 * until the packages are published.
 */

import { existsSync, readdirSync, statSync } from "node:fs";
import { join, resolve } from "node:path";
import { createInterface } from "node:readline/promises";
import { TEMPLATES, copyTree, substitutions, wrapUnavailable } from "./scaffold.js";

const USAGE =
  "usage: create-pylair <app-name> [--mode bridge|wrap] [--identifier com.x.y] [--local /repo] [--template default]";
const VALUE_FLAGS = ["--local", "--identifier", "--template", "--mode", "--into", "--name"];

function argValue(argv: string[], flag: string): string | undefined {
  const i = argv.indexOf(flag);
  return i === -1 ? undefined : argv[i + 1];
}

function fail(msg: string): never {
  console.error(`create-pylair: ${msg}`);
  process.exit(1);
}

function main(argv: string[]): number | Promise<number> {
  const positional = argv.filter(
    (a, i) => !a.startsWith("--") && !VALUE_FLAGS.includes(argv[i - 1]),
  );
  const name = positional[0];
  if (argValue(argv, "--into") !== undefined) return into(argv);
  if (name === "create-pylair") {
    console.log(USAGE);
    return 1;
  }
  if (!name) {
    // Interactive: ask for the name (create-vite convention). Piped or
    // scripted: fail loudly — silently scaffolding nothing is worse.
    if (!process.stdin.isTTY) {
      console.error(`create-pylair: app name required: ${USAGE}`);
      return 1;
    }
    return promptForName(argv);
  }
  return scaffold(name, argv);
}

async function promptForName(argv: string[]): Promise<number> {
  const rl = createInterface({ input: process.stdin, output: process.stdout });
  try {
    const name = (await rl.question("app name: ")).trim();
    if (!name) {
      console.error("create-pylair: app name required");
      return 1;
    }
    return scaffold(name, argv);
  } finally {
    rl.close();
  }
}

/** Add the wrap shell (src-tauri, ui, package.json, .gitignore) to an
 * existing project, never overwriting a file and leaving the project's own
 * app, pyproject.toml and requirements.txt alone. `pylair wrap` writes the
 * `[tool.pylair]` config first, then runs this. */
function into(argv: string[]): number {
  const dir = resolve(process.cwd(), argValue(argv, "--into") ?? ".");
  if (argValue(argv, "--mode") !== "wrap") fail("--into needs --mode wrap");
  if (!existsSync(dir) || !statSync(dir).isDirectory()) fail(`${dir} is not a directory`);
  const local = argValue(argv, "--local");
  const why = wrapUnavailable(local);
  if (why) fail(why);
  const name = argValue(argv, "--name");
  if (!name || !/^[a-z][a-z0-9-]*$/.test(name))
    fail(`--name is required and must be lowercase letters, digits, dashes (got '${name ?? ""}')`);
  const identifier = argValue(argv, "--identifier") ?? `com.example.${name.replace(/-/g, "")}`;
  const { written, kept } = copyTree(join(TEMPLATES, "wrap"), dir, substitutions(name, identifier, local), {
    keepExisting: true,
    skip: (rel) => rel === "app" || rel === "pyproject.toml" || rel === "requirements.txt",
  });
  console.log(JSON.stringify({ written, kept }));
  return 0;
}

function scaffold(name: string, argv: string[]): number {
  if (!/^[a-z][a-z0-9-]*$/.test(name))
    fail(`invalid app name '${name}' — use lowercase letters, digits, dashes`);

  const identifier = argValue(argv, "--identifier") ?? `com.example.${name.replace(/-/g, "")}`;
  const local = argValue(argv, "--local");
  const mode = argValue(argv, "--mode") ?? "bridge";
  if (mode !== "bridge" && mode !== "wrap")
    fail(`--mode must be "bridge" or "wrap", got '${mode}'`);
  const explicit = argValue(argv, "--template");
  if (explicit && mode === "wrap" && explicit !== "wrap")
    fail(`--mode wrap uses the "wrap" template, not '${explicit}'`);
  const template = explicit ?? (mode === "wrap" ? "wrap" : "default");
  if (template === "wrap") {
    const why = wrapUnavailable(local);
    if (why) fail(why);
  }
  const templateDir = join(TEMPLATES, template);
  if (!existsSync(templateDir)) fail(`no template '${template}' at ${templateDir}`);

  const dest = resolve(process.cwd(), name);
  if (existsSync(dest) && readdirSync(dest).length) fail(`${dest} exists and is not empty`);

  copyTree(templateDir, dest, substitutions(name, identifier, local));
  if (!statSync(join(dest, "src-tauri/tauri.conf.json")).isFile())
    fail("template produced no tauri.conf.json — internal error");

  console.log(
    template === "wrap"
      ? `created ${name}/ (wrap mode)

next:
  cd ${name}
  npm install         # or pnpm install
  uv sync             # python env (https://docs.astral.sh/uv/)
  npx pylair dev      # your web app in a native window, with reload
  npx pylair build    # native installer with bundled python
  npx pylair doctor   # diagnose environment any time
`
      : `created ${name}/

next:
  cd ${name}
  npm install         # or pnpm install
  uv sync             # python env (https://docs.astral.sh/uv/)
  npx pylair dev      # supervised dev: python hot reload + vite
                      # (pnpm users: pnpm pylair dev / pnpm pylair build)
  npx pylair build    # native installer with bundled python
  npx pylair doctor   # diagnose environment any time
`,
  );
  return 0;
}

process.exit(await main(process.argv.slice(2)));
