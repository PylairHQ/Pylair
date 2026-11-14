/** App config for `pylair build` — one file, [tool.pylair] in pyproject.toml.
 * App identity (productName/identifier/version) is derived from
 * src-tauri/tauri.conf.json — Tauri owns it; we don't duplicate it.
 */

import { existsSync, readFileSync } from "node:fs";
import { createRequire } from "node:module";
import { dirname, join } from "node:path";
import { parse as parseToml } from "smol-toml";

export interface PylairConfig {
  /** Python module holding the @expose functions (e.g. "app.backend"). */
  appModule: string;
  /** Directory staged as the app backend source (default "backend"). */
  source: string;
  /** Locked requirements file (default "requirements.txt"). */
  requirements: string;
  /** Bundled interpreter version class (default "3.12"). */
  pythonVersion: string;
  /** Extra prune globs on top of the defaults. */
  prune: string[];
  /** Source paths to stage even though they match the secret-looking
   * exclusions — globs relative to `source`, same shape as `prune`. */
  include: string[];
  /** Bundle target override (default: per-OS sensible set). */
  bundles?: string;
}

export interface TauriConfig {
  productName: string;
  identifier: string;
  version: string;
  /** bundle.macOS.signingIdentity — a configured identity means Tauri
   * signs for real during bundling; pylair must not ad-hoc over it. */
  macSigningIdentity?: string;
}

const KEYMAP: Record<string, keyof PylairConfig> = {
  "app-module": "appModule",
  source: "source",
  requirements: "requirements",
  "python-version": "pythonVersion",
  prune: "prune",
  include: "include",
  bundles: "bundles",
};

/** Parse [tool.pylair] with smol-toml — no Python interpreter required at
 * build/dev time (a system python3 may be <3.11 or absent entirely). */
export function loadConfig(appDir: string): PylairConfig {
  const pyproject = join(appDir, "pyproject.toml");
  if (!existsSync(pyproject))
    throw new Error(`no pyproject.toml in ${appDir} — is this a pylair app?`);

  let raw: Record<string, unknown>;
  try {
    const doc = parseToml(readFileSync(pyproject, "utf-8")) as {
      tool?: { pylair?: Record<string, unknown> };
    };
    raw = doc.tool?.pylair ?? {};
  } catch (e) {
    throw new Error(`could not parse ${pyproject}: ${(e as Error).message}`);
  }

  const cfg: PylairConfig = {
    appModule: String(raw["app-module"] ?? ""),
    source: String(raw.source ?? "backend"),
    requirements: String(raw.requirements ?? "requirements.txt"),
    pythonVersion: String(raw["python-version"] ?? "3.12"),
    prune: Array.isArray(raw.prune) ? raw.prune.map(String) : [],
    include: Array.isArray(raw.include) ? raw.include.map(String) : [],
    bundles: raw.bundles ? String(raw.bundles) : undefined,
  };
  if (!cfg.appModule)
    throw new Error(
      `[tool.pylair] app-module is required in ${pyproject} ` +
        `(the module holding your @expose functions)`,
    );
  return cfg;
}

export function loadTauriConfig(appDir: string): TauriConfig {
  const p = join(appDir, "src-tauri", "tauri.conf.json");
  if (!existsSync(p))
    throw new Error(
      `no src-tauri/tauri.conf.json in ${appDir} — run this inside a pylair app`,
    );
  const raw = JSON.parse(readFileSync(p, "utf-8"));
  return {
    productName: raw.productName ?? "app",
    identifier: raw.identifier ?? "com.example.app",
    version: raw.version ?? "0.1.0",
    macSigningIdentity: raw.bundle?.macOS?.signingIdentity ?? undefined,
  };
}

/** Windows signing counts as configured when the cert-thumbprint env is
 * set OR tauri.conf.json names a thumbprint or a custom sign command
 * under bundle.windows. Missing file, malformed JSON or absent keys all
 * mean not configured; never throws. */
export function windowsSigningConfigured(appDir: string): boolean {
  if (process.env.WINDOWS_CERTIFICATE_THUMBPRINT) return true;
  try {
    const raw = JSON.parse(
      readFileSync(join(appDir, "src-tauri", "tauri.conf.json"), "utf-8"),
    );
    return !!(
      raw?.bundle?.windows?.certificateThumbprint ||
      raw?.bundle?.windows?.signCommand
    );
  } catch {
    return false;
  }
}

/**
 * The directory that must be on PYTHONPATH for `cfg.appModule` to import:
 * the PARENT of the source dir ("backend.app" lives at <root>/backend/app.py).
 * Single definition shared by dev, the build import check, and (mirrored in
 * Rust) the release spawn spec.
 */
export function importRoot(base: string, source: string): string {
  return join(base, dirname(source));
}

/** Absolute path of the app's installed Tauri CLI entry (JS), so it can be run
 * with `node` directly — no `npx`/`npx.cmd` shell shim, same on every OS. */
export function tauriCliEntry(appDir: string): string {
  try {
    return createRequire(join(appDir, "package.json")).resolve("@tauri-apps/cli/tauri.js");
  } catch {
    throw new Error("@tauri-apps/cli is not installed in the app (run `pnpm install`).");
  }
}
