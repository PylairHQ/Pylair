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
  writeFileSync,
} from "node:fs";
import { get } from "node:https";
import { arch, platform, tmpdir } from "node:os";
import { basename, join, relative } from "node:path";
import { fileURLToPath } from "node:url";
import { importRoot, loadConfig, loadTauriConfig, tauriCliEntry } from "./config.js";
import { matchGlob } from "./glob.js";
import { DEFAULT_PRUNE, prune } from "./prune.js";
import { x as untar } from "tar";

const PBS_TAG = "20251120";
const PBS_VERSIONS: Record<string, string> = { "3.12": "3.12.12", "3.13": "3.13.9" };
const HOST_TRIPLES: Record<string, string> = {
  "darwin:arm64": "aarch64-apple-darwin",
  "darwin:x64": "x86_64-apple-darwin",
  "win32:x64": "x86_64-pc-windows-msvc",
  "linux:x64": "x86_64-unknown-linux-gnu",
  "linux:arm64": "aarch64-unknown-linux-gnu",
};

const isWin = platform() === "win32";

/** Expected SHA256 per asset, committed in-repo — a hash fetched from the
 * same release as the tarball catches corruption but not a tampered
 * release. Bump together with PBS_TAG/PBS_VERSIONS; verify each value
 * against upstream SHA256SUMS before committing.
 */
const PBS_SHA256: Record<string, string> = {
  "3.12.12-aarch64-apple-darwin": "9955ac87eff0d561e64920dc46fd8625912d2c2be4a53c66ad80125d6d9fbdb7",
  "3.12.12-aarch64-unknown-linux-gnu": "4c4594cb38ddf6769f11f86818a4c6617652f4994e5a962eb4f8cf0ec977a1be",
  "3.12.12-x86_64-apple-darwin": "1b4d8eec597ff69a3b67946f30622a6c25c6612e6d15aa431b864ebff66ac8a1",
  "3.12.12-x86_64-pc-windows-msvc": "70ecfaf0bfc58298db11d6389be92c29ec4ef8cd93ebb6a7e54b41d9fc056b10",
  "3.12.12-x86_64-unknown-linux-gnu": "8d2cd1393c1aec9d701cebeccb83d1519951f71d3ecb42d5e70c70af4db9be34",
  "3.13.9-aarch64-apple-darwin": "29d38bd5fbe267b37cecac990015c662796edcf51c2f5e321d59ec06a1850816",
  "3.13.9-aarch64-unknown-linux-gnu": "fdc5d80488b64fcef3b99f297618f68e5374c13d89e888a66abeff4967afd308",
  "3.13.9-x86_64-apple-darwin": "9da51afb00480a8ccd4909bd302c80d9c104d633c5e865cf37dd5e587185e125",
  "3.13.9-x86_64-pc-windows-msvc": "f4c22b31ddbf8d7824cbcba2d8707621c2c8fab1fb6d2c1810c2bb0304d8e9a8",
  "3.13.9-x86_64-unknown-linux-gnu": "1bc9a94ab2425fb6ae58e45f69cab61647759b57895649701390822d976fa923",
};

export function hostTriple(): string {
  const t = HOST_TRIPLES[`${platform()}:${arch()}`];
  if (!t) throw new Error(`no standalone CPython build for ${platform()}/${arch()}`);
  return t;
}

const DOWNLOAD_IDLE_TIMEOUT_MS = 30_000;
const MAX_REDIRECTS = 5;

/** Download `url` to `to`, following up to `maxRedirects` redirects and
 * failing after `timeoutMs` with no bytes. The body lands in `to.part`
 * and is renamed into place only once it is complete — a dropped
 * connection must never leave a partial file at the cache path (the
 * SHA check would then refuse every later build until it is deleted by
 * hand). `get` is injectable so tests need no network. */
export function download(
  url: string,
  to: string,
  opts?: { get?: typeof get; timeoutMs?: number; maxRedirects?: number },
): Promise<void> {
  const getImpl = opts?.get ?? get;
  const timeoutMs = opts?.timeoutMs ?? DOWNLOAD_IDLE_TIMEOUT_MS;
  const maxRedirects = opts?.maxRedirects ?? MAX_REDIRECTS;
  const part = `${to}.part`;
  return new Promise((res, rej) => {
    let settled = false;
    let out: ReturnType<typeof createWriteStream> | undefined;
    const fail = (e: Error) => {
      if (settled) return;
      settled = true;
      if (out === undefined) {
        rmSync(part, { force: true });
        rej(e);
        return;
      }
      // The stream owns the fd. destroy() waits for in-flight writes
      // before closing it — closing early (closeSync) or twice lets a
      // reused fd number absorb the stale op: the write lands in some
      // other file, or the close kills it. .part is removed only once
      // 'close' says the fd is really gone, keeping the rejection
      // ordered after cleanup.
      const s = out;
      out = undefined;
      s.once("close", () => {
        rmSync(part, { force: true });
        rej(e);
      });
      s.destroy();
    };
    const onErr = (e: unknown) =>
      fail(e instanceof Error ? e : new Error(String(e)));
    const follow = (u: string, redirects: number): void => {
      try {
        if (redirects > maxRedirects)
          return fail(
            new Error(`GET ${url}: too many redirects (cap ${maxRedirects})`),
          );
        const req = getImpl(u, (r) => {
          try {
            if (r.statusCode! >= 300 && r.statusCode! < 400 && r.headers.location) {
              // Location may be relative, and only https targets are
              // fetchable — resolve first, refuse anything else.
              const target = new URL(r.headers.location, u);
              if (target.protocol !== "https:")
                return fail(
                  new Error(
                    `GET ${u}: refusing non-https redirect to ${target.origin}`,
                  ),
                );
              return follow(target.href, redirects + 1);
            }
            if (r.statusCode !== 200)
              return fail(new Error(`GET ${u} -> ${r.statusCode}`));
            const want = Number(r.headers["content-length"]);
            let got = 0;
            let ended = false;
            r.on("data", (c: Buffer) => {
              got += c.length;
            });
            r.on("end", () => {
              ended = true;
            });
            r.on("error", fail);
            r.on("close", () => {
              if (!ended)
                fail(new Error(`GET ${u}: connection dropped mid-body`));
            });
            // Open eagerly — <part>'s existence is synchronous, so no
            // deferred fs.open can land after fail()'s rmSync and recreate
            // the file. autoClose: false keeps fd closing on our explicit
            // destroy() calls only (see fail()).
            const stream = createWriteStream(part, {
              fd: openSync(part, "w"),
              autoClose: false,
            });
            out = stream;
            r.pipe(stream);
            stream.on("finish", () => {
              if (settled) return;
              out = undefined;
              stream.once("close", () => {
                if (!Number.isNaN(want) && got !== want)
                  return fail(
                    new Error(
                      `GET ${u}: incomplete body — ${got} of ${want} bytes`,
                    ),
                  );
                settled = true;
                renameSync(part, to);
                res();
              });
              stream.destroy();
            });
            stream.on("error", fail);
          } catch (e) {
            onErr(e);
          }
        });
        req.setTimeout(timeoutMs, () =>
          req.destroy(
            new Error(`GET ${u}: timed out — no data for ${timeoutMs / 1000}s`),
          ),
        );
        req.on("error", fail);
      } catch (e) {
        onErr(e);
      }
    };
    follow(url, 0);
  });
}

const sha256File = (p: string) =>
  createHash("sha256").update(readFileSync(p)).digest("hex");

/** Cache + integrity for a downloaded asset: a cached file that fails
 * its SHA is deleted and re-downloaded ONCE — a second mismatch still
 * refuses, naming the cache so it is fixable by hand. `dl` injectable
 * for tests. */
export async function ensureTarball(
  url: string,
  tarball: string,
  want: string,
  dl: (u: string, to: string) => Promise<void> = download,
): Promise<void> {
  const cached = existsSync(tarball);
  if (!cached) {
    console.log(`fetching ${basename(tarball)}`);
    await dl(url, tarball);
  } else {
    console.log(`using cached ${basename(tarball)}`);
  }
  if (sha256File(tarball) === want) return;
  if (!cached)
    throw new Error(
      `SHA256 mismatch for ${basename(tarball)}: got ${sha256File(tarball)}, ` +
        `want ${want} — refusing to bundle`,
    );
  // A cached-but-corrupt file is the dropped-partial failure mode: one
  // retry, then refuse.
  console.warn(
    `cached ${basename(tarball)} failed its SHA256 — ` +
      `deleting ${tarball} and re-downloading once`,
  );
  rmSync(tarball, { force: true });
  await dl(url, tarball);
  const got = sha256File(tarball);
  if (got !== want)
    throw new Error(
      `SHA256 mismatch for ${basename(tarball)} after re-download: got ${got}, ` +
        `want ${want} — refusing to bundle (cache file: ${tarball})`,
    );
}

async function fetchPython(
  resources: string,
  cacheDir: string,
  versionClass: string,
): Promise<string> {
  const ver = PBS_VERSIONS[versionClass];
  if (!ver)
    throw new Error(
      `python-version "${versionClass}" not available — supported: ${Object.keys(PBS_VERSIONS).join(", ")}`,
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

function stageEnv(resources: string, python: string, appDir: string, reqs: string, cacheDir: string) {
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
    execFileSync("uv", uvExportArgs(exported), {
      cwd: appDir,
      stdio: "inherit",
    });
    reqsFile = exported;
  } else if (!existsSync(reqs)) {
    console.log(`no uv.lock and no ${reqs} — bundling stdlib only`);
    return;
  } else {
    console.warn(
      "warning: no uv.lock — building from requirements.txt; " +
        "the bundled env may drift from `pylair dev`'s env",
    );
    requireHashes = requirementsHaveHashes(readFileSync(reqs, "utf-8"));
  }
  execFileSync(
    "uv",
    uvPipInstallArgs(python, target, reqsFile, requireHashes),
    { cwd: appDir, stdio: "inherit" },
  );
}

function run(cmd: string, args: string[], cwd: string) {
  const r = spawnSync(cmd, args, { cwd, stdio: "inherit" });
  if (r.error) throw new Error(`${cmd} ${args.join(" ")} failed to start: ${r.error.message}`);
  if (r.status !== 0) throw new Error(`${cmd} ${args.join(" ")} exited ${r.status}`);
}

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
  console.warn(
    `warning: ${failed.length} file(s) failed to byte-compile and are left` +
      ` uncompiled — they will compile (or fail to import) as normal at` +
      ` runtime:\n  ${shown}${failed.length > 10 ? `\n  …and ${failed.length - 10} more` : ""}`,
  );
  return failed;
}

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
        ? !!process.env.WINDOWS_CERTIFICATE_THUMBPRINT
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
