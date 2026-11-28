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
import { isMachOBytes } from "./units.js";

// ── hdiutil with retries ──────────────────────────────────────────────
// hdiutil intermittently fails when a system process holds the image
// (Spotlight, XProtect, a leftover mount): "Resource busy" on create,
// "Resource temporarily unavailable" on attach. Those messages retry;
// anything else fails at once, as run() did.
const HDIUTIL_TRANSIENT = /resource busy|resource temporarily unavailable/i;
const HDIUTIL_ATTEMPTS = 5;

/** The pieces of `runHdiutil` a test stubs. */
export interface HdiutilOps {
  /** The command's exit code and combined output. */
  run: (args: string[]) => { code: number | null; out: string };
  warn: (msg: string) => void;
  sleep: (ms: number) => void;
}

const hdiutilReal: HdiutilOps = {
  run: (args) => {
    const r = spawnSync("hdiutil", args, { encoding: "utf-8" });
    return { code: r.status, out: `${r.stdout ?? ""}${r.stderr ?? ""}` };
  },
  warn: (s) => ui.warn(s),
  sleep: (ms) => Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, ms),
};

/** Where `image` is mounted right now, per `hdiutil info`. */
function mountsOfImage(infoOut: string, image: string): string[] {
  const mounts: string[] = [];
  let forImage = false;
  for (const line of infoOut.split(/\r?\n/)) {
    if (/^(image-path|image-alias)\s*:/.test(line)) {
      forImage = line.slice(line.indexOf(":") + 1).trim() === image;
    } else if (forImage && /^mount-point\s*:/.test(line)) {
      const m = line.slice(line.indexOf(":") + 1).trim();
      if (m) mounts.push(m);
    }
  }
  return mounts;
}

/** Before an attach retry: drop mounts of the same image that a failed
 * attach left behind, plus anything at our mountpoint. Best-effort. */
function detachStaleMounts(o: HdiutilOps, image: string, mountpoint: string): void {
  const stale = new Set<string>([mountpoint]);
  const info = o.run(["info"]);
  if (info.code === 0) for (const m of mountsOfImage(info.out, image)) stale.add(m);
  for (const m of stale) o.run(["detach", m, "-force", "-quiet"]);
}

/** Run hdiutil, retrying only the transient lock messages: 5 attempts,
 * waits of 1, 2, 4, 8 s between them, one warning line per retry. For
 * attach, stale mounts of the image are detached before each retry. */
export function runHdiutil(o: HdiutilOps, args: string[]): void {
  const isAttach = args[0] === "attach";
  const image = isAttach ? args[args.length - 1] : "";
  const mount = isAttach ? args[args.indexOf("-mountpoint") + 1] : "";
  for (let attempt = 1; attempt <= HDIUTIL_ATTEMPTS; attempt++) {
    const r = o.run(args);
    if (r.code === 0) return;
    const out = r.out.trim();
    if (!HDIUTIL_TRANSIENT.test(out) || attempt === HDIUTIL_ATTEMPTS)
      throw new Error(`hdiutil ${args[0]} exited ${r.code}${out ? `:\n${out}` : ""}`);
    if (isAttach) detachStaleMounts(o, image, mount);
    const wait = 2 ** (attempt - 1); // 1, 2, 4, 8 s
    o.warn(`hdiutil ${args[0]} failed transiently; retrying in ${wait}s (attempt ${attempt + 1}/${HDIUTIL_ATTEMPTS})`);
    o.sleep(wait * 1000);
  }
}

// ── macOS ad-hoc signing ──────────────────────────────────────────────
function isMachO(p: string): boolean {
  const fd = openSync(p, "r");
  try {
    const b = Buffer.alloc(8);
    const n = readSync(fd, b, 0, 8, 0);
    return isMachOBytes(n === 8 ? b : b.subarray(0, n));
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
      runHdiutil(hdiutilReal, [
        "create",
        "-volname",
        productName,
        "-srcfolder",
        staging,
        "-ov",
        "-format",
        "UDZO",
        dmgPath,
      ]);
    } finally {
      rmSync(staging, { recursive: true, force: true });
    }

    // Verify the app INSIDE the shipped dmg — mount read-only at a fixed
    // point, verify, always detach, then judge.
    const mount = mkdtempSync(join(tmpdir(), "pylair-mnt-"));
    runHdiutil(hdiutilReal, ["attach", "-nobrowse", "-readonly", "-mountpoint", mount, dmgPath]);
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

// ── staged external-binary signing (0014 sec. 2) ─────────────────────
// Rewriting a unit invalidates upstream signatures, and an unsigned nested
// dylib can fail to load even in an otherwise unsigned development build.
// Signing therefore happens at staging time, before Tauri copies the files
// into the bundle; the final bundle pass seals everything afterwards.

function codeFiles(root: string, test: (p: string) => boolean): string[] {
  const out: string[] = [];
  const walk = (dir: string): void => {
    if (!existsSync(dir)) return;
    for (const e of readdirSync(dir, { withFileTypes: true })) {
      const p = join(dir, e.name);
      if (e.isDirectory()) walk(p);
      else if (e.isFile() && test(p)) out.push(p);
    }
  };
  walk(root);
  return out;
}

function stagedCodeRoots(tauriDir: string): string[] {
  return [join(tauriDir, "binaries"), join(tauriDir, "resources", "binaries")];
}

function machOSharedObject(p: string): boolean {
  const r = spawnSync("otool", ["-D", p], { encoding: "utf-8" });
  if (r.status !== 0) return p.endsWith(".dylib");
  return r.stdout.split(/\r?\n/).slice(1).some((line) => line.trim().length > 0);
}

export function macCodesignArgs(file: string, identity?: string): string[] {
  const args = ["--force", "--sign", identity ?? "-"];
  if (identity) args.push("--options", "runtime", "--timestamp");
  args.push(file);
  return args;
}

/** Sign every Mach-O external binary and unit member. Shared objects are
 * signed innermost-first before executables, matching the bundle's signing
 * order. Real identities get the hardened runtime; unsigned builds still
 * get ad-hoc signatures because newer macOS can refuse unsigned nested
 * dylibs even when the app as a whole is unsigned. */
export function signStagedMacBinaries(tauriDir: string, identity?: string): number {
  const machos = stagedCodeRoots(tauriDir).flatMap((root) => codeFiles(root, isMachO));
  const depth = (p: string): number => p.split(/[\/]/).length;
  const libs = machos.filter(machOSharedObject).sort((a, b) => depth(b) - depth(a));
  const exes = machos.filter((p) => !libs.includes(p)).sort((a, b) => depth(b) - depth(a));
  const signed = [...libs, ...exes];
  for (const p of signed) run("codesign", macCodesignArgs(p, identity), tauriDir);
  return signed.length;
}

/** Render a configured Windows sign command. `%1` is the file placeholder,
 * matching Tauri's bundle.windows.signCommand convention. */
export function windowsSignCommandLine(template: string, file: string): string {
  const quoted = `"${file}"`;
  return template.includes("%1") ? template.replaceAll("%1", quoted) : `${template} ${quoted}`;
}

function findOnPath(exe: string): string | undefined {
  const pathExt = (process.env.PATHEXT ?? ".EXE;.BAT;.CMD").split(";");
  for (const dir of (process.env.PATH ?? "").split(/[;,]/).filter(Boolean)) {
    for (const ext of pathExt) {
      const p = join(dir, exe + ext.toLowerCase());
      if (existsSync(p)) return p;
      const upper = join(dir, exe + ext.toUpperCase());
      if (existsSync(upper)) return upper;
    }
  }
  return undefined;
}

/** Locate signtool.exe: PATH first, then the newest installed Windows Kit. */
export function findSigntool(): string | undefined {
  const onPath = findOnPath("signtool");
  if (onPath) return onPath;
  const roots = [
    join(process.env["ProgramFiles(x86)"] ?? "C:\Program Files (x86)", "Windows Kits", "10", "bin"),
    join(process.env.ProgramFiles ?? "C:\Program Files", "Windows Kits", "10", "bin"),
  ];
  for (const root of roots) {
    if (!existsSync(root)) continue;
    const versions = readdirSync(root, { withFileTypes: true })
      .filter((e) => e.isDirectory() && /^10\./.test(e.name))
      .map((e) => e.name)
      .sort()
      .reverse();
    for (const version of versions) {
      const p = join(root, version, "x64", "signtool.exe");
      if (existsSync(p)) return p;
    }
  }
  return undefined;
}

export interface WindowsSigningOptions {
  certificateThumbprint?: string;
  signCommand?: string;
}

/** Authenticode-sign every .dll and .exe staged from external binaries:
 * single-file sidecars plus every member of every unit. Linux signs nothing
 * and Windows needs no loader rewriting because its loader searches the
 * executable's directory first. */
export function signStagedWindowsBinaries(
  tauriDir: string,
  signing: WindowsSigningOptions,
): number {
  if (!signing.certificateThumbprint && !signing.signCommand) return 0;
  const targets = stagedCodeRoots(tauriDir).flatMap((root) =>
    codeFiles(root, (p) => /\.(dll|exe)$/i.test(p)),
  );
  if (!targets.length) return 0;
  for (const file of targets) {
    if (signing.signCommand) {
      const r = spawnSync(windowsSignCommandLine(signing.signCommand, file), {
        cwd: tauriDir,
        shell: true,
        encoding: "utf-8",
      });
      if (r.status !== 0)
        throw new Error(
          `windows signCommand failed for ${file} (exit ${r.status}): ${`${r.stdout ?? ""}${r.stderr ?? ""}`.trim()}`,
        );
      continue;
    }
    const signtool = findSigntool();
    if (!signtool)
      throw new Error(
        "signtool.exe not found - install the Windows SDK signing tools or configure bundle.windows.signCommand",
      );
    const r = spawnSync(
      signtool,
      [
        "sign",
        "/sha1",
        signing.certificateThumbprint!,
        "/fd",
        "sha256",
        "/td",
        "sha256",
        "/tr",
        "http://timestamp.digicert.com",
        file,
      ],
      { cwd: tauriDir, encoding: "utf-8" },
    );
    if (r.status !== 0)
      throw new Error(`signtool failed for ${file} (exit ${r.status}): ${`${r.stdout ?? ""}${r.stderr ?? ""}`.trim()}`);
  }
  return targets.length;
}
