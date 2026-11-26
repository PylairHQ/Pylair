/** `pylair doctor`: diagnose the dev/build environment. */

import { existsSync, readFileSync, readdirSync } from "node:fs";
import { isAbsolute, join, relative } from "node:path";
import { platform, arch } from "node:os";
import { checkCapabilities, formatFinding, loadCapabilities } from "./capabilities.js";
import { loadConfig, loadTauriConfig, type PylairConfig } from "./config.js";
import { venvPython } from "./dev.js";
import { hostTriple } from "./build.js";
import { nativeToolchain, probe, realEnv } from "./preflight.js";
import { ui, type Hint } from "./ui/index.js";
import { wrapWarnings } from "./wrapconfig.js";
import { STAGED_APP, runSmokeImports, smokeImports } from "./wrapbuild.js";
import { judge, releaseVersion, resolveAppVersions, type AppVersions } from "./versions.js";

// The detection primitives live in ./preflight.js — the module
// `pylair dev` and create-pylair share (vendored there). Re-exported so
// existing imports from ./doctor.js keep working.
export { nativeToolchain } from "./preflight.js";
export type { NativeEnv } from "./preflight.js";

interface Check {
  name: string;
  ok: boolean;
  detail: string;
  /** Shown under a failure: the file to look at and a one-line fix. */
  hint?: Hint;
}

/** What `add` records. The hint is optional so callers that only want the
 * name, verdict and detail (the tests) keep working. */
export type AddCheck = (name: string, ok: boolean, detail: string, hint?: Hint) => void;

/** Numeric major.minor comparison — a string compare would pass "3.9"
 * for a "3.12" floor. Anything unparseable is false. */
export function meetsMinPython(version: string, min: string): boolean {
  const v = /^(\d+)\.(\d+)/.exec(version.trim());
  const m = /^(\d+)\.(\d+)/.exec(min.trim());
  if (!v || !m) return false;
  const [maj, mi] = [Number(v[1]), Number(v[2])];
  const [fmaj, fmi] = [Number(m[1]), Number(m[2])];
  return maj > fmaj || (maj === fmaj && mi >= fmi);
}

/** Wrap-mode checks. `add` records one line each. Exported for tests. */
export function wrapChecks(
  appDir: string,
  cfg: PylairConfig,
  add: AddCheck,
): void {
  const w = cfg.wrap!;
  add("[tool.pylair] config", true, `mode=wrap framework=${w.framework} source=${cfg.source}`);
  const warns = wrapWarnings(w);
  add("wrap command", warns.length === 0, warns.length ? warns.join("\n    ") : w.cmd ? w.cmd.join(" ") : `entry ${w.entry}`, {
    file: "pyproject.toml",
    fix: "edit [tool.pylair.wrap] (see docs/wrap.md)",
  });

  // Dependencies come from a manifest, never from guessing imports.
  const hasLock = existsSync(join(appDir, "uv.lock"));
  const hasReqs = existsSync(join(appDir, cfg.requirements));
  add(
    "dependency manifest",
    hasLock || hasReqs,
    hasLock
      ? "pyproject.toml + uv.lock"
      : hasReqs
        ? `${cfg.requirements} only (no uv.lock): the bundled env can drift from \`pylair dev\`'s; prefer \`uv add\``
        : "none: run `uv init` and `uv add <packages>` (Pylair does not infer dependencies from imports)",
    { file: "pyproject.toml", fix: "run `uv init`, then `uv add <packages>`" },
  );

  const src = join(appDir, cfg.source);
  add("source dir", existsSync(src), src, {
    file: "pyproject.toml",
    fix: "create the directory, or point [tool.pylair] source at the right one",
  });
  if (w.entry)
    add("entry file", existsSync(join(src, w.entry)), join(src, w.entry), {
      file: "pyproject.toml",
      fix: "set [tool.pylair.wrap] entry to a file under the source dir",
    });

  // The framework must import in the env `pylair dev` uses.
  const mods = smokeImports(w);
  if (mods.length) {
    const py = venvPython(appDir);
    if (!existsSync(py))
      add("framework importable", false, `no venv at ${py}: run \`uv sync\``, { file: py, fix: "run `uv sync`" });
    else {
      const bad = runSmokeImports(py, mods, { PYTHONPATH: src }, appDir);
      add("framework importable", !bad, bad ?? `import ${mods.join(", ")} ok in the project venv`, {
        file: "pyproject.toml",
        fix: "add the framework to your dependencies (`uv add`), then run `uv sync`",
      });
    }
  } else {
    add("framework importable", true, "nothing to import for this cmd (not `python -m <module>`)");
  }

  // The window shell the plugin drives, and the no-IPC guarantee for the
  // wrapped page: capabilities must not grant any remote url.
  for (const f of ["ui/index.html", "ui/restarting.html"])
    add(f, existsSync(join(appDir, f)), join(appDir, f), {
      file: join(appDir, f),
      fix: "restore it from the wrap template (`pylair wrap` writes the missing files)",
    });
  try {
    const conf = JSON.parse(readFileSync(join(appDir, "src-tauri", "tauri.conf.json"), "utf-8"));
    const main = (conf.app?.windows ?? []).find((x: { label?: string }) => x.label === "main");
    add(
      'window "main" has create: false',
      main?.create === false,
      main?.create === false ? "the plugin builds it with the download and new-window hooks" : "set \"create\": false on the main window in tauri.conf.json",
      { file: "src-tauri/tauri.conf.json", fix: 'set "create": false on the window labelled "main"' },
    );
  } catch {
    /* tauri.conf.json problems are reported by its own check */
  }
  const capDir = join(appDir, "src-tauri", "capabilities");
  const remote: string[] = [];
  if (existsSync(capDir))
    for (const f of readdirSync(capDir).filter((n) => n.endsWith(".json"))) {
      try {
        if (JSON.parse(readFileSync(join(capDir, f), "utf-8")).remote) remote.push(f);
      } catch {
        /* unparseable capability files are tauri's to report */
      }
    }
  add(
    "wrapped page gets no IPC",
    remote.length === 0,
    remote.length ? `capability ${remote.join(", ")} grants a remote url: the wrapped page would get Tauri IPC` : "no capability grants a remote url",
    {
      file: remote.map((r) => join("src-tauri", "capabilities", r)).join(", "),
      fix: "remove the \"remote\" block from that capability (docs/security.md)",
    },
  );

  // After a build: the bundled interpreter must still import the framework
  // (the post-prune smoke test `pylair build` runs).
  const res = join(appDir, "src-tauri", "resources");
  const bundled = join(res, "python", process.platform === "win32" ? "python.exe" : "bin/python3");
  if (mods.length && existsSync(bundled) && existsSync(join(res, STAGED_APP))) {
    const sep = process.platform === "win32" ? ";" : ":";
    const bad = runSmokeImports(
      bundled,
      mods,
      { PYTHONPATH: `${join(res, STAGED_APP)}${sep}${join(res, "site-packages")}`, PYTHONNOUSERSITE: "1", PYTHONDONTWRITEBYTECODE: "1" },
      appDir,
    );
    add("bundled env imports framework", !bad, bad ?? "ok with the bundled interpreter (last build)", {
      file: "pyproject.toml",
      fix: "check [tool.pylair] prune, then run `pylair build` again",
    });
  }
}

/** A path as the user would type it: relative to the app when inside it. */
function shown(appDir: string, p: string): string {
  return p
    .split(", ")
    .map((x) => {
      if (!isAbsolute(x)) return x;
      const r = relative(appDir, x);
      return r && !r.startsWith("..") && !isAbsolute(r) ? r : x;
    })
    .join(", ");
}

/** The Versions section: what the app actually uses per ecosystem
 * (declared + lockfile-resolved), whether the combination is one release's
 * compatible set, and - when it is not - exactly which part is out of
 * step. Unresolved (no lockfile yet) is never a failure: a fresh scaffold
 * has not installed anything. */
export function versionChecks(appDir: string, add: AddCheck): void {
  const v = resolveAppVersions(appDir);
  const rows: { key: keyof AppVersions; label: string }[] = [
    { key: "client", label: "@pylair/client" },
    { key: "cli", label: "@pylair/cli" },
    { key: "crate", label: "tauri-plugin-pylair" },
    { key: "pylair", label: "pylair" },
  ];
  let present = 0;
  for (const { key, label } of rows) {
    const c = v[key];
    if (c.status === "absent") continue;
    present += 1;
    const detail =
      c.status === "resolved"
        ? `${c.resolved}`
        : c.status === "local"
          ? `local checkout (${c.declared})`
          : `unresolved - declared ${c.declared}; ${c.detail}`;
    add(`versions: ${label}`, true, detail);
  }
  if (present === 0) return;
  const verdict = judge(v);
  if (verdict.kind === "compatible") {
    const r = verdict.release;
    add(
      "versions: compatibility",
      true,
      `a compatible set (npm-v${r.npm}: crate ${r.crate}, pylair ${r.pylair}) - the ecosystems version independently, so differing numbers are expected, not skew`,
    );
  } else if (verdict.kind === "mismatched") {
    add(
      "versions: compatibility",
      false,
      [
        `mismatched - not any single release's set; closest is npm-v${verdict.best.npm}:`,
        ...verdict.offenders.map(
          (o) => `  ${o.component} ${o.have} is out of step - ${o.want} pairs with the rest`,
        ),
      ].join("\n    "),
      { fix: "run `pylair upgrade --check` for the exact edits" },
    );
  } else if (verdict.kind === "unresolved") {
    add(
      "versions: compatibility",
      true,
      `unjudged - unresolved: ${verdict.components.join(", ")} (install/build/uv sync first)`,
    );
  }
}

export async function doctor(appDir: string): Promise<number> {
  const checks: Check[] = [];
  const add: AddCheck = (name, ok, detail, hint) => checks.push({ name, ok, detail, hint });

  const pyVer = probe("python3", ["-c", "import sys;print('%d.%d'%sys.version_info[:2])"]);
  const py = pyVer ? "python3" : "python";
  const pyV = pyVer ?? probe("python", ["-c", "import sys;print('%d.%d'%sys.version_info[:2])"]);
  add(
    "python >= 3.12",
    !!pyV && meetsMinPython(pyV, "3.12"),
    pyV ? `found ${py} ${pyV}` : "not found",
    { fix: "install Python 3.12 or newer (`uv python install 3.12`)" },
  );

  const uvVer = probe("uv", ["--version"]);
  add("uv", !!uvVer, uvVer ?? "not found", { fix: "install uv: https://docs.astral.sh/uv/" });
  add("node", true, process.version);
  const cargoVer = probe("cargo", ["--version"]);
  add("rust/cargo", !!cargoVer, cargoVer ?? "not found", { fix: "install Rust: https://rustup.rs" });
  const tc = nativeToolchain(realEnv());
  add("native toolchain", tc.ok, tc.detail, tc.fix ? { fix: tc.fix } : undefined);

  let triple = "unsupported";
  try {
    triple = hostTriple();
  } catch { /* stays unsupported */ }
  add("host triple", triple !== "unsupported", `${platform()}/${arch()} (${triple})`, {
    fix: "pylair builds on Windows, macOS and Linux (x64 and arm64) only",
  });

  if (existsSync(join(appDir, "pyproject.toml"))) {
    try {
      const cfg = loadConfig(appDir);
      if (cfg.mode === "wrap") {
        wrapChecks(appDir, cfg, add);
      } else {
        add("[tool.pylair] config", true, `app-module=${cfg.appModule} source=${cfg.source}`);
        add("source dir", existsSync(join(appDir, cfg.source)), join(appDir, cfg.source), {
          file: "pyproject.toml",
          fix: "create the directory, or point [tool.pylair] source at the right one",
        });
        add("requirements", existsSync(join(appDir, cfg.requirements)), cfg.requirements, {
          file: "pyproject.toml",
          fix: "create the file, or set [tool.pylair] requirements (uv.lock is used when present)",
        });
      }
    } catch (e) {
      add("[tool.pylair] config", false, (e as Error).message, {
        file: "pyproject.toml",
        fix: "fix the [tool.pylair] table (see docs/build.md)",
      });
    }
    try {
      const t = loadTauriConfig(appDir);
      add("tauri.conf.json", true, `${t.productName} (${t.identifier})`);
    } catch (e) {
      add("tauri.conf.json", false, (e as Error).message, {
        file: "src-tauri/tauri.conf.json",
        fix: "run this inside a pylair app; the file needs productName and identifier",
      });
    }
    add("venv", existsSync(venvPython(appDir)), venvPython(appDir), { file: venvPython(appDir), fix: "run `uv sync`" });

    // Per-function permission scopes (static; exposed names are not loaded
    // here, so unmatched-pattern warnings are left to `pylair build`).
    const loaded = loadCapabilities(appDir);
    const found = [...loaded.findings, ...checkCapabilities(loaded.capabilities)];
    const bad = found.filter((f) => f.level === "error");
    add(
      "capability scopes",
      bad.length === 0,
      found.length === 0
        ? `${loaded.capabilities.length} capability file(s) ok`
        : found.map(formatFinding).join("\n    "),
      { file: "src-tauri/capabilities", fix: "fix the listed scopes (docs/security.md)" },
    );
    versionChecks(appDir, add);
  } else {
    add("app dir", false, `${appDir} has no pyproject.toml`, {
      file: join(appDir, "pyproject.toml"),
      fix: "run this inside a pylair app, or pass --dir <path>",
    });
  }

  // Optional integration: report Ollama when present; absence is a note,
  // not a failure — it's opt-in.
  const ollama = await (async () => {
    try {
      const r = await fetch("http://127.0.0.1:11434/api/tags", {
        signal: AbortSignal.timeout(1500),
      });
      if (!r.ok) return { ok: false, detail: `API returned ${r.status}` };
      const j = (await r.json()) as { models?: { name: string }[] };
      return { ok: true, detail: `${j.models?.length ?? 0} model(s) available` };
    } catch {
      return { ok: true, detail: "not running (optional, pylair-ollama)" };
    }
  })();
  checks.push({
    name: "ollama (optional)",
    ok: ollama.ok,
    detail: ollama.detail,
    hint: { fix: "restart Ollama, or ignore it: it is optional" },
  });

  // Plain output keeps the exact legacy lines (` ok `/`FAIL`, padded name, two
  // spaces, detail): scripts and tests read them. Fancy aligns a symbol, the
  // name and the detail; a failure shows its file and fix underneath in both.
  ui.header("pylair doctor", [["app", appDir]], []);
  const w = Math.max(...checks.map((c) => c.name.length));
  for (const c of checks)
    ui.check(
      c.ok,
      c.name,
      w,
      c.detail,
      c.hint && { fix: c.hint.fix, file: c.hint.file && shown(appDir, c.hint.file) },
    );
  const fails = checks.filter((c) => !c.ok).length;
  ui.line();
  ui.summary(fails ? "fail" : "ok", fails ? `${fails} problem(s) found` : "all checks passed");
  return fails ? 1 : 0;
}
