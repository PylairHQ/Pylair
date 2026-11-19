/** macOS ad-hoc signing of the built .app, verified with codesign. Runs
 * only on macOS hosts; see build() for when it is used. */

import { spawnSync } from "node:child_process";
import {
  closeSync,
  existsSync,
  mkdtempSync,
  openSync,
  readdirSync,
  readSync,
  rmSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { ui } from "../ui/index.js";
import { run } from "./util.js";

// ── macOS ad-hoc signing ──────────────────────────────────────────────
// Mach-O magic numbers, read as the first BE u32 — includes the
// byte-swapped forms and both fat headers.
const MACHO_MAGIC = new Set([
  0xfeedface, 0xfeedfacf, 0xcefaedfe, 0xcffaedfe,
  0xcafebabe, 0xbebafeca, 0xcafebabf, 0xbfbafeca,
]);

function isMachO(p: string): boolean {
  const fd = openSync(p, "r");
  try {
    const b = Buffer.alloc(4);
    return readSync(fd, b, 0, 4, 0) === 4 && MACHO_MAGIC.has(b.readUInt32BE(0));
  } finally {
    closeSync(fd);
  }
}

/** Re-sign a built .app ad-hoc and REQUIRE it to verify. With no signing
 * identity Tauri ships only the linker's ad-hoc stub on the main binary —
 * a signature that doesn't cover the bundle is the worst state for
 * Gatekeeper ("is damaged"), worse than none at all. Signs every Mach-O
 * nested-first rather than `codesign --deep`, which is documented to miss
 * binaries in layouts it doesn't model — a vendored CPython tree is
 * exactly that. Verify with --deep --strict; if it fails, the build
 * fails — a broken signature must not ship silently.
 */
function codesignVerify(path: string): string | null {
  const v = spawnSync("codesign", ["--verify", "--deep", "--strict", path], {
    encoding: "utf-8",
  });
  return v.status === 0 ? null : (v.stderr ?? "").trim();
}

export function adHocSign(tauriDir: string, productName: string): void {
  const bundleDir = join(tauriDir, "target", "release", "bundle", "macos");
  if (!existsSync(bundleDir)) return; // --bundles skipped app — nothing to sign
  const app = readdirSync(bundleDir).find((n) => n.endsWith(".app"));
  if (!app) return;
  const appPath = join(bundleDir, app);

  const machos: string[] = [];
  const walk = (dir: string): void => {
    for (const e of readdirSync(dir, { withFileTypes: true })) {
      const p = join(dir, e.name);
      // Dirent types describe the link, not the target — symlinked files
      // and dirs are skipped; the real binary is signed in place.
      if (e.isDirectory()) walk(p);
      else if (e.isFile() && isMachO(p)) machos.push(p);
    }
  };
  walk(appPath);
  // Deepest first — a parent's signature seals its children, so children
  // must be final when the parent is written.
  machos.sort((a, b) => b.split("/").length - a.split("/").length);
  for (const p of machos) run("codesign", ["--force", "--sign", "-", p], appPath);
  run("codesign", ["--force", "--sign", "-", appPath], bundleDir);

  const err = codesignVerify(appPath);
  if (err) throw new Error(`ad-hoc signing produced an invalid bundle:\n${err}`);
  ui.status(`ad-hoc signed ${app}: ${machos.length} nested binaries, codesign --verify clean`);

  // A dmg built in the same `tauri build` already captured the
  // PRE-signing .app — signing after bundling can't reach inside a
  // compressed image. Repackage each dmg from the signed app and verify
  // the shipped artifact itself, not just the staged copy. The rebuilt
  // image is plain (app + /Applications link) — Tauri's cosmetic layout
  // returns once real signing is configured and repackaging stops.
  const dmgDir = join(tauriDir, "target", "release", "bundle", "dmg");
  if (!existsSync(dmgDir)) return;
  for (const dmg of readdirSync(dmgDir).filter((n) => n.endsWith(".dmg"))) {
    const dmgPath = join(dmgDir, dmg);
    const staging = mkdtempSync(join(tmpdir(), "pylair-dmg-"));
    try {
      run("ditto", [appPath, join(staging, app)], tauriDir);
      run("ln", ["-s", "/Applications", join(staging, "Applications")], tauriDir);
      run(
        "hdiutil",
        ["create", "-volname", productName, "-srcfolder", staging, "-ov", "-format", "UDZO", dmgPath],
        tauriDir,
      );
    } finally {
      rmSync(staging, { recursive: true, force: true });
    }

    // Verify the app INSIDE the shipped dmg — mount read-only at a fixed
    // point, verify, always detach, then judge.
    const mount = mkdtempSync(join(tmpdir(), "pylair-mnt-"));
    run("hdiutil", ["attach", "-nobrowse", "-readonly", "-mountpoint", mount, dmgPath], tauriDir);
    let inDmg: string | null = null;
    try {
      inDmg = codesignVerify(join(mount, app));
    } finally {
      spawnSync("hdiutil", ["detach", mount, "-quiet"], { stdio: "inherit" });
      rmSync(mount, { recursive: true, force: true });
    }
    if (inDmg) throw new Error(`${dmg} contains an invalidly-signed app:\n${inDmg}`);
    ui.status(`repackaged ${dmg} with the signed app, verified inside the image`);
  }
}
