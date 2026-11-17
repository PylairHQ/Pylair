/** Prune staged bundle resources — things an installed app never needs.
 * Defaults are a deny-list of globs relative to the resources root;
 * apps extend or replace it via [tool.pylair] prune / prune-replace.
 */

import { readdirSync, rmSync, statSync } from "node:fs";
import { join } from "node:path";
import { matchGlob } from "./glob.js";

export const DEFAULT_PRUNE = [
  // python-build-standalone dev bits
  "python/include",
  "python/lib/pkgconfig",
  "python/lib/python*/test",
  "python/lib/python*/idlelib",
  "python/lib/python*/tkinter",
  "python/lib/python*/turtledemo",
  "python/lib/python*/pydoc_data",
  "python/lib/python*/ensurepip",
  "python/lib/python*/venv",
  "python/share",
  "python/*.a",
  "python/lib/**/*.a",
  // NOTE: license files are NOT pruned — the PSF license requires
  // LICENSE.txt to ship with redistributed interpreters.
  // site-packages dead weight
  "site-packages/pip",
  "site-packages/pip-*",
  "site-packages/_distutils_hack",
  "site-packages/*/tests",
  "site-packages/*/test",
  // NOT "*/testing": numpy.testing / pandas.testing / torch.testing are
  // imported at runtime by downstream code; pruning them breaks apps.
  // NOT pkg_resources/setuptools: still imported at runtime by many libs.
  "site-packages/**/*.pyc",
  "site-packages/**/__pycache__",
  "site-packages/**/*.dist-info/RECORD.jws",
  // stray caches anywhere
  "backend/**/__pycache__",
  "app/**/__pycache__", // wrap mode stages the app as app/
];

export interface PruneResult {
  removed: number;
  bytesFreed: number;
  hits: { pattern: string; path: string }[];
}

function dirSize(dir: string): number {
  let n = 0;
  for (const e of readdirSync(dir, { withFileTypes: true })) {
    const p = join(dir, e.name);
    n += e.isDirectory() ? dirSize(p) : statSync(p).size;
  }
  return n;
}

/** Walk `root`; delete the *topmost* dir matching any pattern. */
export function prune(root: string, patterns: string[]): PruneResult {
  const res: PruneResult = { removed: 0, bytesFreed: 0, hits: [] };
  const walk = (dir: string, rel: string) => {
    for (const e of readdirSync(dir, { withFileTypes: true })) {
      const p = join(dir, e.name);
      const r = rel ? `${rel}/${e.name}` : e.name;
      const pat = patterns.find((g) => matchGlob(g, r));
      if (pat) {
        res.bytesFreed += e.isDirectory() ? dirSize(p) : statSync(p).size;
        res.hits.push({ pattern: pat, path: r });
        res.removed++;
        rmSync(p, { recursive: true, force: true });
      } else if (e.isDirectory()) {
        walk(p, r);
      }
    }
  };
  walk(root, "");
  return res;
}
