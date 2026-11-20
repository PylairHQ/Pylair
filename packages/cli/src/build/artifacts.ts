/** Which installers `tauri build` produced, for the build summary. */

import { existsSync, readdirSync, statSync } from "node:fs";
import { join } from "node:path";

export interface Artifact {
  path: string;
  bytes: number;
}

/** File kinds a user installs or ships. Signature files, updater archives and
 * Tauri's intermediate folders are left out. */
const INSTALLER = /\.(msi|exe|dmg|appimage|deb|rpm|app)$/i;

function sizeOf(p: string): number {
  const st = statSync(p);
  if (!st.isDirectory()) return st.size;
  let total = 0;
  for (const e of readdirSync(p, { withFileTypes: true })) {
    const c = join(p, e.name);
    try {
      total += e.isDirectory() ? sizeOf(c) : e.isSymbolicLink() ? 0 : statSync(c).size;
    } catch {
      /* vanished or unreadable: not worth failing a summary over */
    }
  }
  return total;
}

/** The installers under `<target>/<profile>/bundle/<kind>/` written at or
 * after `since` (so a stale one from an earlier build is never reported).
 * Looks in `CARGO_TARGET_DIR` when set, else `src-tauri/target`, directly and
 * one level down (a `--target <triple>` build). */
export function findArtifacts(
  tauriDir: string,
  since: number,
  o: { debug?: boolean; env?: NodeJS.ProcessEnv } = {},
): Artifact[] {
  const env = o.env ?? process.env;
  const target = env.CARGO_TARGET_DIR ? env.CARGO_TARGET_DIR : join(tauriDir, "target");
  const profile = o.debug ? "debug" : "release";
  const roots = [join(target, profile, "bundle")];
  if (existsSync(target))
    for (const e of readdirSync(target, { withFileTypes: true }))
      if (e.isDirectory() && e.name !== "release" && e.name !== "debug")
        roots.push(join(target, e.name, profile, "bundle"));

  const found: Artifact[] = [];
  for (const root of roots) {
    if (!existsSync(root)) continue;
    for (const kind of readdirSync(root, { withFileTypes: true })) {
      if (!kind.isDirectory()) continue;
      const dir = join(root, kind.name);
      for (const f of readdirSync(dir)) {
        if (!INSTALLER.test(f)) continue;
        const p = join(dir, f);
        try {
          if (statSync(p).mtimeMs + 2000 < since) continue; // 2s: coarse filesystem clocks
          found.push({ path: p, bytes: sizeOf(p) });
        } catch {
          /* vanished between the listing and the stat */
        }
      }
    }
  }
  return found.sort((a, b) => a.path.localeCompare(b.path));
}
