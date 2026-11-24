import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { after } from "node:test";

import { keysInit, PUBKEY_PLACEHOLDER } from "../dist/keys.js";

const dirs = [];
const tmp = () => {
  const d = mkdtempSync(join(tmpdir(), "pylair-keys-"));
  dirs.push(d);
  return d;
};
after(() => {
  for (const d of dirs) rmSync(d, { recursive: true, force: true });
});

/** An updater-enabled app: conf carrying the placeholder pubkey, plus a fake
 * @tauri-apps/cli so tauriCliEntry() resolves (the generate call is stubbed —
 * the real `signer generate` shape is proved by the u0 spike). */
function fakeApp({ pubkey = PUBKEY_PLACEHOLDER, updater = true } = {}) {
  const dir = tmp();
  const tauri = join(dir, "src-tauri");
  mkdirSync(tauri, { recursive: true });
  const conf = { productName: "up-app", version: "0.1.0", identifier: "com.example.up" };
  if (updater) conf.plugins = { updater: { pubkey, endpoints: ["https://x/latest.json"] } };
  writeFileSync(join(tauri, "tauri.conf.json"), JSON.stringify(conf, null, 2));
  mkdirSync(join(dir, "node_modules/@tauri-apps/cli"), { recursive: true });
  writeFileSync(join(dir, "node_modules/@tauri-apps/cli/tauri.js"), "// stub\n");
  return dir;
}

function confOf(dir) {
  return JSON.parse(readFileSync(join(dir, "src-tauri/tauri.conf.json"), "utf-8"));
}

/** Stubbed ops: generate writes <key> and <key>.pub like `signer generate`;
 * checkIgnored answers are queued per test. */
function ops({ ignored = "untracked", genCodes = [0] } = {}) {
  const calls = { generate: [], checkIgnored: [] };
  const queue = [...genCodes];
  return {
    calls,
    generate: (args, mode, stdin) => {
      calls.generate.push({ args, mode });
      const w = args.indexOf("-w");
      if (queue[0] === 0) {
        writeFileSync(args[w + 1], "PRIVATE-KEY-BYTES");
        writeFileSync(`${args[w + 1]}.pub`, "PUBKEY-TEST\n");
      }
      return queue.length > 1 ? queue.shift() : queue[0];
    },
    checkIgnored: (path, cwd) => {
      calls.checkIgnored.push({ path, cwd });
      return ignored;
    },
  };
}

describe("keysInit", () => {
  test("writes the pubkey into tauri.conf.json and the private key outside the project", async () => {
    const dir = fakeApp();
    const o = ops();
    const code = await keysInit({ appDir: dir, keyPath: join(tmp(), "out", "app.key"), ops: o });
    assert.equal(code, 0);
    assert.equal(confOf(dir).plugins.updater.pubkey, "PUBKEY-TEST");
    const gen = o.calls.generate[0];
    assert.equal(gen.args[1], "signer");
    assert.equal(gen.args[2], "generate");
    // Outside the project: no git check is needed at all.
    assert.equal(o.calls.checkIgnored.length, 0);
  });

  test("refuses a key path inside the project that git would track", async () => {
    const dir = fakeApp();
    const o = ops(); // checkIgnored -> "untracked"
    await assert.rejects(
      keysInit({ appDir: dir, keyPath: "secret.key", ops: o }),
      /refusing to write the private key inside the project/,
    );
    // Nothing was generated and the conf still holds the placeholder.
    assert.equal(o.calls.generate.length, 0);
    assert.equal(confOf(dir).plugins.updater.pubkey, PUBKEY_PLACEHOLDER);
    assert.ok(!existsSync(join(dir, "secret.key")));
  });

  test("refuses an inside-project path it cannot verify (no git repo)", async () => {
    const dir = fakeApp();
    const o = ops({ ignored: "unknown" });
    await assert.rejects(
      keysInit({ appDir: dir, keyPath: "keys/app.key", ops: o }),
      /refusing to write the private key inside the project/,
    );
  });

  test("allows an inside-project path that is git-ignored", async () => {
    const dir = fakeApp();
    const o = ops({ ignored: "ignored" });
    const code = await keysInit({ appDir: dir, keyPath: "keys/app.key", ops: o });
    assert.equal(code, 0);
    assert.equal(o.calls.checkIgnored.length, 1);
    assert.ok(existsSync(join(dir, "keys/app.key")));
  });

  test("refuses an existing key file without --force", async () => {
    const dir = fakeApp();
    const keyPath = join(tmp(), "existing.key");
    writeFileSync(keyPath, "OLD");
    await assert.rejects(keysInit({ appDir: dir, keyPath, ops: ops() }), /already exists.*--force/s);
  });

  test("refuses an existing pubkey without --force, naming the rollover rule", async () => {
    const dir = fakeApp({ pubkey: "dW50cnVzdGVkIGNvbW1lbnQ=" });
    await assert.rejects(
      keysInit({ appDir: dir, keyPath: join(tmp(), "k"), ops: ops() }),
      /pubkey is already set.*signed by the OLD key/s,
    );
  });

  test("--force replaces an existing pubkey", async () => {
    const dir = fakeApp({ pubkey: "OLDKEY" });
    const code = await keysInit({
      appDir: dir,
      keyPath: join(tmp(), "k2"),
      force: true,
      ops: ops(),
    });
    assert.equal(code, 0);
    assert.equal(confOf(dir).plugins.updater.pubkey, "PUBKEY-TEST");
  });

  test("an app without plugins.updater is refused with the fix", async () => {
    const dir = fakeApp({ updater: false });
    await assert.rejects(
      keysInit({ appDir: dir, keyPath: join(tmp(), "k"), ops: ops() }),
      /no plugins\.updater block.*--with-updater/s,
    );
  });

  test("non-interactive with no --key-path fails naming the flag", async () => {
    const dir = fakeApp();
    await assert.rejects(keysInit({ appDir: dir, ops: ops() }), /--key-path/);
  });

  test("password comes from --password or TAURI_SIGNING_PRIVATE_KEY_PASSWORD", async () => {
    const dir = fakeApp();
    const o = ops();
    await keysInit({ appDir: dir, keyPath: join(tmp(), "k"), password: "hunter2", ops: o });
    assert.deepEqual(o.calls.generate[0].args.slice(-2), ["-p", "hunter2"]);

    const dir2 = fakeApp();
    const o2 = ops();
    await keysInit({
      appDir: dir2,
      keyPath: join(tmp(), "k2"),
      env: { TAURI_SIGNING_PRIVATE_KEY_PASSWORD: "envpw" },
      ops: o2,
    });
    assert.deepEqual(o2.calls.generate[0].args.slice(-2), ["-p", "envpw"]);
  });
});
