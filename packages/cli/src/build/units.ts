/** Directory-unit staging and loader rewriting (decision 0014 sec. 2).
 *
 * A unit is one executable plus the shared libraries it needs, kept as a
 * tree. Archive extraction is a trust boundary: paths are validated before
 * anything is written, links must stay inside the tree, and the tree is
 * staged as a whole. Loader rewriting is host-native only, matching the
 * no-cross-compile rule in sec. 9. */

import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import {
  chmodSync,
  closeSync,
  cpSync,
  existsSync,
  lstatSync,
  mkdirSync,
  openSync,
  readdirSync,
  readFileSync,
  readSync,
  readlinkSync,
  rmSync,
  statSync,
  symlinkSync,
  linkSync,
  writeFileSync,
} from "node:fs";
import { platform } from "node:os";
import { basename, dirname, join, posix, relative } from "node:path";
import { gunzipSync, inflateRawSync } from "node:zlib";

export class UnitStageError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "UnitStageError";
  }
}

const isWin = platform() === "win32";

type EntryKind = "file" | "directory" | "symlink" | "hardlink" | "other";

interface ArchiveEntry {
  /** The path exactly as the archive named it, used in errors. */
  rawPath: string;
  path: string;
  kind: EntryKind;
  data?: Buffer;
  size: number;
  mode?: number;
  linkTarget?: string;
  directoryHint?: boolean;
}

export interface TreeInventoryEntry {
  kind: "file" | "symlink";
  sha256: string;
  size: number;
}

export interface TreeInventory {
  entries: Map<string, TreeInventoryEntry>;
  fileCount: number;
  totalBytes: number;
}

function sha256hex(data: Buffer | string): string {
  return createHash("sha256").update(data).digest("hex");
}

function sha256File(path: string): string {
  const hash = createHash("sha256");
  const fd = openSync(path, "r");
  try {
    const buf = Buffer.alloc(1024 * 1024);
    for (;;) {
      const n = readSync(fd, buf, 0, buf.length, null);
      if (!n) break;
      hash.update(buf.subarray(0, n));
    }
  } finally {
    closeSync(fd);
  }
  return hash.digest("hex");
}

/** The exact sec. 2 rejection. Every unsafe-path and escaping-link failure
 * uses this wording and names the archive entry, never just a reason. */
function unsafeArchiveEntry(binaryName: string, entry: string): UnitStageError {
  return new UnitStageError(
    `error: archive for binaries.${binaryName} contains unsafe entry ${JSON.stringify(entry)}\n` +
      `  archives may only carry relative paths inside the unit tree`,
  );
}

function unsafeDirectoryEntry(binaryName: string, entry: string): UnitStageError {
  return new UnitStageError(
    `error: directory for binaries.${binaryName} contains unsafe entry ${JSON.stringify(entry)}\n` +
      `  directory units may only carry relative paths inside the unit tree`,
  );
}

/** Normalize one archive path without ever letting normalization hide a
 * traversal segment. Returns null for the archive's root directory entry. */
function normalizeEntryPath(raw: string): { path: string; directoryHint: boolean } | null {
  let p = raw;
  while (p.startsWith("./")) p = p.slice(2);
  let directoryHint = false;
  while (p.endsWith("/")) {
    directoryHint = true;
    p = p.slice(0, -1);
  }
  if (!p || p === ".") return null;
  if (p.includes("\\")) throw new Error("backslash separator");
  if (p.startsWith("/") || /^[A-Za-z]:/.test(p)) throw new Error("absolute path");
  for (const seg of p.split("/")) {
    if (seg === "..") throw new Error("dotdot segment");
    if (seg === "") throw new Error("empty segment");
  }
  const normalized = posix.normalize(p);
  if (!normalized || normalized === ".") return null;
  return { path: normalized, directoryHint };
}

/** Lexically resolve a link target inside the unit. Symlink targets are
 * relative to the directory containing the link; tar hard-link targets are
 * relative to the unit root. Null means the target escapes. */
export function resolveLinkTarget(entryPath: string, target: string, hardlink: boolean): string | null {
  if (!target || target.includes("\\")) return null;
  if (target.startsWith("/") || /^[A-Za-z]:/.test(target)) return null;
  const base = hardlink ? "" : posix.dirname(entryPath);
  const resolved = posix.normalize(posix.join(base === "." ? "" : base, target));
  if (!resolved || resolved === ".." || resolved.startsWith("../") || posix.isAbsolute(resolved))
    return null;
  return resolved;
}

// ---- zip -----------------------------------------------------------------

function zipEntries(archive: Buffer): ArchiveEntry[] {
  let eocd = -1;
  for (let i = archive.length - 22; i >= Math.max(0, archive.length - 22 - 65536); i--) {
    if (archive.readUInt32LE(i) === 0x06054b50) {
      eocd = i;
      break;
    }
  }
  if (eocd < 0) throw new UnitStageError("zip archive has no end-of-central-directory record");
  const count = archive.readUInt16LE(eocd + 10);
  let off = archive.readUInt32LE(eocd + 16);
  const out: ArchiveEntry[] = [];
  for (let n = 0; n < count; n++) {
    if (off + 46 > archive.length || archive.readUInt32LE(off) !== 0x02014b50)
      throw new UnitStageError("zip central directory is truncated or malformed");
    const flags = archive.readUInt16LE(off + 8);
    const method = archive.readUInt16LE(off + 10);
    const compressed = archive.readUInt32LE(off + 20);
    const uncompressed = archive.readUInt32LE(off + 24);
    const nameLen = archive.readUInt16LE(off + 28);
    const extraLen = archive.readUInt16LE(off + 30);
    const commentLen = archive.readUInt16LE(off + 32);
    const externalAttrs = archive.readUInt32LE(off + 38);
    const localOff = archive.readUInt32LE(off + 42);
    const rawPath = archive.subarray(off + 46, off + 46 + nameLen).toString("utf-8");
    if (compressed === 0xffffffff || uncompressed === 0xffffffff)
      throw new UnitStageError(`zip64 archives are not supported (entry ${JSON.stringify(rawPath)})`);
    if (flags & 0x1)
      throw new UnitStageError(`zip entry ${JSON.stringify(rawPath)} is encrypted, which is not supported`);
    if (localOff + 30 > archive.length || archive.readUInt32LE(localOff) !== 0x04034b50)
      throw new UnitStageError(`zip entry ${JSON.stringify(rawPath)} has a malformed local header`);
    const lhNameLen = archive.readUInt16LE(localOff + 26);
    const lhExtraLen = archive.readUInt16LE(localOff + 28);
    const dataOff = localOff + 30 + lhNameLen + lhExtraLen;
    if (dataOff + compressed > archive.length)
      throw new UnitStageError(`zip entry ${JSON.stringify(rawPath)} is truncated`);
    const packed = archive.subarray(dataOff, dataOff + compressed);
    let data: Buffer;
    if (method === 0) data = Buffer.from(packed);
    else if (method === 8) data = inflateRawSync(packed);
    else
      throw new UnitStageError(
        `zip entry ${JSON.stringify(rawPath)} uses compression method ${method} - only stored (0) and deflated (8) are supported`,
      );
    const mode = (externalAttrs >>> 16) & 0xffff;
    const typeBits = mode & 0o170000;
    const directoryHint = rawPath.endsWith("/") || typeBits === 0o040000;
    const kind: EntryKind = directoryHint
      ? "directory"
      : typeBits === 0o120000
        ? "symlink"
        : typeBits === 0 || typeBits === 0o100000
          ? "file"
          : "other";
    out.push({
      rawPath,
      path: rawPath,
      kind,
      data: kind === "directory" ? undefined : data,
      size: uncompressed,
      mode: mode ? mode & 0o7777 : undefined,
      linkTarget: kind === "symlink" ? data.toString("utf-8") : undefined,
      directoryHint,
    });
    off += 46 + nameLen + extraLen + commentLen;
  }
  return out;
}

// ---- tar.gz ---------------------------------------------------------------

function tarString(block: Buffer, start: number, len: number): string {
  const end = block.indexOf(0, start);
  const stop = end >= 0 && end < start + len ? end : start + len;
  return block.subarray(start, stop).toString("utf-8");
}

function tarOctal(block: Buffer, start: number, len: number): number {
  const raw = tarString(block, start, len).trim();
  if (!raw) return 0;
  return Number.parseInt(raw, 8);
}

function tarSize(block: Buffer): number {
  const first = block[124];
  if ((first & 0x80) === 0) return tarOctal(block, 124, 12);
  // POSIX base-256 size. Units this large are not sensible, but parse the
  // format correctly rather than misreading the high bit as octal text.
  let value = 0n;
  const bytes = block.subarray(124, 136);
  for (let i = 1; i < bytes.length; i++) value = (value << 8n) + BigInt(bytes[i]!);
  if (value > BigInt(Number.MAX_SAFE_INTEGER)) throw new UnitStageError("tar entry is too large");
  return Number(value);
}

function parsePax(data: Buffer): Record<string, string> {
  const out: Record<string, string> = {};
  let off = 0;
  while (off < data.length) {
    const sp = data.indexOf(0x20, off);
    if (sp < 0) break;
    const len = Number.parseInt(data.subarray(off, sp).toString("ascii"), 10);
    if (!Number.isFinite(len) || len <= 0 || off + len > data.length) break;
    const record = data.subarray(sp + 1, off + len - 1).toString("utf-8");
    const eq = record.indexOf("=");
    if (eq > 0) out[record.slice(0, eq)] = record.slice(eq + 1);
    off += len;
  }
  return out;
}

function tarGzEntries(archive: Buffer): ArchiveEntry[] {
  const tar = gunzipSync(archive);
  const out: ArchiveEntry[] = [];
  let off = 0;
  let longName: string | undefined;
  let longLink: string | undefined;
  let pax: Record<string, string> = {};
  while (off + 512 <= tar.length) {
    const header = tar.subarray(off, off + 512);
    if (header.every((b) => b === 0)) break;
    const size = tarSize(header);
    const dataStart = off + 512;
    const dataEnd = dataStart + size;
    if (dataEnd > tar.length) throw new UnitStageError("tar archive is truncated");
    const data = tar.subarray(dataStart, dataEnd);
    const typeflag = String.fromCharCode(header[156] || 0x30);
    if (typeflag === "L") {
      longName = data.toString("utf-8").replace(/\0.*$/s, "");
    } else if (typeflag === "K") {
      longLink = data.toString("utf-8").replace(/\0.*$/s, "");
    } else if (typeflag === "x") {
      pax = { ...pax, ...parsePax(data) };
    } else if (typeflag !== "g") {
      const prefix = tarString(header, 345, 155);
      const shortName = tarString(header, 0, 100);
      const rawPath = pax.path ?? longName ?? (prefix ? `${prefix}/${shortName}` : shortName);
      const linkTarget = pax.linkpath ?? longLink ?? tarString(header, 157, 100) ?? undefined;
      const kind: EntryKind =
        typeflag === "0" || typeflag === "7" || typeflag === "\0"
          ? "file"
          : typeflag === "5"
            ? "directory"
            : typeflag === "2"
              ? "symlink"
              : typeflag === "1"
                ? "hardlink"
                : "other";
      out.push({
        rawPath,
        path: rawPath,
        kind,
        data: kind === "file" ? Buffer.from(data) : undefined,
        size,
        mode: tarOctal(header, 100, 8) || undefined,
        linkTarget: linkTarget || undefined,
        directoryHint: kind === "directory",
      });
      longName = undefined;
      longLink = undefined;
      pax = {};
    }
    off = dataStart + Math.ceil(size / 512) * 512;
  }
  return out;
}

function archiveEntries(archive: Buffer): ArchiveEntry[] {
  if (archive.length >= 4 && archive.readUInt32LE(0) === 0x04034b50) return zipEntries(archive);
  if (archive.length >= 2 && archive[0] === 0x1f && archive[1] === 0x8b) return tarGzEntries(archive);
  throw new UnitStageError("unit archive is neither zip nor tar.gz (magic bytes unrecognized)");
}

function checkedArchiveEntries(archive: Buffer, binaryName: string): ArchiveEntry[] {
  const parsed = archiveEntries(archive);
  const out: ArchiveEntry[] = [];
  for (const entry of parsed) {
    let normalized: { path: string; directoryHint: boolean } | null;
    try {
      normalized = normalizeEntryPath(entry.rawPath);
    } catch {
      throw unsafeArchiveEntry(binaryName, entry.rawPath);
    }
    if (!normalized) continue; // the archive root itself
    entry.path = normalized.path;
    entry.directoryHint = entry.directoryHint || normalized.directoryHint;
    if (entry.kind === "other")
      throw new UnitStageError(
        `archive for binaries.${binaryName} entry ${JSON.stringify(entry.rawPath)} is not a regular file, directory, symlink or hard link`,
      );
    if ((entry.kind === "symlink" || entry.kind === "hardlink") &&
        resolveLinkTarget(entry.path, entry.linkTarget ?? "", entry.kind === "hardlink") === null)
      throw unsafeArchiveEntry(binaryName, entry.rawPath);
    out.push(entry);
  }
  return out;
}

/** Extract a whole zip or tar.gz unit into `dest`, replacing any previous
 * staging. Validation of every entry completes before the first write. */
export function extractUnitArchive(archive: Buffer, dest: string, binaryName: string): void {
  const entries = checkedArchiveEntries(archive, binaryName);
  rmSync(dest, { recursive: true, force: true });
  mkdirSync(dest, { recursive: true });

  // Regular files and directories first. Links are created only after every
  // data write is complete, so no file write can ever pass through a symlink.
  const links: ArchiveEntry[] = [];
  for (const entry of entries) {
    const out = join(dest, ...entry.path.split("/"));
    if (entry.kind === "directory") {
      mkdirSync(out, { recursive: true });
      if (!isWin && entry.mode !== undefined) chmodSync(out, entry.mode & 0o7777);
    } else if (entry.kind === "file") {
      mkdirSync(dirname(out), { recursive: true });
      writeFileSync(out, entry.data ?? Buffer.alloc(0));
      if (!isWin && entry.mode !== undefined) chmodSync(out, entry.mode & 0o7777);
    } else {
      links.push(entry);
    }
  }
  for (const entry of links) {
    const out = join(dest, ...entry.path.split("/"));
    mkdirSync(dirname(out), { recursive: true });
    if (entry.kind === "symlink") {
      const target = entry.linkTarget ?? "";
      let type: "file" | "dir" = "file";
      const resolved = resolveLinkTarget(entry.path, target, false);
      if (resolved) {
        try {
          type = statSync(join(dest, ...resolved.split("/"))).isDirectory() ? "dir" : "file";
        } catch {
          // A dangling in-unit link is still preserved as a file link.
        }
      }
      symlinkSync(target, out, type);
    } else if (entry.kind === "hardlink") {
      const resolved = resolveLinkTarget(entry.path, entry.linkTarget ?? "", true);
      if (!resolved) throw unsafeArchiveEntry(binaryName, entry.rawPath);
      linkSync(join(dest, ...resolved.split("/")), out);
    }
  }
}

/** Count the payload entries and uncompressed regular-file bytes in an
 * archive without extracting it. Used by doctor; validation errors propagate. */
export function inventoryArchive(archive: Buffer, binaryName = "unit"): {
  fileCount: number;
  totalBytes: number;
} {
  const entries = checkedArchiveEntries(archive, binaryName);
  let fileCount = 0;
  let totalBytes = 0;
  for (const entry of entries) {
    if (entry.kind === "directory") continue;
    fileCount++;
    if (entry.kind === "file") totalBytes += entry.size;
  }
  return { fileCount, totalBytes };
}

// ---- directory path sources ------------------------------------------------

function relativePosix(root: string, path: string): string {
  return relative(root, path).split("\\").join("/");
}

/** Inventory a staged unit tree. Symlinks are reviewable by hashing the
 * link target text; regular files hash their bytes. Directories carry no
 * manifest rows. */
export function inventoryDirectoryTree(root: string, binaryName?: string): TreeInventory {
  const entries = new Map<string, TreeInventoryEntry>();
  let totalBytes = 0;
  const walk = (dir: string): void => {
    for (const ent of readdirSync(dir, { withFileTypes: true })) {
      const p = join(dir, ent.name);
      const rel = relativePosix(root, p);
      if (ent.isDirectory()) {
        walk(p);
      } else if (ent.isFile()) {
        const size = lstatSync(p).size;
        entries.set(rel, { kind: "file", sha256: sha256File(p), size });
        totalBytes += size;
      } else if (ent.isSymbolicLink()) {
        const target = readlinkSync(p);
        if (binaryName && resolveLinkTarget(rel, target, false) === null)
          throw unsafeDirectoryEntry(binaryName, rel);
        entries.set(rel, { kind: "symlink", sha256: sha256hex(target), size: 0 });
      } else {
        throw new UnitStageError(
          `${binaryName ? `binaries.${binaryName}: ` : ""}unsupported file type at ${rel} - units carry regular files, directories and symlinks only`,
        );
      }
    }
  };
  walk(root);
  return { entries, fileCount: entries.size, totalBytes };
}

function checkedManifestPath(path: string): boolean {
  try {
    return normalizeEntryPath(path) !== null;
  } catch {
    return false;
  }
}

/** Verify a staged directory source against its checked-in manifest. Missing,
 * extra and mismatched files are all errors and every offending path is named. */
export function verifyDirectoryManifest(
  unitDir: string,
  manifestFile: string,
  binaryName: string,
): TreeInventory {
  if (!existsSync(manifestFile))
    throw new UnitStageError(
      `binaries.${binaryName}: manifest ${manifestFile} does not exist - directory units need a checked-in path-to-sha256 map`,
    );
  let raw: unknown;
  try {
    raw = JSON.parse(readFileSync(manifestFile, "utf-8"));
  } catch (e) {
    throw new UnitStageError(`binaries.${binaryName}: manifest ${manifestFile} is not valid JSON: ${(e as Error).message}`);
  }
  if (typeof raw !== "object" || raw === null || Array.isArray(raw))
    throw new UnitStageError(`binaries.${binaryName}: manifest ${manifestFile} must be an object mapping relative paths to sha256`);
  const expected = new Map<string, string>();
  for (const [path, hash] of Object.entries(raw as Record<string, unknown>)) {
    if (!checkedManifestPath(path))
      throw new UnitStageError(
        `binaries.${binaryName}: manifest path ${JSON.stringify(path)} must be relative, use forward slashes, and contain no empty or .. segments`,
      );
    if (typeof hash !== "string" || !/^[0-9a-f]{64}$/.test(hash))
      throw new UnitStageError(
        `binaries.${binaryName}: manifest hash for ${JSON.stringify(path)} must be 64 lowercase hex characters`,
      );
    expected.set(posix.normalize(path), hash);
  }
  const inventory = inventoryDirectoryTree(unitDir, binaryName);
  const missing = [...expected.keys()].filter((p) => !inventory.entries.has(p)).sort();
  const extra = [...inventory.entries.keys()].filter((p) => !expected.has(p)).sort();
  if (missing.length || extra.length) {
    const parts: string[] = [];
    if (missing.length) parts.push(`missing files: ${missing.join(", ")}`);
    if (extra.length) parts.push(`extra files: ${extra.join(", ")}`);
    throw new UnitStageError(
      `binaries.${binaryName}: directory manifest ${manifestFile} does not match the staged tree - ${parts.join("; ")}`,
    );
  }
  const mismatched = [...expected.entries()]
    .filter(([p, hash]) => inventory.entries.get(p)?.sha256 !== hash)
    .map(([p]) => p)
    .sort();
  if (mismatched.length)
    throw new UnitStageError(
      `binaries.${binaryName}: directory manifest ${manifestFile} hash mismatch for: ${mismatched.join(", ")}`,
    );
  return inventory;
}

/** Copy a vendored directory into the unit staging dir and verify the staged
 * copy against the manifest. The source itself is never modified. */
export function copyDirectoryUnit(
  source: string,
  dest: string,
  manifestFile: string,
  binaryName: string,
): TreeInventory {
  if (!existsSync(source) || !statSync(source).isDirectory())
    throw new UnitStageError(
      `binaries.${binaryName}: executable declares a directory unit, but path ${source} is not a directory`,
    );
  inventoryDirectoryTree(source, binaryName); // validates in-unit symlinks before copying
  rmSync(dest, { recursive: true, force: true });
  mkdirSync(dirname(dest), { recursive: true });
  cpSync(source, dest, { recursive: true, verbatimSymlinks: true });
  return verifyDirectoryManifest(dest, manifestFile, binaryName);
}

// ---- executable format detection -------------------------------------------

const MACHO_MAGIC = new Set([
  0xfeedface, 0xfeedfacf, 0xcefaedfe, 0xcffaedfe,
  0xcafebabe, 0xbebafeca, 0xcafebabf, 0xbfbafeca,
]);

export function isMachOBytes(bytes: Buffer): boolean {
  return bytes.length >= 4 && MACHO_MAGIC.has(bytes.readUInt32BE(0));
}

export function isElfBytes(bytes: Buffer): boolean {
  return bytes.length >= 4 && bytes[0] === 0x7f && bytes[1] === 0x45 && bytes[2] === 0x4c && bytes[3] === 0x46;
}

function magicBytes(path: string): Buffer {
  const fd = openSync(path, "r");
  try {
    const b = Buffer.alloc(4);
    return readSync(fd, b, 0, 4, 0) === 4 ? b : Buffer.alloc(0);
  } finally {
    closeSync(fd);
  }
}

export function isMachOFile(path: string): boolean {
  return isMachOBytes(magicBytes(path));
}

export function isElfFile(path: string): boolean {
  return isElfBytes(magicBytes(path));
}

function regularFiles(root: string): string[] {
  const out: string[] = [];
  const walk = (dir: string): void => {
    for (const ent of readdirSync(dir, { withFileTypes: true })) {
      const p = join(dir, ent.name);
      if (ent.isDirectory()) walk(p);
      else if (ent.isFile()) out.push(p);
    }
  };
  walk(root);
  return out.sort();
}

function relDirOf(root: string, file: string): string {
  const rel = relativePosix(root, dirname(file));
  return rel === "" ? "" : rel;
}

/** The rpath set one loader needs: one entry for each unit directory holding
 * code, expressed relative to the loading file. Empty means the same dir. */
export function computeLoaderRpaths(
  loaderDir: string,
  codeDirs: string[],
  prefix: "@loader_path" | "$ORIGIN",
): string[] {
  const unique = [...new Set(codeDirs.map((d) => posix.normalize(d || ".")).map((d) => (d === "." ? "" : d)))].sort();
  return unique.map((dir) => {
    const rel = posix.relative(loaderDir || ".", dir || ".");
    return rel ? `${prefix}/${rel}` : prefix;
  });
}

// ---- macOS loader rewriting -------------------------------------------------

function toolOutput(cmd: string, args: string[], missing: string): string {
  const r = spawnSync(cmd, args, { encoding: "utf-8" });
  if (r.error) {
    if ((r.error as NodeJS.ErrnoException).code === "ENOENT") throw new UnitStageError(missing);
    throw new UnitStageError(`${cmd} ${args.join(" ")} failed to start: ${r.error.message}`);
  }
  if (r.status !== 0)
    throw new UnitStageError(`${cmd} ${args.join(" ")} exited ${r.status}: ${(r.stderr ?? "").trim()}`);
  return r.stdout ?? "";
}

function otoolInstallNames(file: string): string[] {
  const out = toolOutput("otool", ["-L", file], "otool is required to rewrite Mach-O install names on macOS");
  return out
    .split(/\r?\n/)
    .slice(1)
    .map((line) => line.trim())
    .filter(Boolean)
    .map((line) => /\s+\(compatibility version/.test(line) ? line.slice(0, line.indexOf(" (compatibility version")).trim() : line);
}

function otoolInstallId(file: string): string | undefined {
  // Executables have no LC_ID_DYLIB, and some otool versions make `otool -D`
  // a nonzero answer to that non-question. No id is normal there, not a
  // staging failure; malformed Mach-O still fails at `otool -L` below.
  const r = spawnSync("otool", ["-D", file], { encoding: "utf-8" });
  if (r.error || r.status !== 0) return undefined;
  return (r.stdout ?? "")
    .split(/\r?\n/)
    .slice(1)
    .map((line) => line.trim())
    .find(Boolean);
}

function otoolRpaths(file: string): string[] {
  const out = toolOutput("otool", ["-l", file], "otool is required to rewrite Mach-O rpaths on macOS");
  const rpaths: string[] = [];
  const lines = out.split(/\r?\n/);
  for (let i = 0; i < lines.length; i++) {
    if (!/^\s*cmd LC_RPATH\s*$/.test(lines[i] ?? "")) continue;
    for (let j = i + 1; j < Math.min(lines.length, i + 5); j++) {
      const m = /^\s*path\s+(\S+)/.exec(lines[j] ?? "");
      if (m) {
        rpaths.push(m[1]!);
        break;
      }
    }
  }
  return rpaths;
}

const systemInstallName = (name: string): boolean =>
  name.startsWith("/usr/lib/") || name.startsWith("/System/");

function machOSharedObject(p: string): boolean {
  const r = spawnSync("otool", ["-D", p], { encoding: "utf-8" });
  if (r.status !== 0) return p.endsWith(".dylib");
  return (r.stdout ?? "").split(/\r?\n/).slice(1).some((line) => line.trim().length > 0);
}

/** Ad-hoc re-sign rewritten unit code at staging time. This matters in
 * dev as well as unsigned builds: rewriting invalidates the signature an
 * upstream dylib carried, and newer macOS can refuse an unsigned nested
 * dylib even when the app itself is unsigned. Build-time signing with the
 * app identity replaces these signatures when configured. */
function adHocSignMachOTree(unitDir: string): number {
  const files = regularFiles(unitDir).filter(isMachOFile);
  const depth = (p: string): number => p.split(/[\\/]/).length;
  const libs = files.filter(machOSharedObject).sort((a, b) => depth(b) - depth(a));
  const exes = files.filter((p) => !libs.includes(p)).sort((a, b) => depth(b) - depth(a));
  for (const file of [...libs, ...exes])
    toolOutput(
      "codesign",
      ["--force", "--sign", "-", file],
      "codesign is required to re-sign rewritten Mach-O files on macOS",
    );
  return files.length;
}

function rewriteMachOUnit(unitDir: string): number {
  const files = regularFiles(unitDir).filter(isMachOFile);
  if (!files.length) return 0;
  const codeDirs = [...new Set(files.map((f) => relDirOf(unitDir, f)))].sort();
  let rewritten = 0;
  for (const file of files) {
    const args: string[] = [];
    const id = otoolInstallId(file);
    if (id && !systemInstallName(id)) {
      const wanted = `@rpath/${posix.basename(id)}`;
      if (id !== wanted) args.push("-id", wanted);
    }
    let deps = otoolInstallNames(file);
    if (id && deps[0] === id) deps = deps.slice(1);
    const changes = new Map<string, string>();
    for (const dep of deps) {
      if (systemInstallName(dep)) continue;
      const wanted = `@rpath/${posix.basename(dep)}`;
      if (dep !== wanted) changes.set(dep, wanted);
    }
    for (const [oldName, newName] of changes) args.push("-change", oldName, newName);
    const loaderDir = relDirOf(unitDir, file);
    const wantedRpaths = computeLoaderRpaths(loaderDir, codeDirs, "@loader_path");
    const existing = new Set(otoolRpaths(file));
    for (const rpath of wantedRpaths) if (!existing.has(rpath)) args.push("-add_rpath", rpath);
    if (!args.length) continue;
    toolOutput(
      "install_name_tool",
      [...args, file],
      "install_name_tool is required to rewrite Mach-O install names on macOS",
    );
    rewritten++;
  }
  adHocSignMachOTree(unitDir);
  return rewritten;
}

// ---- Linux loader rewriting -------------------------------------------------

function rewriteElfUnit(unitDir: string): number {
  const files = regularFiles(unitDir).filter(isElfFile);
  if (!files.length) return 0;
  const codeDirs = [...new Set(files.map((f) => relDirOf(unitDir, f)))].sort();
  for (const file of files) {
    const loaderDir = relDirOf(unitDir, file);
    const rpath = computeLoaderRpaths(loaderDir, codeDirs, "$ORIGIN").join(":");
    toolOutput(
      "patchelf",
      ["--set-rpath", rpath, file],
      "patchelf is required to stage directory units on Linux. Install it (Debian/Ubuntu: sudo apt-get install patchelf) or use static single-file binaries; cross-OS staging is not supported",
    );
  }
  return files.length;
}

/** Rewrite a staged unit for the target loader when target and host are the
 * same OS. Windows needs no rewriting: its loader searches the executable's
 * own directory first, which is exactly the unit layout. Cross-OS staging is
 * out of scope by decision 0014 sec. 9, so a mismatched target is untouched. */
export function rewriteUnitLoaders(
  unitDir: string,
  targetTriple?: string,
): { kind: "mach-o" | "elf" | "none"; rewritten: number } {
  const targetOs = !targetTriple
    ? platform()
    : targetTriple.includes("windows")
      ? "win32"
      : targetTriple.includes("apple")
        ? "darwin"
        : targetTriple.includes("linux")
          ? "linux"
          : platform();
  if (targetOs !== platform()) return { kind: "none", rewritten: 0 };
  if (platform() === "darwin") return { kind: "mach-o", rewritten: rewriteMachOUnit(unitDir) };
  if (platform() === "linux") return { kind: "elf", rewritten: rewriteElfUnit(unitDir) };
  return { kind: "none", rewritten: 0 };
}

/** Set the execute bit on the declared entry point no matter what the
 * archive carried. POSIX-only; Windows has no executable bit. */
export function markExecutable(unitDir: string, executable: string): string {
  const p = join(unitDir, ...executable.split("/"));
  if (!existsSync(p))
    throw new UnitStageError(
      `unit executable ${JSON.stringify(executable)} does not exist in the staged tree`,
    );
  if (!isWin) chmodSync(p, 0o755);
  return p;
}
