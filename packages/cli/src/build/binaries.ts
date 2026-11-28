/** External-binary staging (decision 0014 sec. 1-3): resolve each declared
 * binary for the target triple from its `path` or a hash-verified cached
 * download. Single files extract `member` out of archives and stage as
 * `src-tauri/binaries/<name>-<triple>[.exe]` for externalBin. Units extract
 * or copy the whole tree under the sec. 2 safety rules into the resources
 * payload as `binaries/<name>/`, rewrite loader paths for the host OS, and
 * both shapes are recorded in the binaries.json manifest. */

import { createHash } from "node:crypto";
import {
  chmodSync,
  existsSync,
  mkdirSync,
  readFileSync,
  renameSync,
  rmSync,
  statSync,
  writeFileSync,
} from "node:fs";
import type { get as httpsGet } from "node:https";
import { platform, tmpdir } from "node:os";
import { basename, join } from "node:path";
import { deflateRawSync, inflateRawSync } from "node:zlib";
import { x as untar, t as listTar } from "tar";
import type { BinaryDecl, BinaryTarget } from "../config.js";
import { hostTriple } from "./pbs.js";
import { downloadWithRetry } from "./download.js";
import {
  copyDirectoryUnit,
  extractUnitArchive,
  markExecutable,
  rewriteUnitLoaders,
} from "./units.js";

const isWin = platform() === "win32";

export class BinaryStageError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "BinaryStageError";
  }
}

const sha256hex = (data: Buffer): string => createHash("sha256").update(data).digest("hex");

/** The installed sidecar name. Tauri's bundler copies the staged file
 * with the `-<target-triple>` suffix string-replaced away
 * (crates/tauri-bundler src/bundle/windows/nsis/mod.rs and the other
 * bundlers share the rule), so `name-<triple>.exe` installs as
 * `name.exe` on Windows targets and `name-<triple>` installs as `name`
 * elsewhere. The manifest path is exactly this. */
export function installedBinaryName(name: string, windows: boolean): string {
  return windows ? `${name}.exe` : name;
}

const tripleIsWindows = (triple: string): boolean => triple.includes("windows");

/** The extraction-safety rules (0014 sec. 2), applied to the member path
 * itself: relative, forward slashes, no `..` segments, no drive letters.
 * Returns the rejection reason or null. */
export function memberPathRejection(member: string): string | null {
  if (!member) return "member is empty";
  if (member.includes("\\")) return `member ${JSON.stringify(member)} must use forward slashes`;
  if (member.startsWith("/")) return `member ${JSON.stringify(member)} is an absolute path`;
  if (/^[A-Za-z]:/.test(member)) return `member ${JSON.stringify(member)} is an absolute path`;
  for (const seg of member.split("/")) {
    if (seg === "..") return `member ${JSON.stringify(member)} contains a .. segment`;
    if (seg === "") return `member ${JSON.stringify(member)} contains an empty segment`;
  }
  return null;
}

/** Minimal zip reader for single-member extraction: central-directory
 * parsing, stored (0) and deflated (8) entries only, no zip64. Single-file
 * member extraction never writes archive paths to disk, so the entire
 * traversal surface is the member lookup itself. */
export function extractZipMember(archive: Buffer, member: string): Buffer {
  // End of central directory: scan the tail for the 0x06054b50 signature.
  let eocd = -1;
  for (let i = archive.length - 22; i >= Math.max(0, archive.length - 22 - 65536); i--) {
    if (archive.readUInt32LE(i) === 0x06054b50) {
      eocd = i;
      break;
    }
  }
  if (eocd < 0) throw new BinaryStageError("zip archive has no end-of-central-directory record");
  const count = archive.readUInt16LE(eocd + 10);
  let off = archive.readUInt32LE(eocd + 16);
  for (let n = 0; n < count; n++) {
    if (archive.readUInt32LE(off) !== 0x02014b50) break;
    const method = archive.readUInt16LE(off + 10);
    const compressed = archive.readUInt32LE(off + 20);
    const uncompressed = archive.readUInt32LE(off + 24);
    const nameLen = archive.readUInt16LE(off + 28);
    const extraLen = archive.readUInt16LE(off + 30);
    const commentLen = archive.readUInt16LE(off + 32);
    const externalAttrs = archive.readUInt32LE(off + 38);
    const localOff = archive.readUInt32LE(off + 42);
    const name = archive.subarray(off + 46, off + 46 + nameLen).toString("utf-8");
    if (name === member) {
      // Unix symlink in zip: mode bits in the high external-attrs word.
      const mode = (externalAttrs >>> 16) & 0xffff;
      if ((mode & 0o170000) === 0o120000)
        throw new BinaryStageError(
          `archive member ${JSON.stringify(member)} is a symlink - single-file extraction stages file contents only`,
        );
      if (compressed === 0xffffffff || uncompressed === 0xffffffff)
        throw new BinaryStageError(`zip64 archives are not supported (member ${JSON.stringify(member)})`);
      const lhNameLen = archive.readUInt16LE(localOff + 26);
      const lhExtraLen = archive.readUInt16LE(localOff + 28);
      const dataOff = localOff + 30 + lhNameLen + lhExtraLen;
      const data = archive.subarray(dataOff, dataOff + compressed);
      if (method === 0) return Buffer.from(data);
      if (method === 8) return inflateRawSync(data);
      throw new BinaryStageError(
        `zip member ${JSON.stringify(member)} uses compression method ${method} - only stored (0) and deflated (8) are supported`,
      );
    }
    off += 46 + nameLen + extraLen + commentLen;
  }
  throw new BinaryStageError(`archive does not contain member ${JSON.stringify(member)}`);
}

/** Extract exactly one member from a tar.gz archive. Links are rejected
 * outright: the staged sidecar is one file's contents, and a link member
 * has none of its own. */
export async function extractTarMember(archive: string, member: string): Promise<Buffer> {
  const dir = join(
    tmpdir(),
    `pylair-member-${process.pid}-${Math.random().toString(36).slice(2)}`,
  );
  mkdirSync(dir, { recursive: true });
  try {
    // List first: typecheck the entry before anything touches disk.
    const entries: { path: string; type: string }[] = [];
    await listTar({
      file: archive,
      onentry: (e) => entries.push({ path: e.path, type: String(e.type) }),
    });
    const hit = entries.find((e) => e.path === member || e.path === `./${member}`);
    if (!hit)
      throw new BinaryStageError(`archive does not contain member ${JSON.stringify(member)}`);
    if (hit.type !== "File")
      throw new BinaryStageError(
        `archive member ${JSON.stringify(member)} is a ${hit.type} - single-file extraction stages regular files only`,
      );
    await untar({ file: archive, cwd: dir, filter: (p) => p === hit.path });
    return readFileSync(join(dir, hit.path));
  } finally {
    try {
      rmSync(dir, { recursive: true, force: true });
    } catch {
      // best-effort temp cleanup
    }
  }
}

/** Extract `member` out of archive bytes, dispatching on magic bytes. */
export async function extractMember(archive: Buffer, member: string): Promise<Buffer> {
  const rejection = memberPathRejection(member);
  if (rejection) throw new BinaryStageError(rejection);
  if (archive.length >= 4 && archive.readUInt32LE(0) === 0x04034b50) return extractZipMember(archive, member);
  if (archive.length >= 2 && archive[0] === 0x1f && archive[1] === 0x8b) {
    const dir = join(
      tmpdir(),
      `pylair-arc-${process.pid}-${Math.random().toString(36).slice(2)}`,
    );
    mkdirSync(dir, { recursive: true });
    const file = join(dir, "archive.tar.gz");
    writeFileSync(file, archive);
    try {
      return await extractTarMember(file, member);
    } finally {
      try {
        rmSync(dir, { recursive: true, force: true });
      } catch {
        // best-effort temp cleanup
      }
    }
  }
  throw new BinaryStageError(
    `archive for member ${JSON.stringify(member)} is neither zip nor tar.gz (magic bytes unrecognized)`,
  );
}

/** The sha256-keyed download cache, shared by `pylair build` and
 * `pylair dev`: <appDir>/.pylair/cache/binaries/<sha256>. A hit IS the
 * hash proof - the file can only be there under its own hash. */
export function binaryCachePath(cacheDir: string, sha256: string): string {
  return join(cacheDir, "binaries", sha256);
}

/** Fetch `url` into the cache (or reuse the hit) and return the cache
 * path. The download lands in `<cache>.part` and renames into place only
 * complete, so a partial fetch never poisons the cache. */
export async function fetchBinaryToCache(
  url: string,
  sha256: string,
  cacheDir: string,
  opts?: { get?: typeof httpsGet },
): Promise<string> {
  const dest = binaryCachePath(cacheDir, sha256);
  if (existsSync(dest)) return dest;
  mkdirSync(join(cacheDir, "binaries"), { recursive: true });
  const tmp = join(cacheDir, "binaries", `${sha256}.download`);
  try {
    await downloadWithRetry(url, tmp, { resume: true, get: opts?.get });
  } catch (e) {
    throw new BinaryStageError(
      `download failed for ${url}: ${(e as Error).message}`,
    );
  }
  const bytes = readFileSync(tmp);
  const got = sha256hex(bytes);
  if (got !== sha256)
    throw new BinaryStageError(
      `hash mismatch for ${url}: declared sha256 ${sha256} but the download hashes to ${got} - ` +
        `refusing to stage bytes that are not what pyproject pins`,
    );
  renameSync(tmp, dest);
  return dest;
}

/** Resolve one binary's target bytes: verify the declared hash of the
 * source (archive or bare file), extract `member` when the entry names
 * one. Returns the executable bytes to stage. Exported for tests. */
export async function resolveBinaryBytes(
  name: string,
  target: BinaryTarget,
  opts: { cacheDir: string; appDir: string; get?: typeof httpsGet },
): Promise<Buffer> {
  let source: Buffer;
  if (target.path) {
    const p = join(opts.appDir, target.path);
    if (!existsSync(p))
      throw new BinaryStageError(
        `binaries.${name}: path ${target.path} does not exist (${p})`,
      );
    const stat = statSync(p);
    if (stat.isDirectory())
      throw new BinaryStageError(
        `binaries.${name}: path ${target.path} is a directory but no executable key declares it as a unit; ` +
          `single-file binaries need a file path`,
      );
    source = readFileSync(p);
  } else {
    const cache = await fetchBinaryToCache(target.url!, target.sha256!, opts.cacheDir, {
      get: opts.get,
    });
    source = readFileSync(cache);
  }
  const got = sha256hex(source);
  if (got !== target.sha256)
    throw new BinaryStageError(
      `hash mismatch for binaries.${name}: declared sha256 ${target.sha256} but the source hashes to ${got} - ` +
        `refusing to stage; the hash in pyproject is the reviewable record of what ships`,
    );
  if (target.member) {
    try {
      return await extractMember(source, target.member);
    } catch (e) {
      throw new BinaryStageError(`binaries.${name}: ${(e as Error).message}`);
    }
  }
  return source;
}

/** The missing-target error names the exact key to add (0014 sec. 1). */
export function missingTargetError(name: string, triple: string): BinaryStageError {
  return new BinaryStageError(
    `error: binaries.${name} has no source for ${triple}\n` +
      `  add [tool.pylair.binaries.${name}.${triple}] with path or url + sha256,\n` +
      `  or drop the binary from this target`,
  );
}

export interface StagedBinary {
  name: string;
  shape: "file" | "unit";
  /** Executable path inside the unit, units only. */
  executable?: string;
}

/** Stage one unit into `destUnitDir` and rewrite its load commands for
 * the host OS. `destUnitDir` is the exact tree that ships or is used in
 * dev, so rewriting once here covers both modes. */
async function stageUnitTree(
  name: string,
  target: BinaryTarget,
  destUnitDir: string,
  opts: { cacheDir: string; appDir: string; targetTriple: string; get?: typeof httpsGet },
): Promise<void> {
  try {
    if (target.path) {
      const source = join(opts.appDir, target.path);
      const manifest = join(opts.appDir, target.manifest!);
      copyDirectoryUnit(source, destUnitDir, manifest, name);
    } else {
      const cache = await fetchBinaryToCache(target.url!, target.sha256!, opts.cacheDir, {
        get: opts.get,
      });
      extractUnitArchive(readFileSync(cache), destUnitDir, name);
    }
    markExecutable(destUnitDir, target.executable!);
    rewriteUnitLoaders(destUnitDir, opts.targetTriple, name);
  } catch (e) {
    if ((e as Error).message.startsWith("error: archive for binaries.")) throw e;
    throw new BinaryStageError(
      (e as Error).message.startsWith(`binaries.${name}`)
        ? (e as Error).message
        : `binaries.${name}: ${(e as Error).message}`,
    );
  }
}

/** Stage every declared binary for `triple`. Single files land in
 * `src-tauri/binaries/` as externalBin expects (`<name>-<triple>[.exe]`);
 * units land as resources payload trees in
 * `src-tauri/resources/binaries/<name>/`. Returns one record per staged
 * binary in declaration order. */
export async function stageBuildBinaries(
  appDir: string,
  tauriDir: string,
  binaries: Record<string, BinaryDecl>,
  triple: string,
  cacheDir: string,
  opts?: { get?: typeof httpsGet },
): Promise<StagedBinary[]> {
  const names = Object.keys(binaries);
  if (!names.length) return [];
  const outDir = join(tauriDir, "binaries");
  const resourcesDir = join(tauriDir, "resources", "binaries");
  mkdirSync(outDir, { recursive: true });
  const windows = tripleIsWindows(triple);
  const staged: StagedBinary[] = [];
  for (const name of names) {
    const decl = binaries[name]!;
    const target = decl.targets[triple];
    if (!target) throw missingTargetError(name, triple);
    if (target.executable) {
      await stageUnitTree(name, target, join(resourcesDir, name), {
        cacheDir,
        appDir,
        targetTriple: triple,
        get: opts?.get,
      });
      staged.push({ name, shape: "unit", executable: target.executable });
      continue;
    }
    const bytes = await resolveBinaryBytes(name, target, { cacheDir, appDir, get: opts?.get });
    const suffix = windows ? ".exe" : "";
    const dest = join(outDir, `${name}-${triple}${suffix}`);
    writeFileSync(join(outDir, `.${name}.part`), bytes);
    renameSync(join(outDir, `.${name}.part`), dest);
    if (!isWin) chmodSync(dest, 0o755);
    staged.push({ name, shape: "file" });
  }
  return staged;
}

/** Merge `binaries/<name>` entries into tauri.conf.json's
 * bundle.externalBin idempotently, and the resources mapping that carries
 * the manifest into the payload. Declared names are added when absent and
 * never duplicated; entries a developer wrote by hand (anything not
 * colliding with a declared name) are left exactly as they are. The conf
 * is build-tree state - the diff is reviewable in git, same precedent as
 * the updater version sync. Returns the list of changes made. */
export function mergeExternalBin(
  tauriDir: string,
  names: string[],
  includeResources = names.length > 0,
): string[] {
  if (!names.length && !includeResources) return [];
  const confPath = join(tauriDir, "tauri.conf.json");
  const conf = JSON.parse(readFileSync(confPath, "utf-8"));
  conf.bundle ??= {};
  const changes: string[] = [];
  const current: string[] = Array.isArray(conf.bundle.externalBin)
    ? conf.bundle.externalBin
    : conf.bundle.externalBin
      ? [conf.bundle.externalBin]
      : [];
  const wanted = names.map((n) => `binaries/${n}`);
  // Dedup within the user's own entries only: an already-merged pylair
  // entry overlaps with `wanted` by design and must not count as a
  // duplicate, or every rebuild would rewrite the file and lie in the
  // diff. A true no-op leaves the file byte-identical.
  const seen = new Set<string>();
  const merged: string[] = [];
  for (const entry of current) {
    if (seen.has(entry)) {
      changes.push(`removed duplicate externalBin entry ${entry}`);
      continue;
    }
    seen.add(entry);
    merged.push(entry);
  }
  for (const w of wanted) {
    if (seen.has(w)) continue;
    seen.add(w);
    merged.push(w);
    changes.push(`added externalBin entry ${w}`);
  }
  if (merged.length) conf.bundle.externalBin = merged;
  // The manifest rides the resources payload at binaries/binaries.json -
  // without the mapping tauri drops it on the floor at bundle time.
  if (Array.isArray(conf.bundle.resources)) {
    // Array entries keep their source-relative path, so the file would
    // land at the wrong resource path - appending is not an option, and
    // skipping silently would ship an app whose PYLAIR_BINARIES never
    // exists (pylair.binary() fails at runtime with no hint why).
    throw new Error(
      "bundle.resources in src-tauri/tauri.conf.json is an array; " +
        'convert it to the object form { "<src>": "<dest>" } so pylair can map ' +
        "binaries/binaries.json, or remove the binaries table",
    );
  }
  conf.bundle.resources ??= {};
  if (!conf.bundle.resources["resources/binaries"]) {
    conf.bundle.resources["resources/binaries"] = "binaries/";
    changes.push('added resources mapping "resources/binaries" -> "binaries/"');
  }
  if (changes.length) writeFileSync(confPath, JSON.stringify(conf, null, 2) + "\n");
  return changes;
}

/** Write the binaries.json manifest exactly per the 0014 sec. 2 schema
 * the crate parses: files root at `exe_dir`; units root at `payload` and
 * point at `<name>/<executable>`. Returns the manifest path. */
export function writeBinariesManifest(
  binariesDir: string,
  staged: StagedBinary[],
  windows: boolean,
): string {
  mkdirSync(binariesDir, { recursive: true });
  const entries: Record<string, unknown> = {};
  for (const bin of [...staged].sort((a, b) => a.name.localeCompare(b.name))) {
    entries[bin.name] =
      bin.shape === "unit"
        ? { shape: "unit", root: "payload", path: `${bin.name}/${bin.executable}` }
        : { shape: "file", root: "exe_dir", path: installedBinaryName(bin.name, windows) };
  }
  const doc = JSON.stringify({ version: 1, binaries: entries });
  const out = join(binariesDir, "binaries.json");
  writeFileSync(out, doc);
  return out;
}

/** `pylair dev` binary resolution (0014 sec. 3): the declared `path` for
 * the host triple wins; otherwise the hash-verified download lands in the
 * cache once and is reused afterwards. Files stage flat into
 * `.pylair/binaries/`; units stage into `.pylair/binaries/<name>/` with the
 * same layout they install with. Returns the binaries dir when anything
 * was staged, undefined otherwise. */
export async function prepareDevBinaries(
  appDir: string,
  binaries: Record<string, BinaryDecl>,
  opts?: { get?: typeof httpsGet },
): Promise<string | undefined> {
  const names = Object.keys(binaries);
  if (!names.length) return undefined;
  const triple = hostTriple();
  const dir = join(appDir, ".pylair", "binaries");
  const cacheDir = join(appDir, ".pylair", "cache");
  mkdirSync(dir, { recursive: true });
  const windows = tripleIsWindows(triple);
  const staged: StagedBinary[] = [];
  for (const name of names) {
    const decl = binaries[name]!;
    const target = decl.targets[triple];
    if (!target) throw missingTargetError(name, triple);
    if (target.executable) {
      await stageUnitTree(name, target, join(dir, name), { cacheDir, appDir, targetTriple: triple, get: opts?.get });
      staged.push({ name, shape: "unit", executable: target.executable });
      continue;
    }
    const bytes = await resolveBinaryBytes(name, target, { cacheDir, appDir, get: opts?.get });
    const dest = join(dir, installedBinaryName(name, windows));
    writeFileSync(`${dest}.part`, bytes);
    renameSync(`${dest}.part`, dest);
    if (!isWin) chmodSync(dest, 0o755);
    staged.push({ name, shape: "file" });
  }
  writeBinariesManifest(dir, staged, windows);
  return dir;
}
