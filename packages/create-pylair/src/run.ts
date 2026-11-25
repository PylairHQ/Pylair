/** create-pylair's logic, separate from the bin so tests can drive it with
 * fake streams. `run` resolves every answer the command line did not give -
 * interactively on a TTY, never under CI - then scaffolds exactly the files
 * it always did.
 *
 *   npx create-pylair my-app [--mode bridge|wrap] [--framework command|gradio|streamlit]
 *   npx create-pylair --mode wrap --into <existing-dir> --name my-app   (what `pylair wrap` runs)
 */

import { existsSync, readFileSync, readdirSync, statSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";
import {
  TEMPLATES,
  WRAP_APPS,
  WRAP_FRAMEWORKS,
  applyUpdater,
  copyTree,
  substitutions,
  wrapUnavailable,
  type WrapFramework,
} from "./scaffold.js";
import { formatMissing, missingTools, preflightChecks, type PreflightItem } from "./preflight.js";
import { detectProcessCaps, type Caps } from "./ui/caps.js";
import { Ui, paint } from "./ui/index.js";
import { PromptCancelled, askSelect, askText, interactive, type PromptIo } from "./ui/prompts.js";
import { scaffolderVersion } from "./version.js";
import { wordmark } from "./wordmark.js";

const USAGE =
  "usage: create-pylair <app-name> [--mode bridge|wrap] [--framework command|gradio|streamlit] [--identifier com.x.y] [--local /repo] [--template default] [--with-updater]";
const VALUE_FLAGS = ["--local", "--identifier", "--template", "--mode", "--into", "--name", "--framework"];

export interface RunIo {
  input: PromptIo["input"];
  output: NodeJS.WritableStream;
  error: NodeJS.WritableStream;
  env?: NodeJS.ProcessEnv;
  cwd?: string;
  caps?: Caps;
  /** Test seam: the preflight items provider (default: live probes). */
  preflight?: () => PreflightItem[];
}

const NAME_RE = /^[a-z][a-z0-9-]*$/;
const NAME_RULE = "use lowercase letters, digits, dashes";

function argValue(argv: string[], flag: string): string | undefined {
  const i = argv.indexOf(flag);
  return i === -1 ? undefined : argv[i + 1];
}

export async function run(argv: string[], io: RunIo): Promise<number> {
  const env = io.env ?? process.env;
  const cwd = io.cwd ?? process.cwd();
  const out = (s: string) => io.output.write(`${s}\n`);
  const fail = (msg: string): number => {
    io.error.write(`create-pylair: ${msg}\n`);
    return 1;
  };

  if (argValue(argv, "--into") !== undefined) return into(argv, io, cwd);

  const caps = io.caps ?? detectProcessCaps(argv);
  if (caps.fancy) out(wordmark(caps.unicode, scaffolderVersion()));

  const positional = argv.filter(
    (a, i) => !a.startsWith("--") && !VALUE_FLAGS.includes(argv[i - 1]),
  );
  const prompt: PromptIo = { input: io.input, output: io.output, caps };
  const canPrompt = interactive(io.input, env);

  try {
    // The project name: positional arg, else prompt, else loud failure -
    // silently scaffolding nothing is worse.
    let name = positional[0];
    if (name === "create-pylair") {
      out(USAGE);
      return 1;
    }
    if (!name) {
      if (!canPrompt) return fail(`app name required: ${USAGE}`);
      name = await askText(prompt, "project name", (v) =>
        NAME_RE.test(v) ? undefined : NAME_RULE,
      );
    }
    if (!NAME_RE.test(name)) return fail(`invalid app name '${name}': ${NAME_RULE}`);

    // The mode: --mode, or implied by --framework (only wrap has one), else
    // prompt, else the bridge default.
    const frameworkFlag = argValue(argv, "--framework");
    let mode = argValue(argv, "--mode");
    if (mode !== undefined && mode !== "bridge" && mode !== "wrap")
      return fail(`--mode must be "bridge" or "wrap", got '${mode}'`);
    if (frameworkFlag !== undefined) {
      if (!WRAP_FRAMEWORKS.includes(frameworkFlag as WrapFramework))
        return fail(`--framework must be "command", "gradio" or "streamlit", got '${frameworkFlag}'`);
      if (mode === "bridge") return fail("--framework is for wrap mode; --mode bridge cannot use it");
      mode = "wrap";
    }
    if (!mode && canPrompt)
      mode = await askSelect(prompt, "what are you building?", [
        { value: "bridge", label: "a new UI that calls Python" },
        { value: "wrap", label: "wrap an existing Python web app" },
      ]);
    mode ??= "bridge";

    // The framework, wrap mode only: --framework, else prompt, else the
    // command default the template has always shipped.
    let framework = (frameworkFlag ?? "command") as WrapFramework;
    if (mode === "wrap" && !frameworkFlag && canPrompt)
      framework = await askSelect(prompt, "what does the app run?", [
        { value: "command", label: "a server command (uvicorn, gunicorn, ...)" },
        { value: "gradio", label: "Gradio" },
        { value: "streamlit", label: "Streamlit" },
      ]);

    // Auto-update opt-in (decision 0011): --with-updater, else prompt with
    // the default on "no" - unsigned apps shouldn't self-update.
    let withUpdater = argv.includes("--with-updater");
    if (!withUpdater && canPrompt)
      withUpdater = await askSelect(prompt, "add auto-update support? (signing key via pylair keys init)", [
        { value: false, label: "no" },
        { value: true, label: "yes" },
      ]);

    return scaffold(name, mode, framework, argv, io, cwd, caps, fail, withUpdater);
  } catch (e) {
    if (e instanceof PromptCancelled) {
      out("cancelled.");
      return 130;
    }
    throw e;
  }
}

/** Add the wrap shell (src-tauri, ui, package.json, .gitignore) to an
 * existing project, never overwriting a file and leaving the project's own
 * app, pyproject.toml and requirements.txt alone. `pylair wrap` writes the
 * `[tool.pylair]` config first, then runs this; the last stdout line is the
 * JSON result it parses, so this path prints nothing else. */
function into(argv: string[], io: RunIo, cwd: string): number {
  const fail = (msg: string): number => {
    io.error.write(`create-pylair: ${msg}\n`);
    return 1;
  };
  if (argv.includes("--with-updater"))
    return fail(
      "--with-updater applies to fresh scaffolds - for an existing project, " +
        "add plugins.updater by hand (docs/updates.md)",
    );
  const dir = resolve(cwd, argValue(argv, "--into") ?? ".");
  if (argValue(argv, "--mode") !== "wrap") return fail("--into needs --mode wrap");
  if (!existsSync(dir) || !statSync(dir).isDirectory()) return fail(`${dir} is not a directory`);
  const local = argValue(argv, "--local");
  const why = wrapUnavailable(local);
  if (why) return fail(why);
  const name = argValue(argv, "--name");
  if (!name || !NAME_RE.test(name))
    return fail(`--name is required and must be lowercase letters, digits, dashes (got '${name ?? ""}')`);
  const identifier = argValue(argv, "--identifier") ?? `com.example.${name.replace(/-/g, "")}`;
  const { written, kept } = copyTree(join(TEMPLATES, "wrap"), dir, substitutions(name, identifier, local), {
    keepExisting: true,
    skip: (rel) =>
      rel === "app" ||
      rel === "pyproject.toml" ||
      rel === "requirements.txt" ||
      rel === "variants" ||
      rel.startsWith("variants/"),
  });
  io.output.write(`${JSON.stringify({ written, kept })}\n`);
  return 0;
}

function scaffold(
  name: string,
  mode: string,
  framework: WrapFramework,
  argv: string[],
  io: RunIo,
  cwd: string,
  caps: Caps,
  fail: (msg: string) => number,
  withUpdater = false,
): number {
  const identifier = argValue(argv, "--identifier") ?? `com.example.${name.replace(/-/g, "")}`;
  const local = argValue(argv, "--local");
  const explicit = argValue(argv, "--template");
  if (explicit && mode === "wrap" && explicit !== "wrap")
    return fail(`--mode wrap uses the "wrap" template, not '${explicit}'`);
  const template = explicit ?? (mode === "wrap" ? "wrap" : "default");
  if (template === "wrap") {
    const why = wrapUnavailable(local);
    if (why) return fail(why);
  }
  const templateDir = join(TEMPLATES, template);
  if (!existsSync(templateDir)) return fail(`no template '${template}' at ${templateDir}`);

  const dest = resolve(cwd, name);
  if (existsSync(dest) && readdirSync(dest).length) return fail(`${dest} exists and is not empty`);

  const app = WRAP_APPS[framework];
  copyTree(templateDir, dest, substitutions(name, identifier, local, framework), {
    skip: (rel) =>
      rel === "variants" || rel.startsWith("variants/") || (app.sample !== undefined && rel === "app/main.py"),
  });
  if (app.sample !== undefined)
    writeFileSync(
      join(dest, "app", "app.py"),
      readFileSync(join(templateDir, "variants", app.sample), "utf-8"),
    );
  if (!statSync(join(dest, "src-tauri/tauri.conf.json")).isFile())
    return fail("template produced no tauri.conf.json (internal error)");
  if (withUpdater) applyUpdater(dest);

  // The outro prints through the shared ui: a success line, then the Next
  // commands with the command accented and the comment muted. Plain caps
  // leave every byte of the block it has always printed.
  const u = new Ui(caps, { out: (s) => io.output.write(s), err: (s) => io.error.write(s) });
  const next: [cmd: string, note: string][] = [
    [`cd ${name}`, ""],
    ["npm install", "# or pnpm install"],
    ["uv sync", "# python env (https://docs.astral.sh/uv/)"],
    [
      "npx pylair dev",
      mode === "wrap" ? "# your web app in a native window, with reload" : "# supervised dev: python hot reload + vite",
    ],
    ...(mode === "wrap" ? [] : ([["", "# (pnpm users: pnpm pylair dev / pnpm pylair build)"]] as [string, string][])),
    ["npx pylair build", "# native installer with bundled python"],
    ...(withUpdater
      ? ([["npx pylair keys init", "# generate the update signing keypair"]] as [string, string][])
      : []),
    ["npx pylair doctor", "# diagnose environment any time"],
  ];
  u.ok(`Created ${name}/ (${mode === "wrap" ? `wrap, ${framework}` : "bridge"})`);
  u.line();
  u.line(paint(caps, "bold", "Next:"));
  for (const [cmd, note] of next)
    u.line(
      note
        ? `  ${paint(caps, "accent", cmd.padEnd(Math.max(20, cmd.length + 1)))}${paint(caps, "muted", note)}`
        : `  ${paint(caps, "accent", cmd)}`,
    );

  // Preflight, reusing `pylair doctor`'s checks (the same subset `pylair
  // dev` gates the Rust build on - vendored from packages/cli): a missing
  // tool here is a warning, not a failure - the scaffold itself is done.
  // A named list now beats discovering it minutes into the first compile.
  const missing = missingTools((io.preflight ?? preflightChecks)());
  if (missing.length) {
    u.line();
    u.line(
      paint(caps, "warn", "Missing prerequisites (install before `npx pylair dev`):"),
    );
    for (const l of formatMissing(missing)) u.line(`  ${l}`);
  }
  u.line();
  return 0;
}
