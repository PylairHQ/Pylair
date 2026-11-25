// Acceptance harness (decision 0011, PR 4): patch the scaffolded app's
// tauri.conf.json + Cargo.toml for the CI run.
//
// The ONLY product-surface deviation CI makes is loopback http + the
// insecure-transport flag — asserted here, never baked into the
// templates. Two modes:
//
//   --check-placeholders   assert the scaffold still carries the https
//                          placeholder endpoints/rollbackUrl (run once
//                          right after create-pylair)
//   (default)              apply the endpoint/rollbackUrl/insecure/knob
//                          patches, set --version, and sync Cargo.toml
//
//   --app <dir>            app dir (default cwd parent app-under-test)
//   --version <v>          set conf.version AND Cargo.toml [package]
//   --install-mode <m>     bundle.windows.nsis.installMode
//   --port <n>             loopback server port (default 8791)

import { readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";

function arg(flag) {
  const i = process.argv.indexOf(flag);
  return i === -1 ? undefined : process.argv[i + 1];
}
const has = (flag) => process.argv.includes(flag);

const appDir = arg("--app") ?? "app-under-test";
const port = arg("--port") ?? "8791";
const confPath = join(appDir, "src-tauri", "tauri.conf.json");
const conf = JSON.parse(readFileSync(confPath, "utf-8"));

if (has("--check-placeholders")) {
  const ep = conf.plugins?.updater?.endpoints?.[0] ?? "";
  const rb = conf.plugins?.updater?.rollbackUrl ?? "";
  if (!ep.startsWith("https://") || !rb.startsWith("https://")) {
    console.error(
      `scaffold placeholders must stay https — got endpoints=${ep} rollbackUrl=${rb}`,
    );
    process.exit(1);
  }
  if (conf.plugins?.updater?.dangerousInsecureTransportProtocol) {
    console.error(
      "scaffold must NOT carry dangerousInsecureTransportProtocol — CI patches it in",
    );
    process.exit(1);
  }
  console.log(`placeholders ok: endpoint=${ep}`);
  process.exit(0);
}

const version = arg("--version");
if (version) {
  conf.version = version;
  const cargoPath = join(appDir, "src-tauri", "Cargo.toml");
  const cargo = readFileSync(cargoPath, "utf-8");
  // The [package] version must match tauri.conf.json — pylair build
  // refuses otherwise (assertUpdaterVersionSync). First version= line
  // after [package] is the target.
  const m = cargo.match(/(\[package\][\s\S]*?^version\s*=\s*")([^"]+)(")/m);
  if (!m) throw new Error(`${cargoPath}: no [package] version found`);
  writeFileSync(cargoPath, cargo.replace(m[0], `${m[1]}${version}${m[3]}`));
}

conf.plugins.updater.endpoints = [`http://127.0.0.1:${port}/latest/latest.json`];
conf.plugins.updater.rollbackUrl = `http://127.0.0.1:${port}/releases/v{{version}}/latest.json`;
// Loopback http is the only deviation CI allows itself — it exists
// precisely for this flag (decision 0011 §1). Never in the template.
conf.plugins.updater.dangerousInsecureTransportProtocol = true;

// Short knobs so the pending window fits CI time: a 10s heartbeat is
// what v2-slow must outlive (its ~45s progress must still CONFIRM);
// hardCap gives the slow variant wide margin.
conf.plugins.pylair = {
  ...(conf.plugins.pylair ?? {}),
  update: { heartbeatMs: 10000, maxBoots: 3, hardCapMs: 150000 },
};

// Always write the knob explicitly: builds share one conf file, so an
// unflagged build after a --install-mode perMachine build would inherit
// it silently (v1 landed in Program Files on exactly this bug).
const installMode = arg("--install-mode") ?? "currentUser";
conf.bundle.windows = conf.bundle.windows ?? {};
conf.bundle.windows.nsis = conf.bundle.windows.nsis ?? {};
conf.bundle.windows.nsis.installMode = installMode;

writeFileSync(confPath, JSON.stringify(conf, null, 2) + "\n");
console.log(
  `patched ${confPath}: version=${conf.version} endpoints→127.0.0.1:${port}` +
    (installMode ? ` installMode=${installMode}` : ""),
);
