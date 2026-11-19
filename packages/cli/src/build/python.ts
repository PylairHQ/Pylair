/** The bundled interpreter and its packages: fetch and unpack CPython,
 * export the lock with uv, and install site-packages through the bundled
 * interpreter (native wheels only). */

import { existsSync, mkdirSync, readFileSync, rmSync } from "node:fs";
import { join } from "node:path";
import { x as untar } from "tar";
import { ensureTarball } from "./download.js";
import { PBS_SHA256, PBS_TAG, PBS_VERSIONS, hostTriple } from "./pbs.js";
import { ui, type StepHandle } from "../ui/index.js";
import { isWin, runTool } from "./util.js";

export async function fetchPython(
  resources: string,
  cacheDir: string,
  versionClass: string,
): Promise<string> {
  const ver = PBS_VERSIONS[versionClass];
  if (!ver)
    throw new Error(
      `python-version "${versionClass}" not available (supported: ${Object.keys(PBS_VERSIONS).join(", ")})`,
    );
  const triple = hostTriple();
  const asset = `cpython-${ver}+${PBS_TAG}-${triple}-install_only_stripped.tar.gz`;
  const base = `https://github.com/astral-sh/python-build-standalone/releases/download/${PBS_TAG}`;
  mkdirSync(resources, { recursive: true });
  mkdirSync(cacheDir, { recursive: true });
  const tarball = join(cacheDir, asset);

  // The interpreter ships to end users — verify against hashes committed
  // in-repo (see PBS_SHA256), not a checksum file from the same release.
  const want = PBS_SHA256[`${ver}-${triple}`];
  if (!want) throw new Error(`no committed SHA256 for ${ver}/${triple}`);
  await ensureTarball(`${base}/${asset}`, tarball, want);

  const dest = join(resources, "python");
  rmSync(dest, { recursive: true, force: true });
  mkdirSync(dest, { recursive: true });
  // Extract in-process — same lesson as the tomllib probe: whichever
  // `tar` PATH resolves to can differ (System32 bsdtar vs Git Bash GNU
  // tar, which dies on drive-letter paths). The `tar` package handles
  // pax long names (>100 chars) and the bin/python3 -> python3.12
  // symlinks the POSIX builds carry.
  await untar({ file: tarball, cwd: dest, strip: 1 });
  const py = join(dest, isWin ? "python.exe" : "bin/python3");
  if (!existsSync(py)) throw new Error(`expected interpreter at ${py}`);
  return py;
}

/** uv export args: the lock's production set only. --no-emit-project
 * drops the scaffold's own project entry — its source is staged by
 * stageSource(), not installed as a package. */
export function uvExportArgs(exported: string): string[] {
  return [
    "export",
    "--frozen",
    "--format",
    "requirements-txt",
    "--no-dev",
    "--no-emit-project",
    "-o",
    exported,
  ];
}

/** True when a requirements file carries any `--hash` entries.
 * `\s` before `--hash` excludes a trailing comment mention. */
export function requirementsHaveHashes(reqsText: string): boolean {
  return /\s--hash=/.test(reqsText);
}

/** `uv pip install` args. `--require-hashes` is passed ONLY for the
 * requirements.txt fallback: an exported lock legitimately mixes hashed
 * registry deps with hash-less `file://` path deps, which the flag
 * would reject — and individual `--hash` entries are verified without
 * it anyway. */
export function uvPipInstallArgs(
  python: string,
  target: string,
  reqsFile: string,
  requireHashes: boolean,
): string[] {
  const args = [
    "pip",
    "install",
    "--python",
    python,
    "--target",
    target,
    "-r",
    reqsFile,
  ];
  if (requireHashes) args.push("--require-hashes");
  return args;
}

export interface StageEnvOptions {
  step?: StepHandle;
  verbose?: boolean;
}

export async function stageEnv(
  resources: string,
  python: string,
  appDir: string,
  reqs: string,
  cacheDir: string,
  o: StageEnvOptions = {},
): Promise<void> {
  const target = join(resources, "site-packages");
  rmSync(target, { recursive: true, force: true });
  // dev uses `uv sync` (uv.lock) — build must install the SAME locked set,
  // hashed, or dev/prod parity is a fiction. requirements.txt is the
  // fallback for projects that don't lock.
  const lock = join(appDir, "uv.lock");
  let reqsFile = reqs;
  let requireHashes = false;
  if (existsSync(lock)) {
    const exported = join(cacheDir, "requirements.exported.txt");
    await runTool(ui, "uv", uvExportArgs(exported), appDir, o);
    reqsFile = exported;
  } else if (!existsSync(reqs)) {
    ui.line(`no uv.lock and no ${reqs}: bundling stdlib only`);
    return;
  } else {
    ui.warn(
      "warning: no uv.lock, building from requirements.txt; " +
        "the bundled env may drift from `pylair dev`'s env",
    );
    requireHashes = requirementsHaveHashes(readFileSync(reqs, "utf-8"));
  }
  await runTool(ui, "uv", uvPipInstallArgs(python, target, reqsFile, requireHashes), appDir, o);
}
