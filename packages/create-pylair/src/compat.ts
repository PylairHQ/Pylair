/** The compatibility manifest (src/compat.json): which npm/crate/pylair
 * versions form a working set, per npm release tag, plus the manual
 * upgrade notes a user hitting that release must apply. One source of
 * truth - create-pylair vendors this file byte-identically (sync-ui.mjs)
 * and derives its scaffold pins from it, so the release-bump PR edits
 * exactly this file.
 *
 * Version numbers move independently across the three ecosystems (npm
 * 0.1.0-rc.32 pairs with crate 0.1.0-rc.25 and pylair 0.1.0rc18) - that
 * is expected, not skew. */

import data from "./compat.json" with { type: "json" };

export interface CompatRelease {
  /** The npm release tag - both @pylair/client and @pylair/cli. */
  npm: string;
  /** tauri-plugin-pylair on crates.io. */
  crate: string;
  /** pylair on PyPI. */
  pylair: string;
  date: string;
  /** Manual steps a user upgrading through this release must apply. */
  notes: string[];
}

export interface CompatManifest {
  /** The newest npm release tag this manifest knows. */
  current: string;
  /** Newest first. */
  releases: CompatRelease[];
}

export const COMPAT: CompatManifest = data as CompatManifest;

export function currentRelease(): CompatRelease {
  const r = COMPAT.releases.find((r) => r.npm === COMPAT.current);
  if (!r) throw new Error(`compat.json: current ${COMPAT.current} has no release entry`);
  return r;
}

/** Every release strictly newer than `npm` (oldest first - the order a
 * user upgrading applies them). Unknown tags yield everything, which is
 * the safe answer for "what did I miss". */
export function releasesAfter(npm: string): CompatRelease[] {
  const idx = COMPAT.releases.findIndex((r) => r.npm === npm);
  const slice = idx === -1 ? COMPAT.releases : COMPAT.releases.slice(0, idx);
  return [...slice].reverse();
}
