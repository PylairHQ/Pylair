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
import { join } from "node:path";

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
