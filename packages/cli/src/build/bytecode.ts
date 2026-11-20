/** Precompiled bytecode for the staged Python tree (docs/build.md,
 * "Precompiled bytecode"). */

import { spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import { join } from "node:path";
import { ui } from "../ui/index.js";

/** pyc invalidation mode shipped in installers — see docs/build.md.
 * checked-hash: installers rewrite mtimes, so timestamp pycs silently
 * invalidate; hash-based headers survive repackaging. checked (not
 * unchecked) re-validates the source hash per import — ~10ms slower on
 * a numpy-sized tree but a post-install source edit recompiles instead
 * of running stale bytecode. Measured (Windows, numpy tree):
 * ~665ms no-pyc / ~267ms checked / ~256ms unchecked per cold import.
 */
export const PYC_INVALIDATION_MODE = "checked-hash" as const;

/** Precompile the staged .py tree with the BUNDLED interpreter — same
 * CPython and magic number as the runtime that will load the pycs.
 * An installed app (possibly read-only) must not compile its Python on
 * every launch. Runs post-prune so denied dirs stay uncompiled, and
 * relative args + cwd keep co_filename bundle-relative: identical pycs
 * regardless of build path, tracebacks show `site-packages/…`.
 * `-s`/`-d` would also work; relative args are the smallest version of
 * the same thing.
 *
 * Exit 1 from compileall means "some files failed to compile" — a
 * dependency may ship a py2-only file or invalid fixture that nothing
 * imports. That warns (paths returned) instead of failing the build;
 * the files are left uncompiled and will compile — or fail to import —
 * as normal at runtime. Real failures still throw: the interpreter
 * not starting, a signal, or any other exit code. Returns the failed
 * paths (empty on full success). */
export function compileBytecode(
  resources: string,
  python: string,
  dirs: string[],
): string[] {
  const existing = dirs.filter((d) => existsSync(join(resources, d)));
  if (!existing.length) return [];
  const r = spawnSync(
    python,
    [
      "-m",
      "compileall",
      "-q",
      "-f",
      "-j",
      "0",
      "--invalidation-mode",
      PYC_INVALIDATION_MODE,
      ...existing,
    ],
    { cwd: resources, encoding: "utf-8" },
  );
  if (r.error) throw new Error(`${python} -m compileall failed to start: ${r.error.message}`);
  if (r.signal || (r.status !== 0 && r.status !== 1)) {
    throw new Error(
      `${python} -m compileall ${r.signal ? `died on ${r.signal}` : `exited ${r.status}`}`,
    );
  }
  if (r.status === 0) return [];
  // Quiet mode still prints `*** Error compiling '<path>'...` per file.
  const failed = [
    ...String(r.stderr).matchAll(/^\*\*\* Error compiling '([^']+)'/gm),
    ...String(r.stdout).matchAll(/^\*\*\* Error compiling '([^']+)'/gm),
  ].map((m) => m[1].replace(/\\\\/g, "\\")); // repr-escaped on Windows
  const shown = failed.slice(0, 10).join("\n  ");
  ui.warn(
    `warning: ${failed.length} file(s) failed to byte-compile and are left` +
      ` uncompiled; they will compile (or fail to import) as normal at` +
      ` runtime:\n  ${shown}${failed.length > 10 ? `\n  and ${failed.length - 10} more` : ""}`,
  );
  return failed;
}
