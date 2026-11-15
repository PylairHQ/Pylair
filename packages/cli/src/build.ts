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
import { stageNotices } from "./build/notices.js";
import { fetchPython, stageEnv } from "./build/python.js";
import { adHocSign } from "./build/sign.js";
import { stageSource } from "./build/source.js";
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
export { stageNotices } from "./build/notices.js";
export { hostTriple } from "./build/pbs.js";
export { stageSource } from "./build/source.js";
export { requirementsHaveHashes, uvExportArgs, uvPipInstallArgs } from "./build/python.js";

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
