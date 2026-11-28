/** App config for `pylair build` - one file, [tool.pylair] in pyproject.toml.
 * App identity (productName/identifier/version) is derived from
 * src-tauri/tauri.conf.json - Tauri owns it; we don't duplicate it.
 */

import { existsSync, readFileSync } from "node:fs";
import { createRequire } from "node:module";
import { dirname, join } from "node:path";
import { parse as parseToml } from "smol-toml";
import { MODES, parseWrapConfig } from "./wrapconfig.js";
import type { Mode, WrapConfig } from "./wrapconfig.js";

export type { Mode, WrapConfig } from "./wrapconfig.js";

export interface PylairConfig {
  /** `bridge` (default) or `wrap` (an existing Python web UI in the window). */
  mode: Mode;
  /** `[tool.pylair.wrap]`; present exactly when `mode` is `wrap`. */
  wrap?: WrapConfig;
  /** Python module holding the @expose functions (e.g. "app.backend").
   * Empty in wrap mode, which has no @expose surface. */
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
   * exclusions - globs relative to `source`, same shape as `prune`. */
  include: string[];
  /** Bundle target override (default: per-OS sensible set). */
  bundles?: string;
  /** `[tool.pylair.watch]` - dev reload globs, relative to `source`. */
  watch: { include: string[]; ignore: string[] };
  /** `[tool.pylair.binaries]` - declared external binaries (decision 0014). */
  binaries: Record<string, BinaryDecl>;
}

/** One per-target source of a declared binary. Exactly one of `path`/`url`.
 * Single files hash the file or archive in `sha256`. `member` extracts one
 * file out of that archive. `executable` instead declares a directory unit:
 * url sources are archives hashed in `sha256`, directory path sources carry
 * `manifest`, a checked-in relative-path -> sha256 map of the tree. */
export interface BinaryTarget {
  path?: string;
  url?: string;
  sha256?: string;
  member?: string;
  executable?: string;
  manifest?: string;
}

/** A `[tool.pylair.binaries.<name>]` entry. `member` means the single-file
 * shape; `executable` means the directory-unit shape (0014 sec. 2). */
export interface BinaryDecl {
  license: string;
  licenseFile?: string;
  licenseUrl?: string;
  version?: string;
  targets: Record<string, BinaryTarget>;
}

/** The desktop triples pylair builds for; cross-compilation is out of
 * scope (0014 sec. 9) but every target triple must still parse. */
export const KNOWN_TRIPLES = [
  "x86_64-pc-windows-msvc",
  "aarch64-pc-windows-msvc",
  "x86_64-apple-darwin",
  "aarch64-apple-darwin",
  "x86_64-unknown-linux-gnu",
  "aarch64-unknown-linux-gnu",
] as const;

const BINARY_KEYS = ["license", "license_file", "license_url", "version"] as const;
const TARGET_KEYS = ["path", "url", "sha256", "member", "executable", "manifest"] as const;

/** The relative-path shape both archive members and unit executables use:
 * forward slashes, no absolute form, no empty or `..` segments. */
function relativePathProblem(value: string): string | null {
  if (!value) return "is empty";
  if (value.includes("\\")) return "must use forward slashes";
  if (value.startsWith("/") || /^[A-Za-z]:/.test(value)) return "must be relative, not absolute";
  for (const seg of value.split("/")) {
    if (seg === "..") return "must not contain a .. segment";
    if (seg === "") return "must not contain an empty segment";
  }
  return null;
}

/** Parse `[tool.pylair.binaries]`: strict keys, a mandatory license per
 * entry, per-target exactly-one-of path/url plus a hex sha256. Strictness
 * is the point - a typo'd key must fail loudly, never silently never ship. */
export function parseBinariesConfig(
  raw: unknown,
  pyproject: string,
): Record<string, BinaryDecl> {
  if (raw === undefined || raw === null) return {};
  if (typeof raw !== "object" || Array.isArray(raw))
    throw new Error(`[tool.pylair.binaries] must be a table, got ${JSON.stringify(raw)} (${pyproject})`);
  const out: Record<string, BinaryDecl> = {};
  for (const [name, v] of Object.entries(raw as Record<string, unknown>)) {
    const where = `[tool.pylair.binaries.${name}]`;
    if (typeof v !== "object" || v === null || Array.isArray(v))
      throw new Error(`${where} must be a table (${pyproject})`);
    const entry = v as Record<string, unknown>;
    for (const key of Object.keys(entry)) {
      if ((BINARY_KEYS as readonly string[]).includes(key)) continue;
      if ((KNOWN_TRIPLES as readonly string[]).includes(key)) continue;
      if (key.includes("-"))
        throw new Error(
          `${where} unknown target triple ${JSON.stringify(key)} - known triples: ${KNOWN_TRIPLES.join(", ")} (${pyproject})`,
        );
      throw new Error(
        `${where} unknown key ${JSON.stringify(key)} - valid keys: ${BINARY_KEYS.join(", ")} plus one table per target triple (${pyproject})`,
      );
    }
    const license = entry.license;
    if (typeof license !== "string" || license.trim() === "")
      throw new Error(
        `${where} license is required (an SPDX identifier) - shipping a binary without ` +
          `recording its license is never right (decision 0014 sec. 7) (${pyproject})`,
      );
    for (const key of ["license_file", "license_url", "version"] as const) {
      if (entry[key] !== undefined && typeof entry[key] !== "string")
        throw new Error(`${where} ${key} must be a string (${pyproject})`);
    }
    const targets: Record<string, BinaryTarget> = {};
    for (const triple of KNOWN_TRIPLES) {
      const t = entry[triple];
      if (t === undefined) continue;
      const tw = `${where}.${triple}`;
      if (typeof t !== "object" || t === null || Array.isArray(t))
        throw new Error(`${tw} must be a table (${pyproject})`);
      const tt = t as Record<string, unknown>;
      for (const key of Object.keys(tt))
        if (!(TARGET_KEYS as readonly string[]).includes(key))
          throw new Error(
            `${tw} unknown key ${JSON.stringify(key)} - valid keys: ${TARGET_KEYS.join(", ")} (${pyproject})`,
          );
      if (tt.executable !== undefined && typeof tt.executable !== "string")
        throw new Error(`${tw} executable must be a string (${pyproject})`);
      if (tt.member !== undefined && typeof tt.member !== "string")
        throw new Error(`${tw} member must be a string (${pyproject})`);
      if (tt.manifest !== undefined && typeof tt.manifest !== "string")
        throw new Error(`${tw} manifest must be a string (${pyproject})`);
      const executable = tt.executable as string | undefined;
      const member = tt.member as string | undefined;
      const isUnit = executable !== undefined;
      if (isUnit && member !== undefined)
        throw new Error(
          `${tw} has both member and executable - member extracts one file, executable ` +
            `declares a directory unit; use exactly one (${pyproject})`,
        );
      for (const [key, value] of [
        ["member", member],
        ["executable", executable],
      ] as const) {
        if (value === undefined) continue;
        const problem = relativePathProblem(value);
        if (problem) throw new Error(`${tw} ${key} ${problem}: ${JSON.stringify(value)} (${pyproject})`);
      }
      const hasPath = typeof tt.path === "string";
      const hasUrl = typeof tt.url === "string";
      if (tt.path !== undefined && !hasPath) throw new Error(`${tw} path must be a string (${pyproject})`);
      if (tt.url !== undefined && !hasUrl) throw new Error(`${tw} url must be a string (${pyproject})`);
      if (hasPath && hasUrl)
        throw new Error(`${tw} has both path and url - exactly one source per target (${pyproject})`);
      if (!hasPath && !hasUrl)
        throw new Error(
          `${tw} has neither path nor url - exactly one source per target (${pyproject})`,
        );
      const manifest = tt.manifest as string | undefined;
      if (!isUnit && manifest !== undefined)
        throw new Error(`${tw} manifest is only valid for a directory unit (with executable) (${pyproject})`);
      if (isUnit && hasUrl && manifest !== undefined)
        throw new Error(`${tw} manifest is only valid for a directory path source, not a url archive (${pyproject})`);
      if (isUnit && hasPath && (manifest === undefined || manifest.trim() === ""))
        throw new Error(
          `${tw} directory units need manifest = "path/to/manifest.json" - a checked-in ` +
            `map of every relative file path to its sha256 (${pyproject})`,
        );
      const needsSha256 = !isUnit || hasUrl;
      if (isUnit && hasPath && tt.sha256 !== undefined)
        throw new Error(
          `${tw} sha256 is not meaningful for a directory unit - the manifest hashes every file (${pyproject})`,
        );
      if (needsSha256) {
        if (typeof tt.sha256 !== "string")
          throw new Error(
            `${tw} sha256 is required - the hash is the reviewable record of what ships, even for path sources (${pyproject})`,
          );
        if (!/^[0-9a-f]{64}$/.test(tt.sha256))
          throw new Error(
            `${tw} sha256 must be 64 lowercase hex characters, got ${JSON.stringify(tt.sha256)} (${pyproject})`,
          );
      }
      targets[triple] = {
        path: hasPath ? (tt.path as string) : undefined,
        url: hasUrl ? (tt.url as string) : undefined,
        sha256: tt.sha256 as string | undefined,
        member,
        executable,
        manifest,
      };
    }
    out[name] = {
      license: license.trim(),
      licenseFile: entry.license_file as string | undefined,
      licenseUrl: entry.license_url as string | undefined,
      version: entry.version as string | undefined,
      targets,
    };
  }
  return out;
}

export interface TauriConfig {
  productName: string;
  identifier: string;
  version: string;
  /** bundle.macOS.signingIdentity - a configured identity means Tauri
   * signs for real during bundling; pylair must not ad-hoc over it. */
  macSigningIdentity?: string;
  /** plugins.updater - present exactly when the app is updater-enabled
   * (decision 0011). Drives the build-time version-sync check. */
  updater?: { pubkey?: string; endpoints?: string[] };
}

/** Parse [tool.pylair] with smol-toml - no Python interpreter required at
 * build/dev time (a system python3 may be <3.11 or absent entirely). */
export function loadConfig(appDir: string): PylairConfig {
  const pyproject = join(appDir, "pyproject.toml");
  if (!existsSync(pyproject))
    throw new Error(`no pyproject.toml in ${appDir}: is this a pylair app?`);

  let raw: Record<string, unknown>;
  try {
    const doc = parseToml(readFileSync(pyproject, "utf-8")) as {
      tool?: { pylair?: Record<string, unknown> };
    };
    raw = doc.tool?.pylair ?? {};
  } catch (e) {
    throw new Error(`could not parse ${pyproject}: ${(e as Error).message}`);
  }

  const modeRaw = raw.mode ?? "bridge";
  if (typeof modeRaw !== "string" || !(MODES as readonly string[]).includes(modeRaw))
    throw new Error(
      `[tool.pylair] mode must be "bridge" or "wrap", got ${JSON.stringify(modeRaw)} (${pyproject})`,
    );
  const mode = modeRaw as Mode;
  const wrap = mode === "wrap" ? parseWrapConfig(raw.wrap, pyproject) : undefined;

  const cfg: PylairConfig = {
    mode,
    wrap,
    appModule: String(raw["app-module"] ?? ""),
    source: String(raw.source ?? (mode === "wrap" ? "app" : "backend")),
    requirements: String(raw.requirements ?? "requirements.txt"),
    pythonVersion: String(raw["python-version"] ?? "3.12"),
    prune: Array.isArray(raw.prune) ? raw.prune.map(String) : [],
    include: Array.isArray(raw.include) ? raw.include.map(String) : [],
    bundles: raw.bundles ? String(raw.bundles) : undefined,
    watch: parseWatchConfig(raw.watch, pyproject),
    binaries: parseBinariesConfig(raw.binaries, pyproject),
  };
  if (mode === "bridge" && !cfg.appModule)
    throw new Error(
      `[tool.pylair] app-module is required in ${pyproject} ` +
        `(the module holding your @expose functions)`,
    );
  return cfg;
}

/** Parse `[tool.pylair.watch]`: dev reload include/ignore globs,
 * relative to `source`. Strict keys and strict string arrays - a typo'd
 * key or a non-string glob must fail loudly, not silently never fire. */
function parseWatchConfig(
  raw: unknown,
  pyproject: string,
): { include: string[]; ignore: string[] } {
  const out = { include: ["**/*.py"], ignore: [] as string[] };
  if (raw === undefined || raw === null) return out;
  if (typeof raw !== "object" || Array.isArray(raw))
    throw new Error(`[tool.pylair.watch] must be a table, got ${JSON.stringify(raw)} (${pyproject})`);
  for (const key of Object.keys(raw as Record<string, unknown>))
    if (key !== "include" && key !== "ignore")
      throw new Error(
        `[tool.pylair.watch] unknown key ${JSON.stringify(key)} - only "include" and "ignore" are valid (${pyproject})`,
      );
  const r = raw as Record<string, unknown>;
  for (const key of ["include", "ignore"] as const) {
    const v = r[key];
    if (v === undefined) continue;
    if (!Array.isArray(v) || v.some((g) => typeof g !== "string"))
      throw new Error(
        `[tool.pylair.watch] ${key} must be an array of glob strings (${pyproject})`,
      );
    if (v.some((g) => (g as string).trim() === ""))
      throw new Error(`[tool.pylair.watch] ${key} must not contain empty globs (${pyproject})`);
    (out[key] as string[]) = v as string[];
  }
  return out;
}

export function loadTauriConfig(appDir: string): TauriConfig {
  const p = join(appDir, "src-tauri", "tauri.conf.json");
  if (!existsSync(p))
    throw new Error(
      `no src-tauri/tauri.conf.json in ${appDir}: run this inside a pylair app`,
    );
  const raw = JSON.parse(readFileSync(p, "utf-8"));
  return {
    productName: raw.productName ?? "app",
    identifier: raw.identifier ?? "com.example.app",
    version: raw.version ?? "0.1.0",
    macSigningIdentity: raw.bundle?.macOS?.signingIdentity ?? undefined,
    updater: raw.plugins?.updater ?? undefined,
  };
}

/** The Windows signing configuration pylair itself needs for staged
 * external binaries: the cert-thumbprint env wins over the conf, and a
 * custom sign command is passed through verbatim. */
export function windowsSigningOptions(appDir: string): {
  certificateThumbprint?: string;
  signCommand?: string;
} {
  let confThumbprint: string | undefined;
  let signCommand: string | undefined;
  try {
    const raw = JSON.parse(
      readFileSync(join(appDir, "src-tauri", "tauri.conf.json"), "utf-8"),
    );
    confThumbprint = raw?.bundle?.windows?.certificateThumbprint ?? undefined;
    signCommand = raw?.bundle?.windows?.signCommand ?? undefined;
  } catch {
    // Missing or malformed conf means no conf-provided signing.
  }
  return {
    certificateThumbprint: process.env.WINDOWS_CERTIFICATE_THUMBPRINT ?? confThumbprint,
    signCommand,
  };
}

/** Windows signing counts as configured when the cert-thumbprint env is
 * set OR tauri.conf.json names a thumbprint or a custom sign command
 * under bundle.windows. Missing file, malformed JSON or absent keys all
 * mean not configured; never throws. */
export function windowsSigningConfigured(appDir: string): boolean {
  const opts = windowsSigningOptions(appDir);
  return !!(opts.certificateThumbprint || opts.signCommand);
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
 * with `node` directly - no `npx`/`npx.cmd` shell shim, same on every OS. */
export function tauriCliEntry(appDir: string): string {
  try {
    return createRequire(join(appDir, "package.json")).resolve("@tauri-apps/cli/tauri.js");
  } catch {
    throw new Error("@tauri-apps/cli is not installed in the app (run `pnpm install`).");
  }
}
