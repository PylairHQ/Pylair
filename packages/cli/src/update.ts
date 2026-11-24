/** Auto-update support (decision 0011): `pylair update manifest` plus the
 * checks `pylair build` runs on updater-enabled apps.
 *
 * The manifest is Tauri's update JSON:
 *   {version, notes?, pub_date, platforms: {<os>-<arch>: {signature, url}}}
 * plus a per-platform `sha256` extension (unknown keys are ignored by the
 * plugin; it is the skip-list key of 0011 §3).
 */

import { createHash } from "node:crypto";
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readdirSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { parse as parseToml } from "smol-toml";
import { x as untar } from "tar";
import { loadTauriConfig, type TauriConfig } from "./config.js";
import { ui } from "./ui/index.js";

export interface UpdateManifestOptions {
  appDir: string;
  /** `--base-url`: the directory the manifest and artifacts are hosted at
   * (e.g. `https://…/releases/latest/download`). Falls back to the app's
   * `plugins.updater.endpoints[0]` minus its filename. Required otherwise. */
  baseUrl?: string;
  /** `--channel beta` writes `latest-beta.json`; default `latest.json`. */
  channel?: string;
  notes?: string;
  /** `--out`: manifest path (default `<target>/release/bundle/latest[-ch].json`). */
  out?: string;
  /** Allow an http:// base url — dev/CI only (0011 §1: https is mandatory
   * for shipped endpoints; the plugin itself refuses http by default). */
  insecureDev?: boolean;
  env?: NodeJS.ProcessEnv;
  /** Test seam for pub_date. */
  now?: () => Date;
}

/** bundle/<kind> directory → the manifest platform's OS half. */
const KIND_OS: Record<string, string> = {
  appimage: "linux",
  macos: "darwin",
  nsis: "windows",
};

/** Per bundle kind, what counts as an updater payload (its `.sig` sibling
 * is found separately). macOS has no version in the name. */
const KIND_FILE: Record<string, RegExp> = {
  appimage: /\.appimage$/i,
  macos: /\.app\.tar\.gz$/i,
  nsis: /-setup\.(exe|nsis\.zip)$/i,
};

/** Arch spellings → the rust-target spelling the manifest platform keys use
 * (`x86_64`/`aarch64`/`i686`): node `process.arch` names, rust-triple
 * prefixes, and the `amd64`/`x64` tokens artifact filenames carry. */
const ARCH_CANON: Record<string, string> = {
  x86_64: "x86_64",
  amd64: "x86_64",
  x64: "x86_64",
  aarch64: "aarch64",
  arm64: "aarch64",
  i686: "i686",
  i586: "i686",
  x86: "i686",
  ia32: "i686",
  armv7: "armv7",
};

/** Rust-triple arch prefixes (the first `-`-separated segment). */
const TRIPLE_ARCHS = new Set(Object.keys(ARCH_CANON).concat(["riscv64gc"]));

function canonArch(a: string): string {
  return ARCH_CANON[a.toLowerCase()] ?? a.toLowerCase();
}

/** `<product>_<version>_<arch>-setup.exe` etc. — the version embedded in an
 * artifact's name, when it carries one (semver-shaped token). */
function nameVersion(file: string): string | undefined {
  return file.match(/(\d+\.\d+\.\d+(?:-[0-9A-Za-z][0-9A-Za-z.-]*)?)/)?.[1];
}

function nameArch(file: string): string | undefined {
  const m = file.match(/(?:^|[_-])(aarch64|arm64|x86_64|x64|amd64|i686|i586|x86)(?:[_.-]|$)/i);
  return m ? canonArch(m[1]) : undefined;
}

/** Where tauri build writes bundles: `<target>/release/bundle` directly and
 * per `--target <triple>` one level down — same root set as findArtifacts. */
function bundleRoots(tauriDir: string, env: NodeJS.ProcessEnv): string[] {
  const target = env.CARGO_TARGET_DIR ? env.CARGO_TARGET_DIR : join(tauriDir, "target");
  const profile = "release";
  const roots = [join(target, profile, "bundle")];
  if (existsSync(target))
    for (const e of readdirSync(target, { withFileTypes: true }))
      if (e.isDirectory() && e.name !== "release" && e.name !== "debug")
        roots.push(join(target, e.name, profile, "bundle"));
  return roots;
}

export interface UpdaterArtifact {
  /** Absolute path. */
  path: string;
  /** Basename, for the manifest URL. */
  file: string;
  /** `linux-x86_64`, `windows-x86_64`, `darwin-aarch64`, … */
  platform: string;
  /** Version parsed from the filename (macOS tarballs carry none). */
  version?: string;
}

/** The signed updater artifacts under every bundle root, keyed by platform.
 * Arch comes from the `--target <triple>` dir name, then the filename, then
 * the host — a host build of `aarch64.app.tar.gz` still lands darwin-aarch64. */
export function findUpdaterArtifacts(
  tauriDir: string,
  env: NodeJS.ProcessEnv = process.env,
): UpdaterArtifact[] {
  const byPlatform = new Map<string, UpdaterArtifact>();
  for (const root of bundleRoots(tauriDir, env)) {
    if (!existsSync(root)) continue;
    // <target>/<triple>/release/bundle roots carry the arch in the triple
    // segment two levels up; the flat release/bundle root has none.
    const triple = root.split(/[\\/]/).at(-3) ?? "";
    const tripleArch = /^[a-z0-9_]+-/.test(triple) && TRIPLE_ARCHS.has(triple.split("-")[0])
      ? canonArch(triple.split("-")[0])
      : "";
    for (const kind of readdirSync(root, { withFileTypes: true })) {
      const os = KIND_OS[kind.name];
      if (!kind.isDirectory() || !os) continue;
      const re = KIND_FILE[kind.name];
      for (const file of readdirSync(join(root, kind.name))) {
        if (!re.test(file)) continue;
        const arch = tripleArch || nameArch(file) || canonArch(process.arch);
        const platform = `${os}-${arch}`;
        const artifact: UpdaterArtifact = {
          path: join(root, kind.name, file),
          file,
          platform,
          version: nameVersion(file),
        };
        const prev = byPlatform.get(platform);
        // Two spellings of the same payload (.exe and .nsis.zip): the exe wins.
        if (prev && !(prev.file.endsWith(".zip") && file.endsWith(".exe")))
          throw new Error(
            `multiple updater artifacts for ${platform}: ${prev.file} and ${file} ` +
              `(${join(root, kind.name)}) - clean the bundle dir and rebuild`,
          );
        byPlatform.set(platform, artifact);
      }
    }
  }
  return [...byPlatform.values()].sort((a, b) => a.platform.localeCompare(b.platform));
}

/** tauri.conf.json `version` must equal src-tauri/Cargo.toml `[package]
 * version`: the updater compares the offer against the CONF value baked into
 * the bundle, while `env!("CARGO_PKG_VERSION")` reads Cargo.toml — a drifted
 * pair makes an app report the wrong version after update (0011 §1). Run for
 * every build of an updater-enabled app. */
export function assertUpdaterVersionSync(tauriDir: string, confVersion: string): void {
  const cargoPath = join(tauriDir, "Cargo.toml");
  if (!existsSync(cargoPath)) return; // not a tauri app after all
  const doc = parseToml(readFileSync(cargoPath, "utf-8")) as {
    package?: { version?: string };
  };
  const cargoVersion = doc.package?.version;
  if (cargoVersion === undefined || cargoVersion === confVersion) return;
  throw new Error(
    `version mismatch: src-tauri/tauri.conf.json says "${confVersion}" but ` +
      `src-tauri/Cargo.toml [package] says "${cargoVersion}". The updater ` +
      "compares offers against tauri.conf.json; bump both or the app will " +
      "report a stale version after an update (decision 0011, section 1).",
  );
}

/** The manifest's artifact directory base: --base-url, or the app's own
 * endpoint minus its manifest filename. Trailing-slash normalized. */
function resolveBaseUrl(opts: UpdateManifestOptions, tconf: TauriConfig): string {
  let base = opts.baseUrl?.trim();
  if (!base) {
    const endpoint = tconf.updater?.endpoints?.[0];
    if (endpoint) {
      base = endpoint.replace(/\/[^/]*$/, "");
    }
  }
  if (!base)
    throw new Error(
      "no base url: pass --base-url https://host/dir (the directory the manifest " +
        "and artifacts are uploaded to), or configure plugins.updater.endpoints",
    );
  let url: URL;
  try {
    url = new URL(base);
  } catch {
    throw new Error(`--base-url is not a URL: ${base}`);
  }
  if (url.protocol !== "https:" && !opts.insecureDev)
    throw new Error(
      `--base-url must be https (${base}); endpoints are https-only outside dev ` +
        "(decision 0011, section 1). For a local dev server pass --insecure-dev.",
    );
  if (url.protocol !== "https:" && url.protocol !== "http:")
    throw new Error(`--base-url must be https: got ${url.protocol}// (${base})`);
  return base.replace(/\/+$/, "");
}

function sha256File(path: string): string {
  return createHash("sha256").update(readFileSync(path)).digest("hex");
}

/** The CFBundleShortVersionString inside a `<name>.app.tar.gz` updater
 * artifact: the filename carries no version, so without this check a stale
 * bundle (validly signed) would be published under the new version - and
 * Macs would keep being re-offered the old build. Only the matching
 * Contents/Info.plist entry is extracted; tauri writes it as XML. */
export function appTarVersion(path: string, file: string): string {
  const tmp = mkdtempSync(join(tmpdir(), "pylair-uptar-"));
  try {
    const plists: string[] = [];
    try {
      untar({
        file: path,
        cwd: tmp,
        sync: true,
        filter: (p) => /(^|\/)Contents\/Info\.plist$/.test(p),
        onentry: (e) => {
          if (e.type === "File") plists.push(e.path);
        },
      });
    } catch (e) {
      throw new Error(
        `${file}: not a readable .app.tar.gz (${(e as Error).message}) - ` +
          "was it produced by tauri bundle?",
      );
    }
    for (const p of plists) {
      const m = readFileSync(join(tmp, p), "utf-8").match(
        /<key>\s*CFBundleShortVersionString\s*<\/key>\s*<string>\s*([^<]+?)\s*<\/string>/,
      );
      if (m) return m[1].trim();
    }
    throw new Error(
      `${file}: no CFBundleShortVersionString in any Contents/Info.plist ` +
        "inside the tarball - not a macOS app bundle?",
    );
  } finally {
    rmSync(tmp, { recursive: true, force: true });
  }
}

/** Scan the bundle output for signed updater artifacts and write
 * `latest[-<channel>].json`. Returns the manifest path. Fails loudly on: no
 * artifacts, a missing `.sig`, artifacts whose embedded version disagrees
 * with tauri.conf.json, or a non-https base url. */
export function updateManifest(opts: UpdateManifestOptions): { file: string; platforms: string[] } {
  const tauriDir = join(opts.appDir, "src-tauri");
  const env = opts.env ?? process.env;
  const tconf = loadTauriConfig(opts.appDir);
  const version = tconf.version;
  if (typeof version !== "string" || !version)
    throw new Error(`tauri.conf.json has no "version" - the manifest needs one`);

  const artifacts = findUpdaterArtifacts(tauriDir, env);
  if (!artifacts.length)
    throw new Error(
      `no updater artifacts under ${tauriDir}/target/.../bundle - build signed ` +
        "updater artifacts first (`pylair build --sign`, or bundle." +
        "createUpdaterArtifacts + TAURI_SIGNING_PRIVATE_KEY)",
    );

  const base = resolveBaseUrl(opts, tconf);
  const platforms: Record<string, { signature: string; url: string; sha256: string }> = {};
  for (const a of artifacts) {
    // macOS tarballs carry no version in the name - read the bundled
    // Info.plist instead; other platforms carry it in the filename.
    const darwin = a.platform.startsWith("darwin-");
    const actual = darwin ? appTarVersion(a.path, a.file) : a.version;
    if (actual !== undefined && actual !== version)
      throw new Error(
        `artifact version mismatch: ${a.file} ` +
          (darwin
            ? `bundles CFBundleShortVersionString "${actual}" `
            : `is ${actual} `) +
          `but tauri.conf.json says ${version} - rebuild the version you're shipping`,
      );
    const sig = `${a.path}.sig`;
    if (!existsSync(sig))
      throw new Error(
        `unsigned updater artifact: ${a.file} has no ${a.file}.sig - ` +
          "was the build signed? (createUpdaterArtifacts + TAURI_SIGNING_PRIVATE_KEY)",
      );
    const signature = readFileSync(sig, "utf-8").trim();
    if (!signature) throw new Error(`empty signature file: ${sig}`);
    platforms[a.platform] = {
      signature,
      url: `${base}/${encodeURIComponent(a.file)}`,
      sha256: sha256File(a.path),
    };
  }

  const manifest: Record<string, unknown> = {
    version,
    ...(opts.notes ? { notes: opts.notes } : {}),
    pub_date: (opts.now ?? (() => new Date()))().toISOString(),
    platforms,
  };
  const name = `latest${opts.channel ? `-${opts.channel}` : ""}.json`;
  const out = opts.out ?? join(bundleRoots(tauriDir, env)[0], name);
  mkdirSync(dirname(out), { recursive: true });
  writeFileSync(out, JSON.stringify(manifest, null, 2) + "\n");
  return { file: out, platforms: Object.keys(platforms) };
}

/** `pylair update manifest`. */
export function updateManifestCommand(opts: UpdateManifestOptions): number {
  const { file, platforms } = updateManifest(opts);
  ui.ok(`update manifest: wrote ${file}`);
  ui.note(`  platforms: ${platforms.join(", ")}`);
  return 0;
}
