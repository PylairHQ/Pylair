/** Staging the app source into the bundle, minus anything that looks
 * like a secret unless `[tool.pylair] include` names it. */

import { cpSync } from "node:fs";
import { relative } from "node:path";
import { matchGlob } from "../glob.js";

/** Basename globs for files that look like secrets or credentials —
 * staged source must never carry them into an installer. */
const SECRET_PATTERNS = [
  "*.pem",
  "*.key",
  "*.p12",
  "*.pfx",
  "id_rsa*",
  "id_ed25519*",
  "credentials*.json",
  "service-account*.json",
];

/** True when a `/`-separated relative path looks like it holds secrets:
 * any dot-segment (.env, .git, .venv, .DS_Store) or a basename matching
 * the credential patterns. */
function secretish(rel: string): boolean {
  const segs = rel.split("/");
  if (segs.some((s) => s.startsWith("."))) return true;
  return SECRET_PATTERNS.some((g) => matchGlob(g, segs[segs.length - 1]));
}

/** Could `pattern` match something beneath `dir`? Refusing a denied dir
 * would otherwise hide an included descendant from cpSync's walk. */
function couldMatchUnder(pattern: string, dir: string): boolean {
  const d = dir.split("/");
  const p = pattern.split("/");
  for (let i = 0; i < d.length; i++) {
    if (i >= p.length) return false; // pattern ends above the dir
    if (p[i] === "**") return true; // ** crosses any depth
    if (/[*?]/.test(p[i])) {
      if (!matchGlob(p[i], d[i])) return false;
    } else if (p[i] !== d[i]) return false;
  }
  return true;
}

/**
 * Copy the app source tree into the bundle, minus anything that looks
 * like a secret. A path is skipped when any segment starts with "." or
 * its basename matches SECRET_PATTERNS, UNLESS an `include` glob
 * (relative to srcDir, same shape as `prune` entries) names it — an
 * include wins even inside a denied directory, which stays walkable so
 * the descent can reach it. Skips are reported, not fatal: `warn` gets
 * the count, the first 10 names, and how to opt a path back in; nothing
 * is printed when nothing was skipped.
 *
 * Returns the `/`-separated relative paths that were refused.
 */
export function stageSource(
  srcDir: string,
  dest: string,
  include: string[] = [],
  warn: (msg: string) => void = console.warn,
  /** Top-level names to leave out entirely (a root source dir holding the
   * project's own `src-tauri`, `node_modules`...). Not reported as skipped. */
  exclude: string[] = [],
): string[] {
  const pats = include.map((g) => g.replace(/\\/g, "/").replace(/^\.\//, ""));
  const skipped: string[] = [];
  cpSync(srcDir, dest, {
    recursive: true,
    filter: (src) => {
      const rel = relative(srcDir, src).replace(/\\/g, "/");
      if (!rel) return true; // the root itself
      if (exclude.includes(rel.split("/")[0])) return false;
      if (pats.some((g) => matchGlob(g, rel))) return true;
      if (!secretish(rel)) return true;
      if (pats.some((g) => couldMatchUnder(g, rel))) return true; // descend only
      skipped.push(rel);
      return false;
    },
  });
  if (skipped.length) {
    warn(
      `warning: skipped ${skipped.length} secret-looking path(s) under ${srcDir}: ` +
        skipped.slice(0, 10).join(", ") +
        (skipped.length > 10 ? ", …" : ""),
    );
    warn(`  to ship one deliberately, list it in [tool.pylair] include = ["path/under/source"]`);
  }
  return skipped;
}
