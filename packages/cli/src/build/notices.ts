/** THIRD-PARTY-NOTICES for the bundle: the licenses of everything it
 * embeds (pylair, the interpreter, every installed wheel). */

import {
  cpSync,
  existsSync,
  mkdirSync,
  readdirSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { createHash } from "node:crypto";
import type { get as httpsGet } from "node:https";
import { join } from "node:path";
import type { BinaryDecl } from "../config.js";
import { downloadWithRetry } from "./download.js";
import { ui } from "../ui/index.js";

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
    ui.warn("warning: no license files found under resources/python; wrote a placeholder NOTICE");
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
          ui.warn(`warning: bundled wheel ${e.name} declares a copyleft license`);
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
  ui.status(`third-party notices: pylair, python interpreter, ${wheels} wheel(s)`);
  return out;
}

/** GPL-family identifiers trigger the loud build warning (0014 sec. 7):
 * the build never blocks, but the obligation class must be impossible to
 * not think about. */
export function isGplFamily(spdx: string): boolean {
  return /^(GPL-2\.0|GPL-3\.0|AGPL-)/i.test(spdx.trim());
}

export const GPL_WARNING =
  "distributing a GPL binary imposes source-offer obligations on you; " +
  "LGPL requires the notice text you are shipping";

/** Binaries are the fourth notices source (0014 sec. 7): every declared
 * binary carries its license text into THIRD-PARTY-NOTICES/binaries/<name>/
 * - license_file copied verbatim, or license_url downloaded once into the
 * cache beside the binary downloads. Runs after stageNotices (which owns
 * the tree), appends its lines to INDEX.txt, and writes the
 * machine-readable index.json SBOM seed. */
export async function stageBinaryNotices(
  resources: string,
  appDir: string,
  binaries: Record<string, BinaryDecl>,
  triple: string,
  cacheDir: string,
  opts?: { get?: typeof httpsGet },
): Promise<void> {
  const names = Object.keys(binaries);
  if (!names.length) return;
  const out = join(resources, "THIRD-PARTY-NOTICES");
  const indexLines: string[] = [];
  const sbom: Record<string, unknown>[] = [];
  for (const name of names) {
    const decl = binaries[name]!;
    const dest = join(out, "binaries", name);
    mkdirSync(dest, { recursive: true });
    if (decl.licenseFile) {
      const src = join(appDir, decl.licenseFile);
      if (!existsSync(src))
        throw new Error(
          `binaries.${name}: license_file ${decl.licenseFile} does not exist (${src}) - ` +
            `the notice text is required in the bundle (0014 sec. 7)`,
        );
      cpSync(src, join(dest, basenameOf(decl.licenseFile)));
    } else if (decl.licenseUrl) {
      const cacheKey =
        "license-" + createHash("sha256").update(decl.licenseUrl).digest("hex").slice(0, 16);
      const cachePath = join(cacheDir, "binaries", cacheKey);
      if (!existsSync(cachePath)) {
        mkdirSync(join(cacheDir, "binaries"), { recursive: true });
        const tmp = `${cachePath}.download`;
        await downloadWithRetry(decl.licenseUrl, tmp, { get: opts?.get });
        writeFileSync(cachePath, readFileSync(tmp));
        rmSync(tmp, { force: true });
      }
      cpSync(cachePath, join(dest, "LICENSE.txt"));
    } else {
      throw new Error(
        `binaries.${name}: license_file or license_url is required - the bundle must carry ` +
          `the license text, not just the SPDX id (0014 sec. 7)`,
      );
    }
    if (isGplFamily(decl.license))
      ui.warn(
        `warning: binaries.${name} is ${decl.license} - ${GPL_WARNING}`,
        { bare: true },
      );
    const target = decl.targets[triple];
    let host = "";
    if (target?.url) {
      try {
        host = new URL(target.url).host;
      } catch {
        host = "";
      }
    }
    indexLines.push(
      `binaries/${name}/ - ${name}${decl.version ? " " + decl.version : ""} (${decl.license})${host ? ", " + host : ""}`,
    );
    sbom.push({
      name,
      ...(decl.version ? { version: decl.version } : {}),
      spdx: decl.license,
      ...(target?.url ? { source_url: target.url } : {}),
      ...(target ? { sha256: target.sha256 } : {}),
    });
  }
  const indexPath = join(out, "INDEX.txt");
  const prior = existsSync(indexPath) ? readFileSync(indexPath, "utf-8") : "";
  writeFileSync(indexPath, prior + indexLines.join("\n") + "\n");
  writeFileSync(join(out, "index.json"), JSON.stringify({ binaries: sbom }, null, 2) + "\n");
  ui.status(`binary notices: ${names.length} binar${names.length === 1 ? "y" : "ies"} recorded`);
}

function basenameOf(p: string): string {
  return p.replace(/\\/g, "/").split("/").pop() ?? p;
}
