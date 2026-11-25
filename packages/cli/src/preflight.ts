/** Environment preflight: the fast tool check `pylair dev` runs before
 * the Rust build and `create-pylair` runs after scaffolding. These are
 * `pylair doctor`'s own detection primitives - the single source for
 * both callers.
 *
 * create-pylair cannot import @pylair/cli (it runs via `npx
 * create-pylair`, zero runtime dependencies), so
 * packages/create-pylair/scripts/sync-ui.mjs vendors this file beside
 * src/ui/ at build time and test/parity.test.js tripwires drift. Keep
 * it dependency-free: node builtins only - no ./ui, no ./doctor - and
 * pure ASCII (the parity test scans vendored sources). */

import { spawnSync } from "node:child_process";
import { existsSync, readdirSync } from "node:fs";
import { join } from "node:path";
import { platform } from "node:os";

/** Run a `--version`-style probe: trimmed output on exit 0, null on
 * failure ("" counts - `pkg-config --exists` prints nothing). */
export function probe(cmd: string, args: string[]): string | null {
  const r = spawnSync(cmd, args, { encoding: "utf-8" });
  return r.status === 0 ? (r.stdout || r.stderr).trim() : null;
}

/** The commands `nativeToolchain` consults. Injectable so tests stub the
 * platform and every tool answer: `run` returns trimmed output on exit 0
 * ("" counts: `pkg-config --exists` prints nothing), `null` on failure. */
export interface NativeEnv {
  platform: NodeJS.Platform;
  run: (cmd: string, args: string[]) => string | null;
  exists: (p: string) => boolean;
  /** Names inside a directory, [] when unreadable. */
  list: (dir: string) => string[];
  env: NodeJS.ProcessEnv;
}

const MSVC_FIX =
  'install "Desktop development with C++" via Visual Studio Installer, or repair it if an update is pending';

/** The C/C++ toolchain `pylair build` needs on this OS, past cargo itself:
 * MSVC build tools on Windows (OK only when vswhere at the standard
 * Installer path reports an instance with the VC tools component: rustc
 * locates MSVC via the setup instance registry vswhere reads, so files on
 * disk without a registered instance do not link; those get a repair hint),
 * Xcode Command Line Tools on macOS, a C compiler and the webkit2gtk-4.1 dev
 * files Tauri links against on Linux. */
export function nativeToolchain(e: NativeEnv): { ok: boolean; detail: string; fix?: string } {
  if (e.platform === "win32") {
    const pf86 = e.env["ProgramFiles(x86)"] ?? "C:\\Program Files (x86)";
    const pf = e.env.ProgramFiles ?? "C:\\Program Files";
    const vswhere = join(pf86, "Microsoft Visual Studio", "Installer", "vswhere.exe");
    if (e.exists(vswhere)) {
      const found = e.run(vswhere, [
        "-utf8",
        "-latest",
        "-products",
        "*",
        "-requires",
        "Microsoft.VisualStudio.Component.VC.Tools.x86.x64",
        "-property",
        "installationPath",
      ]);
      const dir = found?.split(/\r?\n/).find((l) => l.trim());
      if (dir) return { ok: true, detail: `MSVC build tools at ${dir}` };
    }
    // A link.exe on disk that vswhere does not see is a broken or
    // half-registered install: rustc will not find it either.
    for (const root of [join(pf, "Microsoft Visual Studio", "2022"), join(pf86, "Microsoft Visual Studio", "2019"), join(pf86, "Microsoft Visual Studio", "2017")]) {
      for (const ed of ["Enterprise", "Professional", "Community", "BuildTools", "Preview"]) {
        const msvc = join(root, ed, "VC", "Tools", "MSVC");
        if (e.list(msvc).some((v) => e.exists(join(msvc, v, "bin", "Hostx64", "x64", "link.exe"))))
          return {
            ok: false,
            detail: `MSVC tools found at ${join(root, ed)} but Visual Studio does not list the install`,
            fix: "repair it from Visual Studio Installer (More > Repair)",
          };
      }
    }
    return {
      ok: false,
      detail: "no MSVC C++ build tools found (link.exe missing)",
      fix: MSVC_FIX,
    };
  }
  if (e.platform === "darwin") {
    const p = e.run("xcode-select", ["-p"]);
    return p
      ? { ok: true, detail: `Xcode command line tools at ${p}` }
      : { ok: false, detail: "Xcode command line tools not found", fix: "run `xcode-select --install`" };
  }
  if (e.platform !== "linux")
    return { ok: true, detail: `${e.platform}: no toolchain requirements known` };

  const cc = e.run("cc", ["--version"]) ?? e.run("gcc", ["--version"]) ?? e.run("clang", ["--version"]);
  const hasPkgConfig = e.run("pkg-config", ["--version"]) !== null;
  const gtk = hasPkgConfig && e.run("pkg-config", ["--exists", "webkit2gtk-4.1"]) !== null;
  const missing: string[] = [];
  const pkgs: string[] = [];
  if (!cc) {
    missing.push("a C compiler (cc/gcc/clang)");
    pkgs.push("build-essential");
  }
  if (!hasPkgConfig) {
    missing.push("pkg-config");
    pkgs.push("pkg-config", "libwebkit2gtk-4.1-dev");
  } else if (!gtk) {
    missing.push("webkit2gtk-4.1 dev files");
    pkgs.push("libwebkit2gtk-4.1-dev");
  }
  if (missing.length)
    return { ok: false, detail: `missing: ${missing.join(", ")}`, fix: `sudo apt-get install -y ${pkgs.join(" ")}` };
  const gtkVer = e.run("pkg-config", ["--modversion", "webkit2gtk-4.1"]);
  return { ok: true, detail: `${cc?.split("\n")[0]} + webkit2gtk-4.1 ${gtkVer ?? "found"}` };
}

/** One preflight line: name, verdict, detail, and the one-line install
 * fix shown when the tool is missing. */
export interface PreflightItem {
  name: string;
  ok: boolean;
  detail: string;
  fix?: string;
}

/** The live environment for real runs (extracted so tests inject fakes). */
export function realEnv(): NativeEnv {
  return {
    platform: platform(),
    run: probe,
    exists: existsSync,
    list: (d) => {
      try {
        return readdirSync(d);
      } catch {
        return [];
      }
    },
    env: process.env,
  };
}

/** The fast subset of `pylair doctor`'s checks that gate a Rust build:
 * node (always ok - the CLI runs on it), uv (env sync), cargo (the
 * compile itself) and the platform's native toolchain. Everything is a
 * spawn-or-stat probe; a clean machine answers in well under a second. */
export function preflightChecks(e: NativeEnv = realEnv()): PreflightItem[] {
  const items: PreflightItem[] = [{ name: "node", ok: true, detail: process.version }];
  const uv = e.run("uv", ["--version"]);
  items.push({
    name: "uv",
    ok: uv !== null,
    detail: uv ?? "not found",
    fix: "install uv: https://docs.astral.sh/uv/",
  });
  const cargo = e.run("cargo", ["--version"]);
  items.push({
    name: "rust/cargo",
    ok: cargo !== null,
    detail: cargo ?? "not found",
    fix: "install Rust: https://rustup.rs",
  });
  const tc = nativeToolchain(e);
  items.push({ name: "native toolchain", ok: tc.ok, detail: tc.detail, fix: tc.fix });
  return items;
}

/** The failed subset of a preflight run. */
export function missingTools(items: PreflightItem[]): PreflightItem[] {
  return items.filter((i) => !i.ok);
}

/** One line per missing tool - the identical "clear list" `pylair dev`
 * prints before refusing a build and create-pylair prints as its
 * post-scaffold warning. */
export function formatMissing(missing: PreflightItem[]): string[] {
  return missing.map((m) => `${m.name}: ${m.detail}${m.fix ? ` - ${m.fix}` : ""}`);
}
