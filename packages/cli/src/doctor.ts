/** `pylair doctor` — diagnose the dev/build environment. */

import { spawnSync } from "node:child_process";
import { existsSync, readFileSync, readdirSync } from "node:fs";
import { join } from "node:path";
import { platform, arch } from "node:os";
import { checkCapabilities, formatFinding, loadCapabilities } from "./capabilities.js";
import { loadConfig, loadTauriConfig, type PylairConfig } from "./config.js";
import { venvPython } from "./dev.js";
import { hostTriple } from "./build.js";
import { wrapWarnings } from "./wrapconfig.js";
import { STAGED_APP, runSmokeImports, smokeImports } from "./wrapbuild.js";

interface Check {
  name: string;
  ok: boolean;
  detail: string;
}

function probe(cmd: string, args: string[]): string | null {
  const r = spawnSync(cmd, args, { encoding: "utf-8" });
  return r.status === 0 ? (r.stdout || r.stderr).trim() : null;
}

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
  add: (name: string, ok: boolean, detail: string) => void,
): void {
  const w = cfg.wrap!;
  add("[tool.pylair] config", true, `mode=wrap framework=${w.framework} source=${cfg.source}`);
  const warns = wrapWarnings(w);
  add("wrap command", warns.length === 0, warns.length ? warns.join("\n    ") : w.cmd ? w.cmd.join(" ") : `entry ${w.entry}`);

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
  );

  const src = join(appDir, cfg.source);
  add("source dir", existsSync(src), src);
  if (w.entry) add("entry file", existsSync(join(src, w.entry)), join(src, w.entry));

  // The framework must import in the env `pylair dev` uses.
  const mods = smokeImports(w);
  if (mods.length) {
    const py = venvPython(appDir);
    if (!existsSync(py)) add("framework importable", false, `no venv at ${py}: run \`uv sync\``);
    else {
      const bad = runSmokeImports(py, mods, { PYTHONPATH: src }, appDir);
      add("framework importable", !bad, bad ?? `import ${mods.join(", ")} ok in the project venv`);
    }
  } else {
    add("framework importable", true, "nothing to import for this cmd (not `python -m <module>`)");
  }

  // The window shell the plugin drives, and the no-IPC guarantee for the
  // wrapped page: capabilities must not grant any remote url.
  for (const f of ["ui/index.html", "ui/restarting.html"])
    add(f, existsSync(join(appDir, f)), join(appDir, f));
  try {
    const conf = JSON.parse(readFileSync(join(appDir, "src-tauri", "tauri.conf.json"), "utf-8"));
    const main = (conf.app?.windows ?? []).find((x: { label?: string }) => x.label === "main");
    add(
      'window "main" has create: false',
      main?.create === false,
      main?.create === false ? "the plugin builds it with the download and new-window hooks" : "set \"create\": false on the main window in tauri.conf.json",
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
    add("bundled env imports framework", !bad, bad ?? "ok with the bundled interpreter (last build)");
  }
}

export async function doctor(appDir: string): Promise<number> {
  const checks: Check[] = [];
  const add = (name: string, ok: boolean, detail: string) =>
    checks.push({ name, ok, detail });

  const pyVer = probe("python3", ["-c", "import sys;print('%d.%d'%sys.version_info[:2])"]);
  const py = pyVer ? "python3" : "python";
  const pyV = pyVer ?? probe("python", ["-c", "import sys;print('%d.%d'%sys.version_info[:2])"]);
  add(
    "python >= 3.12",
    !!pyV && meetsMinPython(pyV, "3.12"),
    pyV ? `found ${py} ${pyV}` : "not found",
  );

  add("uv", !!probe("uv", ["--version"]), probe("uv", ["--version"]) ?? "not found");
  add("node", true, process.version);
  add(
    "rust/cargo",
    !!probe("cargo", ["--version"]),
    probe("cargo", ["--version"]) ?? "not found",
  );

  let triple = "unsupported";
  try {
    triple = hostTriple();
  } catch { /* stays unsupported */ }
  add("host triple", triple !== "unsupported", `${platform()}/${arch()} → ${triple}`);

  if (existsSync(join(appDir, "pyproject.toml"))) {
    try {
      const cfg = loadConfig(appDir);
      if (cfg.mode === "wrap") {
        wrapChecks(appDir, cfg, add);
      } else {
        add("[tool.pylair] config", true, `app-module=${cfg.appModule} source=${cfg.source}`);
        add("source dir", existsSync(join(appDir, cfg.source)), join(appDir, cfg.source));
        add(
          "requirements",
          existsSync(join(appDir, cfg.requirements)),
          cfg.requirements,
        );
      }
    } catch (e) {
      add("[tool.pylair] config", false, (e as Error).message);
    }
    try {
      const t = loadTauriConfig(appDir);
      add("tauri.conf.json", true, `${t.productName} (${t.identifier})`);
    } catch (e) {
      add("tauri.conf.json", false, (e as Error).message);
    }
    add("venv", existsSync(venvPython(appDir)), venvPython(appDir));

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
    );
  } else {
    add("app dir", false, `${appDir} has no pyproject.toml`);
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
      return { ok: true, detail: "not running (optional — pylair-ollama)" };
    }
  })();
  checks.push({ name: "ollama (optional)", ok: ollama.ok, detail: ollama.detail });

  const w = Math.max(...checks.map((c) => c.name.length));
  for (const c of checks)
    console.log(`${c.ok ? " ok " : "FAIL"} ${c.name.padEnd(w)}  ${c.detail}`);
  const fails = checks.filter((c) => !c.ok).length;
  console.log(fails ? `\n${fails} problem(s) found` : "\nall checks passed");
  return fails ? 1 : 0;
}
