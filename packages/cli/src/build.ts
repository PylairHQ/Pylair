/** `pylair build` — the M0 recipe, generalized:
 *   fetch standalone CPython → stage site-packages via the bundled
 *   interpreter (native wheels only — no cross-compile) → stage app
 *   source → prune → tauri build → unsigned warning.
 */

import { createHash } from "node:crypto";
import { execFileSync, spawnSync } from "node:child_process";
import {
  closeSync,
  cpSync,
  createWriteStream,
  existsSync,
  mkdirSync,
  mkdtempSync,
  openSync,
  readdirSync,
  readFileSync,
  readSync,
  renameSync,
  rmSync,
  statSync,
  writeFileSync,
} from "node:fs";
import type { IncomingMessage } from "node:http";
import { get } from "node:https";
import { arch, platform, tmpdir } from "node:os";
import { basename, join, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import {
  importRoot,
  loadConfig,
  loadTauriConfig,
  tauriCliEntry,
  windowsSigningConfigured,
} from "./config.js";
import { matchGlob } from "./glob.js";
import { DEFAULT_PRUNE, prune } from "./prune.js";
import { x as untar } from "tar";
import { compileBytecode } from "./build/bytecode.js";
import { ensureTarball } from "./build/download.js";
import { PBS_SHA256, PBS_TAG, PBS_VERSIONS, hostTriple } from "./build/pbs.js";
import { fetchPython, stageEnv } from "./build/python.js";
import { isWin, run } from "./build/util.js";

export { compileBytecode, PYC_INVALIDATION_MODE } from "./build/bytecode.js";
export {
  DownloadError,
  backoffMs,
  download,
  downloadTimeoutMs,
  downloadWithRetry,
  ensureTarball,
} from "./build/download.js";
export { hostTriple } from "./build/pbs.js";
export { requirementsHaveHashes, uvExportArgs, uvPipInstallArgs } from "./build/python.js";

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

function adHocSign(tauriDir: string, productName: string): void {
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
  console.log(`ad-hoc signed ${app} — ${machos.length} nested binaries, codesign --verify clean`);

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
    console.log(`repackaged ${dmg} with the signed app — verified inside the image`);
  }
}

/** Shipped bundles must carry the licenses of what they embed. Collects:
 *  pylair's own dual license, the interpreter's license tree (PSF license
 *  requires LICENSE text with redistributed CPython), and each wheel's
 *  dist-info license files. Runs before prune so nothing collected is
 *  deleted afterwards. Returns the notice dir.
 */
export function stageNotices(resources: string, pylairDir: string): string {
  const out = join(resources, "THIRD-PARTY-NOTICES");
  rmSync(out, { recursive: true, force: true });
  mkdirSync(out, { recursive: true });
  const index: string[] = [];
  const copyInto = (subdir: string, src: string, dest: string) => {
    mkdirSync(join(out, subdir), { recursive: true });
    cpSync(src, join(out, subdir, dest));
  };
  const LICENSEISH = /^(licen[cs]e|copying|notice|authors|copyright)/i;

  for (const f of ["LICENSE-MIT", "LICENSE-APACHE"]) {
    const src = join(pylairDir, f);
    if (existsSync(src)) copyInto("pylair", src, f);
  }
  index.push("pylair/ — pylair itself (MIT OR Apache-2.0)");

  // The interpreter: python-build-standalone ships a licenses/ dir holding
  // CPython's license plus every statically-linked dep (OpenSSL, libffi…).
  const pyRoot = join(resources, "python");
  let found = 0;
  const walkPy = (dir: string) => {
    for (const e of readdirSync(dir, { withFileTypes: true })) {
      const p = join(dir, e.name);
      if (e.isDirectory()) {
        if (e.name.toLowerCase() === "licenses") {
          cpSync(p, join(out, "python", "licenses"), { recursive: true });
          found++;
        } else walkPy(p);
      } else if (LICENSEISH.test(e.name)) {
        copyInto("python", p, e.name);
        found++;
      }
    }
  };
  if (existsSync(pyRoot)) walkPy(pyRoot);
  if (!found) {
    mkdirSync(join(out, "python"), { recursive: true });
    writeFileSync(
      join(out, "python", "NOTICE.txt"),
      "This app bundles CPython (PSF-2.0 license) via python-build-standalone.\n" +
        "License texts were not located in the interpreter tree at build time;\n" +
        "they ship upstream in the python/licenses directory of each\n" +
        "python-build-standalone release.\n",
    );
    console.warn("warning: no license files found under resources/python — wrote placeholder NOTICE");
  }
  index.push("python/ — bundled CPython interpreter (PSF-2.0) and its vendored deps");

  // Wheels: every installed dist-info/egg-info carries license files or
  // license metadata. Copy whichever exists.
  const sp = join(resources, "site-packages");
  let wheels = 0;
  if (existsSync(sp)) {
    for (const e of readdirSync(sp, { withFileTypes: true })) {
      if (!e.isDirectory()) continue;
      const m = /^(.*?)-[\d.]+.*\.(dist-info|egg-info)$/.exec(e.name);
      if (!m) continue;
      const pkg = m[1].replace(/[^\w.-]/g, "_");
      const d = join(sp, e.name);
      let kept = false;
      for (const f of readdirSync(d, { withFileTypes: true })) {
        if (f.isDirectory() && f.name.toLowerCase() === "licenses") {
          cpSync(join(d, f.name), join(out, pkg, "licenses"), { recursive: true });
          kept = true;
        } else if (f.isFile() && LICENSEISH.test(f.name)) {
          copyInto(pkg, join(d, f.name), f.name);
          kept = true;
        }
      }
      const meta = join(d, "METADATA");
      if (existsSync(meta)) {
        const lines = readFileSync(meta, "utf-8")
          .split("\n")
          .filter((l) => /^(License|Classifier: License)/i.test(l));
        // Copyleft is legal to ship but app authors deserve to know they
        // bundled it — warn, never block.
        if (/\b(a?lgpl|gpl)[ -]?v?\d/i.test(lines.join(" ")))
          console.warn(`warning: bundled wheel ${e.name} declares a copyleft license`);
        if (lines.length) {
          mkdirSync(join(out, pkg), { recursive: true });
          writeFileSync(join(out, pkg, "METADATA-LICENSE.txt"), lines.join("\n") + "\n");
          kept = true;
        }
      }
      if (kept) {
        wheels++;
        index.push(`${pkg}/ — from ${e.name}`);
      }
    }
  }
  writeFileSync(join(out, "INDEX.txt"), index.join("\n") + "\n");
  console.log(`third-party notices: pylair, python interpreter, ${wheels} wheel(s)`);
  return out;
}

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
): string[] {
  const pats = include.map((g) => g.replace(/\\/g, "/").replace(/^\.\//, ""));
  const skipped: string[] = [];
  cpSync(srcDir, dest, {
    recursive: true,
    filter: (src) => {
      const rel = relative(srcDir, src).replace(/\\/g, "/");
      if (!rel) return true; // the root itself
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

export interface BuildOptions {
  appDir: string;
  /** Extra tauri-cli args (e.g. --bundles). */
  tauriArgs?: string[];
  /** Skip the interpreter fetch (reuse staged resources/python). */
  skipFetch?: boolean;
}

export async function build(opts: BuildOptions): Promise<void> {
  const { appDir } = opts;
  const tauriDir = join(appDir, "src-tauri");
  const resources = join(tauriDir, "resources");
  const tconf = loadTauriConfig(appDir);

  const cfg = loadConfig(appDir);

  const targetTriple = hostTriple(); // cross-compile refused by construction
  console.log(
    `building ${tconf.productName} ${tconf.version} for ${platform()}/${arch()} (${targetTriple})`,
  );
  console.log("note: pylair builds native bundles only — run this on each target OS");

  const cacheDir = join(appDir, ".pylair", "cache");
  const bundled = opts.skipFetch
    ? join(resources, "python", isWin ? "python.exe" : "bin/python3")
    : await fetchPython(resources, cacheDir, cfg.pythonVersion);

  stageEnv(resources, bundled, appDir, join(appDir, cfg.requirements), cacheDir);
  // Licenses ship with the bundle — collect before prune can remove sources.
  stageNotices(resources, fileURLToPath(new URL("..", import.meta.url)));

  // Stage app source so resources/ mirrors the installed layout.
  const srcDir = join(appDir, cfg.source);
  if (!existsSync(srcDir)) throw new Error(`source dir ${srcDir} missing`);
  rmSync(join(resources, cfg.source), { recursive: true, force: true });
  stageSource(srcDir, join(resources, cfg.source), cfg.include);

  const res = prune(resources, [...DEFAULT_PRUNE, ...cfg.prune]);
  const mib = (res.bytesFreed / 2 ** 20).toFixed(1);
  console.log(`pruned ${res.removed} paths (${mib} MiB) from staged resources`);

  // Precompile what survives the prune — read-only installs cannot
  // write pycs at launch, so the bundle ships them ready-made.
  compileBytecode(resources, bundled, ["site-packages", cfg.source]);

  // Over-pruning must fail at build time, not on the user's machine:
  // import the app module with the staged tree exactly as the bundle sees it.
  const sep = isWin ? ";" : ":";
  // The import root is the PARENT of the staged source dir ("backend.app"
  // resolves from resources/, not resources/backend/). Run from an unrelated
  // cwd so the project checkout can't satisfy the import by accident.
  // PYTHONDONTWRITEBYTECODE: the check exercises the compiled pycs rather
  // than writing a second, non-deterministic set into the staged tree.
  const check = spawnSync(
    bundled,
    ["-c", `import ${cfg.appModule}; print("staged import ok")`],
    {
      encoding: "utf-8",
      cwd: tmpdir(),
      env: {
        ...process.env,
        PYTHONPATH: `${importRoot(resources, cfg.source)}${sep}${join(resources, "site-packages")}`,
        PYTHONNOUSERSITE: "1",
        PYTHONDONTWRITEBYTECODE: "1",
      },
    },
  );
  if (check.status !== 0)
    throw new Error(
      `staged bundle failed the import check (${cfg.appModule}) — ` +
        `pruning or missing deps? ${check.stderr?.trim()}`,
    );

  // ── tauri build ────────────────────────────────────────────────────
  // Signing is per-platform: only macOS creds (env OR tauri.conf
  // signingIdentity) mean this Mac build gets a real signature — a Windows
  // thumbprint on its own must not suppress the ad-hoc pass.
  // NOTE: TAURI_SIGNING_PRIVATE_KEY signs *updater* artifacts, not the app —
  // it must NOT silence the unsigned warning.
  const appleSigned = !!(
    process.env.APPLE_SIGNING_IDENTITY ||
    process.env.APPLE_CERTIFICATE ||
    tconf.macSigningIdentity
  );
  const hasSigning =
    platform() === "darwin"
      ? appleSigned
      : platform() === "win32"
        ? windowsSigningConfigured(appDir)
        : true; // no signing concept on Linux — don't warn
  run(process.execPath, [tauriCliEntry(appDir), "build", ...(opts.tauriArgs ?? [])], appDir);
  // Unsigned macOS builds get a full-bundle ad-hoc signature — better
  // than the linker stub Tauri leaves on just the main binary. Skipped
  // entirely when a real identity is configured: ad-hoc must never run
  // over a proper signature.
  if (platform() === "darwin" && !appleSigned) adHocSign(tauriDir, tconf.productName);
  if (!hasSigning) {
    console.warn(`
╔══════════════════════════════════════════════════════════════════╗
║  UNSIGNED BUILD — this bundle is not code-signed or notarized.     ║
║  macOS Gatekeeper and Windows SmartScreen will warn users.         ║
║  To sign: set APPLE_SIGNING_IDENTITY / TAURI_SIGNING_PRIVATE_KEY / ║
║  WINDOWS_CERTIFICATE_THUMBPRINT (see docs/build.md).               ║
╚══════════════════════════════════════════════════════════════════╝`);
  }
}
