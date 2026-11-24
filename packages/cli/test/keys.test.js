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
 * the real `signer generate` shape is proved by the u0 spike). `dir` lets a
 * test nest the app inside a fake monorepo. */
function fakeApp({ pubkey = PUBKEY_PLACEHOLDER, updater = true, dir = tmp() } = {}) {
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
 * repoTop/checkIgnored answers are set per test. */
function ops({ ignored = "untracked", genCodes = [0], repoTop = undefined } = {}) {
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
    repoTop: () => repoTop,
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
    const code = await keysInit({
      appDir: dir,
      keyPath: join(tmp(), "out", "app.key"),
      noPassword: true,
      ops: o,
    });
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
    const code = await keysInit({
      appDir: dir,
      keyPath: "keys/app.key",
      noPassword: true,
      ops: o,
    });
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
      noPassword: true,
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

  test("a key path at the monorepo root is refused unless git-ignored", async () => {
    const repo = tmp();
    const dir = fakeApp({ dir: join(repo, "packages", "app") });
    await assert.rejects(
      keysInit({
        appDir: dir,
        keyPath: join(repo, "root.key"),
        ops: ops({ repoTop: repo }),
      }),
      /refusing to write the private key inside the repository.*root\.key/,
    );
    const dir2 = fakeApp({ dir: join(repo, "packages", "app2") });
    const code = await keysInit({
      appDir: dir2,
      keyPath: join(repo, "ignored.key"),
      noPassword: true,
      ops: ops({ ignored: "ignored", repoTop: repo }),
    });
    assert.equal(code, 0);
  });

  test("a key path in a sibling package is refused the same way", async () => {
    const repo = tmp();
    const dir = fakeApp({ dir: join(repo, "packages", "app") });
    await assert.rejects(
      keysInit({
        appDir: dir,
        keyPath: join(repo, "sibling", "k.key"),
        ops: ops({ repoTop: repo }),
      }),
      /inside the repository.*sibling[\\/]k\.key/,
    );
  });

  test("a key path outside the repository needs no git check", async () => {
    const repo = tmp();
    const dir = fakeApp({ dir: join(repo, "packages", "app") });
    const o = ops({ repoTop: repo });
    const code = await keysInit({
      appDir: dir,
      keyPath: join(tmp(), "far.key"),
      noPassword: true,
      ops: o,
    });
    assert.equal(code, 0);
    assert.equal(o.calls.checkIgnored.length, 0);
  });

  test("a failed interactive generate is not retried with an empty password", async () => {
    const dir = fakeApp();
    // First (inherit) generate fails - e.g. a mistyped confirmation; a retry
    // would succeed. The old code silently fell back to -p "".
    const o = ops({ genCodes: [1, 0] });
    await assert.rejects(
      keysInit({
        appDir: dir,
        keyPath: join(tmp(), "k"),
        prompt: {}, // marks the run interactive; askText isn't reached (keyPath given)
        ops: o,
      }),
      /signer generate failed - no key was written/,
    );
    assert.equal(o.calls.generate.length, 1);
    assert.equal(o.calls.generate[0].mode, "inherit");
    assert.ok(!o.calls.generate.some((c) => c.args.includes("-p")));
  });

  test("non-interactive without a password source fails naming the flags", async () => {
    const dir = fakeApp();
    const o = ops();
    await assert.rejects(
      keysInit({ appDir: dir, keyPath: join(tmp(), "k"), ops: o }),
      /non-interactive.*--password.*--no-password/s,
    );
    assert.equal(o.calls.generate.length, 0);
  });

  test("--no-password is the explicit opt-in to an unprotected key", async () => {
    const dir = fakeApp();
    const o = ops();
    await keysInit({ appDir: dir, keyPath: join(tmp(), "k"), noPassword: true, ops: o });
    assert.deepEqual(o.calls.generate[0].args.slice(-2), ["-p", ""]);
  });

  test("--password and --no-password conflict", async () => {
    const dir = fakeApp();
    const o = ops();
    await assert.rejects(
      keysInit({
        appDir: dir,
        keyPath: join(tmp(), "k"),
        password: "x",
        noPassword: true,
        ops: o,
      }),
      /--password or --no-password, not both/,
    );
    assert.equal(o.calls.generate.length, 0);
  });
});
