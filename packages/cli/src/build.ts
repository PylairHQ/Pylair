/** `pylair build`, the M0 recipe, generalized:
 *   fetch standalone CPython, stage site-packages via the bundled
 *   interpreter (native wheels only, no cross-compile), stage app
 *   source, prune, tauri build, unsigned warning.
 *
 * Output goes through ui/ (one step line per stage with its time; the tauri
 * build folded into one progress line on a live terminal; plain, stable text
 * everywhere else).
 */

/*
 * Where each step lives. build() below calls into them; this file also
 * re-exports the helpers other modules and the tests import from here.
 *
 *   build/pbs.ts       python-build-standalone release, versions, host
 *                      triples, committed SHA-256 table, hostTriple()
 *   build/download.ts  the interpreter download: retry, resume, progress,
 *                      timeout, cache and SHA-256 check (ensureTarball)
 *   build/python.ts    fetchPython (unpack CPython), uv export and
 *                      `uv pip install` into site-packages (stageEnv)
 *   build/source.ts    stageSource: copy the app source, refuse secrets
 *   build/notices.ts   stageNotices: THIRD-PARTY-NOTICES for the bundle
 *   build/bytecode.ts  compileBytecode: checked-hash pycs for the bundle
 *   build/sign.ts      macOS ad-hoc signing of the built .app (adHocSign)
 *   build/artifacts.ts the installers a build produced, for the summary
 *   build/util.ts      isWin, run() and runTool(), shared by build() and the modules
 *   ./prune.ts         pruning the staged tree (DEFAULT_PRUNE, prune)
 *   ./config.ts        [tool.pylair] and tauri.conf.json loading
 */

import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { arch, platform, tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import {
  importRoot,
  loadConfig,
  loadTauriConfig,
  tauriCliEntry,
  windowsSigningConfigured,
} from "./config.js";
import { assertCapabilities, checkPatterns, formatFinding, loadCapabilities } from "./capabilities.js";
import { loadManifest } from "./manifest.js";
import { emit } from "./codegen.js";
import { DEFAULT_PRUNE, prune } from "./prune.js";
import { findArtifacts } from "./build/artifacts.js";
import { compileBytecode } from "./build/bytecode.js";
import { stageNotices } from "./build/notices.js";
import { hostTriple } from "./build/pbs.js";
import { fetchPython, stageEnv } from "./build/python.js";
import { adHocSign } from "./build/sign.js";
import { stageSource } from "./build/source.js";
import { isWin, runTool } from "./build/util.js";
import { assertUpdaterVersionSync } from "./update.js";
import { formatDuration, formatSize, ui } from "./ui/index.js";
import { wrapWarnings } from "./wrapconfig.js";
import {
  ROOT_SOURCE_EXCLUDES,
  STAGED_APP,
  runSmokeImports,
  smokeImports,
  sourceIsRoot,
  writeRuntimeFile,
} from "./wrapbuild.js";

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
  /** `pylair build --sign`: produce signed updater artifacts — forces
   * bundle.createUpdaterArtifacts and requires TAURI_SIGNING_PRIVATE_KEY
   * in the environment (0011 §6). */
  sign?: boolean;
  /** Skip the interpreter fetch (reuse staged resources/python). */
  skipFetch?: boolean;
  /** Stage and verify everything (interpreter, locked env, source, prune,
   * bytecode, the import check) but stop before `tauri build`: no installer,
   * no signing. What CI and `pylair build --stage-only` use to prove a
   * project builds without paying for a native bundle. */
  stageOnly?: boolean;
  /** Stream the uv and tauri output as it comes instead of folding it into
   * the live progress line (`pylair build --verbose`). */
  verbose?: boolean;
}

const UNSIGNED_WARNING =
  "UNSIGNED BUILD: Gatekeeper and SmartScreen will warn users (see docs/build.md#signing)";

/**
 * The typed stubs are a build artifact of the BACKEND, owned by
 * `pylair build` (docs/codegen.md): a fresh clone has no api.generated.ts,
 * so the frontend typecheck inside `tauri build` can only pass if the
 * stubs are regenerated first. Same codegen `pylair dev` runs — import the
 * app module, emit typed wrappers — but against the bundled interpreter
 * with the staged site-packages, because a build machine has no venv and
 * the deps are exactly what the app will ship. A import failure is a build
 * failure with the python error named: shipping an installer whose
 * frontend compiled against stale or absent stubs is never right.
 */
export function runBuildCodegen(
  appDir: string,
  cfg: { appModule: string; source: string },
  python: string,
  resources: string,
): string {
  const sep = isWin ? ";" : ":";
  const out = join(appDir, "src", "api.generated.ts");
  mkdirSync(join(appDir, "src"), { recursive: true });
  let manifest;
  try {
    manifest = loadManifest({
      python,
      module: cfg.appModule,
      cwd: tmpdir(),
      env: {
        PYTHONPATH: `${importRoot(appDir, cfg.source)}${sep}${join(resources, "site-packages")}`,
        PYTHONNOUSERSITE: "1",
        PYTHONDONTWRITEBYTECODE: "1",
      },
    });
  } catch (e) {
    throw new Error(
      `pylair build: codegen failed to import '${cfg.appModule}' with the bundled python: ` +
        `${(e as Error).message}. The typed stubs are required - the frontend typecheck ` +
        `inside the installer build cannot pass without them (docs/codegen.md)`,
    );
  }
  const ts = emit(manifest, cfg.appModule);
  if (!existsSync(out) || readFileSync(out, "utf-8") !== ts) writeFileSync(out, ts);
  return out;
}

export async function build(opts: BuildOptions): Promise<void> {
  const { appDir } = opts;
  const verbose = opts.verbose ?? false;
  const started = Date.now();
  const tauriDir = join(appDir, "src-tauri");
  const resources = join(tauriDir, "resources");
  const tconf = loadTauriConfig(appDir);

  const cfg = loadConfig(appDir);
  const wrap = cfg.mode === "wrap" ? cfg.wrap! : undefined;
  if (wrap) for (const w of wrapWarnings(wrap)) ui.warn(`warning: ${w}`);

  // Capability scopes are checked first: a bad `allow: []` or a half-scoped
  // window is a configuration error, not worth a Python download to find out.
  for (const w of assertCapabilities(appDir)) ui.warn(formatFinding(w), { bare: true });

  // Updater-enabled apps: tauri.conf.json's version is what the updater
  // compares an offer against; Cargo.toml's is what the binary reports —
  // they must match or an updated app misreports itself (0011 §1). Checked
  // for every build once plugins.updater exists, and whenever --sign asks
  // for updater artifacts.
  if (tconf.updater || opts.sign) assertUpdaterVersionSync(tauriDir, tconf.version);
  if (opts.sign) {
    if (!process.env.TAURI_SIGNING_PRIVATE_KEY)
      throw new Error(
        "pylair build --sign: TAURI_SIGNING_PRIVATE_KEY is not set - generate " +
          "the keypair with `pylair keys init` and put the private key in CI " +
          "secrets (docs/updates.md)",
      );
    if (!tconf.updater)
      ui.warn(
        "warning: --sign produces updater artifacts but tauri.conf.json has no " +
          "plugins.updater block - nothing will consume them (docs/updates.md)",
        { bare: true },
      );
  }

  const targetTriple = hostTriple(); // cross-compile refused by construction
  ui.header(
    "pylair build",
    [
      ["app", tconf.productName],
      ["version", tconf.version],
      ["platform", `${platform()}/${arch()} (${targetTriple}), native build only`],
      ["mode", wrap ? `wrap (${wrap.framework})` : "bridge"],
    ],
    [
      `building ${tconf.productName} ${tconf.version} for ${platform()}/${arch()} (${targetTriple})`,
      "note: pylair builds native bundles only; run this on each target OS",
    ],
  );
  const stages = [
    "bundle python",
    "install deps",
    ...(wrap ? [] : ["codegen"]),
    "stage app",
    "prune",
    "compile bytecode",
    "smoke test",
    ...(opts.stageOnly ? [] : ["build installer"]),
  ];
  ui.setStepWidth(Math.max(...stages.map((s) => s.length)));

  const cacheDir = join(appDir, ".pylair", "cache");
  const bundled = await ui.step("bundle python", async (s) => {
    if (opts.skipFetch) {
      s.detail("reusing staged python");
      return join(resources, "python", isWin ? "python.exe" : "bin/python3");
    }
    return fetchPython(resources, cacheDir, cfg.pythonVersion);
  });

  await ui.step("install deps", (s) =>
    stageEnv(resources, bundled, appDir, join(appDir, cfg.requirements), cacheDir, { step: s, verbose }),
  );

  if (!wrap) {
    await ui.step("codegen", (s) => {
      runBuildCodegen(appDir, cfg, bundled, resources);
      s.detail("src/api.generated.ts");
    });
  }

  // Stage app source so resources/ mirrors the installed layout.
  // Wrap mode stages the app as `app/` whatever the source dir is called, so
  // tauri.conf.json's resource mapping never changes with `source`.
  const staged = wrap ? STAGED_APP : cfg.source;
  const srcDir = join(appDir, cfg.source);
  await ui.step("stage app", () => {
    // Licenses ship with the bundle: collect before prune can remove sources.
    stageNotices(resources, fileURLToPath(new URL("..", import.meta.url)));
    if (!existsSync(srcDir)) throw new Error(`source dir ${srcDir} missing`);
    if (wrap?.entry && !existsSync(join(srcDir, wrap.entry)))
      throw new Error(`[tool.pylair.wrap] entry ${wrap.entry} not found under ${srcDir}`);
    rmSync(join(resources, staged), { recursive: true, force: true });
    stageSource(
      srcDir,
      join(resources, staged),
      cfg.include,
      undefined,
      wrap && sourceIsRoot(appDir, cfg.source) ? ROOT_SOURCE_EXCLUDES : [],
    );
    // What the plugin reads at startup: the launcher command and the window options.
    if (wrap) writeRuntimeFile(join(resources, "pylair-wrap.json"), cfg, STAGED_APP);
  });

  await ui.step("prune", (s) => {
    const res = prune(resources, [...DEFAULT_PRUNE, ...cfg.prune]);
    const mib = (res.bytesFreed / 2 ** 20).toFixed(1);
    if (ui.caps.fancy) s.detail(`${res.removed} paths, ${mib} MiB freed`);
    else ui.line(`pruned ${res.removed} paths (${mib} MiB) from staged resources`);
  });

  // Precompile what survives the prune: read-only installs cannot
  // write pycs at launch, so the bundle ships them ready-made.
  await ui.step("compile bytecode", (s) => {
    const failed = compileBytecode(resources, bundled, ["site-packages", staged]);
    if (failed.length) s.detail(`${failed.length} file(s) left uncompiled`);
  });

  // Over-pruning must fail at build time, not on the user's machine:
  // import the app (bridge) or the framework (wrap) with the staged tree
  // exactly as the bundle sees it.
  const sep = isWin ? ";" : ":";
  await ui.step("smoke test", (s) => {
    if (wrap) {
      // The server runs in the staged app dir; the framework (gradio,
      // streamlit, or the `-m` module of a command) must import with the
      // bundled interpreter after the prune.
      const failure = runSmokeImports(
        bundled,
        smokeImports(wrap),
        {
          PYTHONPATH: `${join(resources, STAGED_APP)}${sep}${join(resources, "site-packages")}`,
          PYTHONNOUSERSITE: "1",
          PYTHONDONTWRITEBYTECODE: "1",
        },
        tmpdir(),
      );
      if (failure)
        throw new Error(
          `staged bundle failed the framework import check (${smokeImports(wrap).join(", ")}): ` +
            `pruning or missing deps? ${failure}`,
        );
      s.detail(`imports ${smokeImports(wrap).join(", ")}`);
    } else {
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
          `staged bundle failed the import check (${cfg.appModule}): ` +
            `pruning or missing deps? ${check.stderr?.trim()}`,
        );
      s.detail(`imports ${cfg.appModule}`);
    }

    // With the app importable, ask it which functions it exposes and warn
    // about scope patterns that match none. Best effort: a failure here only
    // skips the warning, it never fails the build.
    const caps = wrap ? [] : loadCapabilities(appDir).capabilities;
    if (caps.length) {
      try {
        const m = loadManifest({
          python: bundled,
          module: cfg.appModule,
          cwd: tmpdir(),
          env: {
            PYTHONPATH: `${importRoot(resources, cfg.source)}${sep}${join(resources, "site-packages")}`,
            PYTHONNOUSERSITE: "1",
            PYTHONDONTWRITEBYTECODE: "1",
          },
        });
        const names = (m.functions ?? []).map((f) => f.name);
        for (const w of checkPatterns(caps, names)) ui.warn(formatFinding(w), { bare: true });
      } catch (e) {
        ui.note(
          `note: skipped the capability pattern check (could not list exposed functions: ${(e as Error).message.split("\n")[0]})`,
        );
      }
    }
  });

  if (opts.stageOnly) {
    ui.line();
    ui.summary(
      "ok",
      `stage-only: resources staged and verified; tauri build skipped (${formatDuration(Date.now() - started)})`,
    );
    return;
  }

  // Signing is per-platform: only macOS creds (env OR tauri.conf
  // signingIdentity) mean this Mac build gets a real signature; a Windows
  // thumbprint on its own must not suppress the ad-hoc pass.
  // NOTE: TAURI_SIGNING_PRIVATE_KEY signs *updater* artifacts, not the app,
  // so it must NOT silence the unsigned warning.
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
        : true; // no signing concept on Linux: don't warn

  const tauriArgs = opts.tauriArgs ?? [];
  // --sign forces bundle.createUpdaterArtifacts via tauri's --config merge,
  // so a one-off signed build needs no conf edit. Merged last: it wins over
  // a --config the user forwarded only for this one key.
  if (opts.sign)
    tauriArgs.push("--config", JSON.stringify({ bundle: { createUpdaterArtifacts: true } }));
  const tauriStarted = Date.now();
  await ui.step("build installer", (s) =>
    runTool(ui, process.execPath, [tauriCliEntry(appDir), "build", ...tauriArgs], appDir, {
      step: s,
      verbose: verbose || tauriArgs.includes("--verbose"),
    }),
  );
  // Unsigned macOS builds get a full-bundle ad-hoc signature, better
  // than the linker stub Tauri leaves on just the main binary. Skipped
  // entirely when a real identity is configured: ad-hoc must never run
  // over a proper signature.
  if (platform() === "darwin" && !appleSigned)
    await ui.step("ad-hoc sign", () => adHocSign(tauriDir, tconf.productName));

  const artifacts = findArtifacts(tauriDir, tauriStarted, { debug: tauriArgs.includes("--debug") });
  ui.line();
  ui.summary("ok", `built ${tconf.productName} ${tconf.version} in ${formatDuration(Date.now() - started)}`);
  if (artifacts.length)
    ui.rows(
      artifacts.map((a) => [formatSize(a.bytes).padStart(9), a.path] as const),
      2,
    );
  else ui.note("  no installer found under the bundle directory (built with --no-bundle?)");
  if (!hasSigning) ui.warn(UNSIGNED_WARNING, { bare: true });
}
