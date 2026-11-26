/** Read the Pylair versions an app actually uses: the requirement as
 * declared in each ecosystem's manifest plus the exact version resolved
 * in its lockfile. Never guesses - a missing lockfile or entry is
 * "unresolved", a file:/path dependency is "local", an undeclared
 * component is "absent". Layout is identical for bridge and wrap apps
 * (package.json, src-tauri/Cargo.toml, pyproject.toml), so one probe
 * covers both. */

import { existsSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { parse as parseToml } from "smol-toml";
import { COMPAT, type CompatRelease } from "./compat.js";

export type ResolveStatus = "resolved" | "unresolved" | "local" | "absent";

export interface ComponentVersion {
  /** The requirement exactly as declared, null when not declared. */
  declared: string | null;
  /** The exact lockfile-resolved version, null when unavailable. */
  resolved: string | null;
  status: ResolveStatus;
  /** One-line explanation when not resolved. */
  detail?: string;
}

export interface AppVersions {
  /** @pylair/client (npm). */
  client: ComponentVersion;
  /** @pylair/cli (npm). */
  cli: ComponentVersion;
  /** tauri-plugin-pylair (crates.io). */
  crate: ComponentVersion;
  /** pylair (PyPI). */
  pylair: ComponentVersion;
}

function absent(): ComponentVersion {
  return { declared: null, resolved: null, status: "absent" };
}

function local(declared: string): ComponentVersion {
  return {
    declared,
    resolved: null,
    status: "local",
    detail: "path dependency (a checkout, not a registry version)",
  };
}

function finish(declared: string, resolved: string | null, missingLock: string): ComponentVersion {
  if (resolved) return { declared, resolved, status: "resolved" };
  return { declared, resolved: null, status: "unresolved", detail: missingLock };
}

const LOCALISH = /^(file:|link:|workspace:)/;

// ---- npm: package.json + package-lock.json / pnpm-lock.yaml ----------

interface PkgJson {
  dependencies?: Record<string, string>;
  devDependencies?: Record<string, string>;
}

function declaredNpm(pkg: PkgJson | null, name: string): string | null {
  return pkg?.dependencies?.[name] ?? pkg?.devDependencies?.[name] ?? null;
}

/** package-lock.json v2/v3: the packages map keys on node_modules paths. */
function npmLockVersion(appDir: string, name: string): string | null {
  const lockPath = join(appDir, "package-lock.json");
  if (!existsSync(lockPath)) return null;
  try {
    const lock = JSON.parse(readFileSync(lockPath, "utf-8")) as {
      packages?: Record<string, { version?: string }>;
    };
    return lock.packages?.[`node_modules/${name}`]?.version ?? null;
  } catch {
    return null;
  }
}

/** pnpm-lock.yaml v9: importers "." dep blocks carry specifier/version.
 * Targeted extraction, not YAML parsing - the lockfile shape we need is
 * exactly two indented keys under the quoted package name. */
function pnpmLockVersion(appDir: string, name: string): string | null {
  const lockPath = join(appDir, "pnpm-lock.yaml");
  if (!existsSync(lockPath)) return null;
  try {
    const text = readFileSync(lockPath, "utf-8");
    const needle = new RegExp(
      `^\\s{6,8}['"]?${name.replace("/", "\\/")}['"]?:\\s*\\n(?:\\s+specifier:[^\\n]*\\n)?\\s+version:\\s*([^\\s]+)\\s*$`,
      "m",
    );
    const m = needle.exec(text);
    if (!m) return null;
    // Link/file resolutions are checkouts, not registry versions.
    if (/^(link:|file:)/.test(m[1])) return null;
    return m[1].replace(/['"]/g, "");
  } catch {
    return null;
  }
}

function probeNpm(appDir: string, name: string): ComponentVersion {
  const pkgPath = join(appDir, "package.json");
  if (!existsSync(pkgPath)) return absent();
  let pkg: PkgJson | null = null;
  try {
    pkg = JSON.parse(readFileSync(pkgPath, "utf-8")) as PkgJson;
  } catch {
    return absent();
  }
  const declared = declaredNpm(pkg, name);
  if (!declared) return absent();
  if (LOCALISH.test(declared)) return local(declared);
  const hasAnyLock =
    existsSync(join(appDir, "package-lock.json")) || existsSync(join(appDir, "pnpm-lock.yaml"));
  const resolved = npmLockVersion(appDir, name) ?? pnpmLockVersion(appDir, name);
  return finish(
    declared,
    resolved,
    hasAnyLock
      ? "lockfile has no entry - run the install again"
      : "no lockfile yet - install first (npm install / pnpm install)",
  );
}

// ---- crate: src-tauri/Cargo.toml + Cargo.lock -------------------------

function probeCrate(appDir: string): ComponentVersion {
  const tomlPath = join(appDir, "src-tauri", "Cargo.toml");
  if (!existsSync(tomlPath)) return absent();
  let declared: string | null = null;
  try {
    const doc = parseToml(readFileSync(tomlPath, "utf-8")) as {
      dependencies?: Record<string, unknown>;
      "build-dependencies"?: Record<string, unknown>;
    };
    const dep = doc.dependencies?.["tauri-plugin-pylair"] ?? doc["build-dependencies"]?.["tauri-plugin-pylair"];
    if (typeof dep === "string") declared = dep;
    else if (dep && typeof dep === "object") {
      const table = dep as Record<string, unknown>;
      if (typeof table.path === "string") return local(`path = "${table.path}"`);
      if (typeof table.version === "string") declared = table.version;
    }
  } catch {
    return { declared: null, resolved: null, status: "unresolved", detail: "Cargo.toml unparsable" };
  }
  if (!declared) return absent();
  const lockPath = join(appDir, "src-tauri", "Cargo.lock");
  let resolved: string | null = null;
  if (existsSync(lockPath)) {
    try {
      const lock = parseToml(readFileSync(lockPath, "utf-8")) as {
        package?: { name?: string; version?: string }[];
      };
      resolved =
        lock.package?.find((p) => p.name === "tauri-plugin-pylair")?.version ?? null;
    } catch {
      /* unparsable lock - stays unresolved */
    }
  }
  return finish(
    declared,
    resolved,
    existsSync(lockPath)
      ? "Cargo.lock has no tauri-plugin-pylair entry - run cargo update"
      : "no Cargo.lock yet - build once (cargo build / pylair dev)",
  );
}

// ---- pylair: pyproject.toml + uv.lock ---------------------------------

function probePylair(appDir: string): ComponentVersion {
  const pyprojectPath = join(appDir, "pyproject.toml");
  if (!existsSync(pyprojectPath)) return absent();
  let declared: string | null = null;
  try {
    const doc = parseToml(readFileSync(pyprojectPath, "utf-8")) as {
      project?: { dependencies?: string[] };
    };
    for (const dep of doc.project?.dependencies ?? []) {
      // pylair==x.y, "pylair @ file:...", bare "pylair" - never pylair-ollama.
      const m = /^pylair(?!\S*-)\s*(==\s*(\S+)|@\s*(\S+))?\s*$/.exec(dep.trim());
      if (m) {
        if (m[3]?.startsWith("file:")) return local(dep.trim());
        declared = m[2] ? `==${m[2]}` : dep.trim();
        break;
      }
    }
  } catch {
    return { declared: null, resolved: null, status: "unresolved", detail: "pyproject.toml unparsable" };
  }
  if (!declared) return absent();
  const lockPath = join(appDir, "uv.lock");
  let resolved: string | null = null;
  if (existsSync(lockPath)) {
    try {
      const lock = parseToml(readFileSync(lockPath, "utf-8")) as {
        package?: { name?: string; version?: string }[];
      };
      resolved = lock.package?.find((p) => p.name === "pylair")?.version ?? null;
    } catch {
      /* unparsable lock - stays unresolved */
    }
  }
  return finish(
    declared,
    resolved,
    existsSync(lockPath)
      ? "uv.lock has no pylair entry - run uv lock"
      : "no uv.lock yet - run uv sync",
  );
}

/** Resolve every component. Pure file reading - no process spawns, fully
 * offline. */
export function resolveAppVersions(appDir: string): AppVersions {
  return {
    client: probeNpm(appDir, "@pylair/client"),
    cli: probeNpm(appDir, "@pylair/cli"),
    crate: probeCrate(appDir),
    pylair: probePylair(appDir),
  };
}

// ---- verdict: how the resolved set sits against the manifest ----------

export type Verdict =
  | { kind: "compatible"; release: CompatRelease }
  | {
      kind: "mismatched";
      /** The release most of the set agrees with (newest on ties). */
      best: CompatRelease;
      offenders: { component: string; have: string; want: string }[];
    }
  | { kind: "unresolved"; components: string[] }
  | { kind: "empty" };

interface Participation {
  key: keyof AppVersions;
  label: string;
  version: string | null;
  unresolved: boolean;
}

function participants(v: AppVersions): Participation[] {
  const rows: { key: keyof AppVersions; label: string }[] = [
    { key: "client", label: "@pylair/client" },
    { key: "cli", label: "@pylair/cli" },
    { key: "crate", label: "tauri-plugin-pylair" },
    { key: "pylair", label: "pylair" },
  ];
  return rows
    .filter(({ key }) => v[key].status !== "absent")
    .map(({ key, label }) => ({
      key,
      label,
      version: v[key].resolved,
      unresolved: v[key].status === "unresolved" || v[key].status === "local",
    }));
}

/** The version of `key` in a release tuple. client/cli share npm. */
export function releaseVersion(r: CompatRelease, key: keyof AppVersions): string {
  if (key === "client" || key === "cli") return r.npm;
  if (key === "crate") return r.crate;
  return r.pylair;
}

/** Judge the resolved set against the manifest. A set is compatible when
 * every component with a resolved version matches one release's tuple -
 * subsets are fine (a wrap app may not use the client). Anything
 * unresolved makes the whole verdict unjudgable: a wrong guess is worse
 * than no guess. */
export function judge(v: AppVersions): Verdict {
  const parts = participants(v);
  if (parts.length === 0) return { kind: "empty" };
  const unresolvedNames = parts.filter((p) => p.unresolved).map((p) => p.label);
  if (unresolvedNames.length > 0) return { kind: "unresolved", components: unresolvedNames };

  const exact = COMPAT.releases.find((r) =>
    parts.every((p) => p.version === releaseVersion(r, p.key)),
  );
  if (exact) return { kind: "compatible", release: exact };

  // Best-scoring release (newest on ties - releases are newest-first).
  let best: CompatRelease | null = null;
  let bestScore = -1;
  for (const r of COMPAT.releases) {
    const score = parts.filter((p) => p.version === releaseVersion(r, p.key)).length;
    if (score > bestScore) {
      bestScore = score;
      best = r;
    }
  }
  const offenders = parts
    .filter((p) => p.version !== releaseVersion(best!, p.key))
    .map((p) => ({
      component: p.label,
      have: p.version!,
      want: releaseVersion(best!, p.key),
    }));
  return { kind: "mismatched", best: best!, offenders };
}
