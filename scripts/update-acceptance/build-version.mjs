// Acceptance harness (decision 0011, PR 4): build one app variant and
// stage its release directory.
//
//   node build-version.mjs --app <dir> --variant <name> --version <semver>
//                          --tag <tag> --out <htdocs> --bundles <list>
//                          [--install-mode <m>] [--crash] [--skip-fetch]
//                          [--port 8791]
//
// Variants (backend/app.py tails — the @@VARIANT_BODY@@ slot):
//   base    v1 baseline — instant "accept" component
//   good    registers pylair.update.on_update_confirmed + state watcher
//   slow    good + a component reporting progress ~45s (outlives the
//           10s heartbeat, under the 150s hard cap — must still CONFIRM)
//   broken  on_startup raises — readiness_failed rollback
//   crash   lib.rs exit thread + never-ready backend — crash_loop
//
// Per build: conf version + Cargo.toml synced (patch-conf.mjs), backend
// + frontend fixtures written, `pylair build --sign`, updater artifact +
// .sig + latest.json staged under <out>/releases/<tag>/, and the raw
// installable under <out>/installable/<tag>/.
//
// macOS: after tauri build the .app is re-signed with the hardened
// runtime (codesign --force --deep --options runtime -s -), the updater
// tarball is repacked from THAT app and re-signed — so the staged
// watchdog copy and the installed bundle both carry hardened-runtime
// rules (PR 4's "runs outside the .app" evidence).

import { execSync } from "node:child_process";
import {
  copyFileSync,
  cpSync,
  existsSync,
  mkdirSync,
  rmSync,
  readFileSync,
  readdirSync,
  writeFileSync,
  appendFileSync,
} from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));

function arg(flag) {
  const i = process.argv.indexOf(flag);
  return i === -1 ? undefined : process.argv[i + 1];
}
const has = (flag) => process.argv.includes(flag);
const need = (flag) => {
  const v = arg(flag);
  if (v === undefined) throw new Error(`missing ${flag}`);
  return v;
};

const appDir = resolve(need("--app"));
const variant = need("--variant");
const version = need("--version");
const tag = need("--tag");
const out = resolve(need("--out"));
const bundles = need("--bundles");
const port = arg("--port") ?? "8791";

const VARIANTS = {
  base: 'ready("accept")',
  good: `pylair_update.on_update_confirmed(lambda: _log("confirmed", VERSION))
    ready("accept")`,
  // ~45s of loading reports INSIDE the hook — readiness stays blocked
  // while the pending window's progress.json advances (heartbeat is
  // 10s in the CI conf, hard cap 150s). A daemon thread would report
  // past startup end and the readiness sweep marks unmanaged loading
  // components failed — that is what bit the first local run.
  slow: `pylair_update.on_update_confirmed(lambda: _log("confirmed", VERSION))
    for _i in range(30):
        loading("model", _i / 30.0)
        time.sleep(1.5)
    ready("model")
    ready("accept")`,
  broken: 'raise RuntimeError("acceptance fixture: v3 on_startup always fails")',
  crash: "time.sleep(300)",
};

const q = (s) => `"${String(s).replace(/"/g, '\\"')}"`;
function run(cmd, argv, opts = {}) {
  const line = [cmd, ...argv.map(q)].join(" ");
  console.log(`$ ${line}`);
  // execSync → OS shell: pnpm/tauri resolve via PATH on every OS (the
  // Windows shims are .cmd files execFile can't spawn directly).
  execSync(line, { stdio: "inherit", cwd: appDir, ...opts });
}

const t0 = Date.now();

// 1. conf + Cargo.toml (patch-conf owns the https assertion + the
//    loopback/insecure/knob edits).
const patchArgs = [
  join(HERE, "patch-conf.mjs"),
  "--app",
  appDir,
  "--version",
  version,
  "--port",
  port,
];
if (has("--install-mode")) patchArgs.push("--install-mode", need("--install-mode"));
execSync(["node", ...patchArgs.map(q)].join(" "), { stdio: "inherit" });

// 2. backend fixture with placeholders filled.
let backend = readFileSync(join(HERE, "fixtures", "backend.py.tmpl"), "utf-8");
const body = VARIANTS[variant];
if (!body) throw new Error(`unknown variant ${variant} (have ${Object.keys(VARIANTS)})`);
backend = backend.replaceAll("@@VERSION@@", version).replaceAll("@@VARIANT_BODY@@", body);
if (backend.includes("@@")) throw new Error("backend template placeholders unfilled");
writeFileSync(join(appDir, "backend", "app.py"), backend);

// 3. frontend driver (idempotent — replaces the scaffold's demo UI).
copyFileSync(join(HERE, "fixtures", "App.tsx"), join(appDir, "src", "App.tsx"));

// 4. lib.rs: restore pristine, then insert the crash hook when asked.
const libPath = join(appDir, "src-tauri", "src", "lib.rs");
const origPath = `${libPath}.accept-orig`;
if (!existsSync(origPath)) copyFileSync(libPath, origPath);
let lib = readFileSync(origPath, "utf-8");
if (has("--crash")) {
  const hook = readFileSync(join(HERE, "fixtures", "crash-hook.rs"), "utf-8");
  const anchor = "    tauri::Builder::default()";
  if (!lib.includes(anchor)) throw new Error("lib.rs: no tauri::Builder anchor");
  lib = lib.replace(anchor, `${hook}${anchor}`);
}
writeFileSync(libPath, lib);

// 5. build: --sign forces createUpdaterArtifacts (also already set by the
//    scaffold) and fails fast without TAURI_SIGNING_PRIVATE_KEY.
//    `update manifest` scans the bundle dir and refuses ambiguity, so
//    drop previous variants' artifacts first — the exe cache
//    (target/release minus bundle/) survives → incremental compiles.
rmSync(join(appDir, "src-tauri/target/release/bundle"), { recursive: true, force: true });
const buildArgs = ["exec", "pylair", "build", "--dir", appDir, "--bundles", bundles, "--sign"];
if (has("--skip-fetch")) buildArgs.push("--skip-fetch");
run("pnpm", buildArgs);

// 6. macOS: hardened-runtime ad-hoc sign the .app, repack the updater
//    tarball from it, re-sign the tarball (evidence for the
//    "watchdog runs outside the .app" claim).
const bundleDir = join(appDir, "src-tauri", "target", "release", "bundle");
if (process.platform === "darwin") {
  const macosDir = join(bundleDir, "macos");
  const app = readdirSync(macosDir).find((f) => f.endsWith(".app"));
  if (!app) throw new Error(`no .app under ${macosDir}`);
  const appPath = join(macosDir, app);
  run("codesign", ["--force", "--deep", "--options", "runtime", "-s", "-", appPath]);
  run("codesign", ["-dv", "--verbose=4", appPath]);
  const tar = `${app}.tar.gz`;
  const tarPath = join(macosDir, tar);
  // tar the way tauri does: the .app at the archive root.
  execSync(`tar czf "${tar}" "${app}"`, { cwd: macosDir, stdio: "inherit" });
  // Re-sign the repacked tarball. The workflow exports BOTH the key
  // content (TAURI_SIGNING_PRIVATE_KEY — `sign` picks it up as -k) and
  // its path (UPD_KEY_PATH); -f alongside the env key is a clap
  // conflict, so -f only when no env key is present. -p "" matches the
  // --no-password generation. --app-version re-binds the version in the
  // sig's trusted comment — `tauri build` sets it on its own sigs but a
  // manual signer call does not, and restore_update forces
  // require_signed_version (this broke the macOS rollback fetch).
  const keyArgs = [
    ...(process.env.UPD_KEY_PATH && !process.env.TAURI_SIGNING_PRIVATE_KEY
      ? ["-f", process.env.UPD_KEY_PATH]
      : []),
    "-p",
    "",
  ];
  run("pnpm", ["exec", "tauri", "signer", "sign", ...keyArgs, "--app-version", version, tarPath]);
}

// 7. stage <out>/releases/<tag>/ — artifact + .sig + latest.json.
const KIND_FILE = {
  appimage: /\.appimage$/i,
  macos: /\.app\.tar\.gz$/i,
  nsis: /-setup\.exe$/i,
};
const relDir = join(out, "releases", tag);
rmSync(relDir, { recursive: true, force: true });
mkdirSync(relDir, { recursive: true });
// Tauri bundle names vs the dirs they land in: `app` → bundle/macos.
const DIR_FOR_KIND = { app: "macos" };
const bundleKinds = bundles.split(",").map((k) => DIR_FOR_KIND[k] ?? k);
let artifact;
for (const kind of bundleKinds) {
  const dir = join(bundleDir, kind);
  if (!existsSync(dir)) continue;
  for (const f of readdirSync(dir)) {
    if (!KIND_FILE[kind]?.test(f)) continue;
    artifact = { kind, file: f, path: join(dir, f) };
    copyFileSync(artifact.path, join(relDir, f));
    if (existsSync(`${artifact.path}.sig`))
      copyFileSync(`${artifact.path}.sig`, join(relDir, `${f}.sig`));
  }
}
if (!artifact) throw new Error(`no updater artifact under ${bundleDir} (${bundles})`);

run("pnpm", [
  "exec",
  "pylair",
  "update",
  "manifest",
  "--dir",
  appDir,
  "--base-url",
  `http://127.0.0.1:${port}/releases/${tag}`,
  "--insecure-dev",
  "--out",
  join(relDir, "latest.json"),
]);

// 8. stage <out>/installable/<tag>/ — what the scenario installs first.
const instDir = join(out, "installable", tag);
rmSync(instDir, { recursive: true, force: true });
mkdirSync(instDir, { recursive: true });
if (artifact.kind === "macos") {
  const app = readdirSync(join(bundleDir, "macos")).find((f) => f.endsWith(".app"));
  cpSync(join(bundleDir, "macos", app), join(instDir, app), { recursive: true });
} else {
  copyFileSync(artifact.path, join(instDir, artifact.file));
}

// 9. timing line for the PR body's per-leg wall times.
appendFileSync(
  join(out, "timings.jsonl"),
  JSON.stringify({ tag, variant, ms: Date.now() - t0 }) + "\n",
);
console.log(`${tag} (${variant} ${version}): built + staged in ${((Date.now() - t0) / 1000).toFixed(0)}s`);
