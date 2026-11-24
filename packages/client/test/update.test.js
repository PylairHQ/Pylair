import { test, afterEach } from "node:test";
import assert from "node:assert/strict";
import { update, setBridge, UpdateError, UPDATE_FAILED } from "../dist/index.js";

/**
 * update.* (decision 0011): command wiring, download event forwarding
 * in upstream's shapes, and UpdateError code mapping.
 */

function fakeBridge(impl = {}) {
  const calls = [];
  const b = {
    calls,
    emit: null,
    async invoke(cmd, args) {
      calls.push({ cmd, args });
      return impl.invoke?.(cmd, args);
    },
    async invokeChannel(cmd, args, onEvent) {
      calls.push({ cmd, args });
      b.emit = onEvent;
      return impl.invokeChannel?.(cmd, args, onEvent);
    },
    async listen() {
      return () => {};
    },
  };
  return b;
}

afterEach(() => setBridge(null));

test("check() maps the offer and returns null for none", async () => {
  const b = fakeBridge({
    invoke: () => ({ version: "1.2.0", notes: "fixes", sha256: "ab", current: "1.1.0" }),
  });
  setBridge(b);
  assert.deepEqual(await update.check(), {
    version: "1.2.0",
    notes: "fixes",
    sha256: "ab",
    current: "1.1.0",
  });
  assert.equal(b.calls[0].cmd, "plugin:pylair|python_update_check");
});

test("check() resolves null on a skipped or absent update", async () => {
  const b = fakeBridge({ invoke: () => null });
  setBridge(b);
  assert.equal(await update.check(), null);
});

test("downloadAndInstall forwards Started/Progress/Finished then installs", async () => {
  const b = fakeBridge();
  setBridge(b);
  const seen = [];
  const p = update.downloadAndInstall({ onEvent: (e) => seen.push(e) });
  // The channel registers on a later microtask (after ipc()); wait for it.
  for (let i = 0; i < 50 && !b.emit; i++) await new Promise((r) => setImmediate(r));
  assert.ok(b.emit, "download channel never registered");
  b.emit({ event: "Started", data: { contentLength: 100 } });
  b.emit({ event: "Progress", data: { chunkLength: 60 } });
  b.emit({ event: "Finished", data: {} });
  await p;
  assert.deepEqual(seen, [
    { event: "Started", data: { contentLength: 100 } },
    { event: "Progress", data: { chunkLength: 60 } },
    { event: "Finished", data: {} },
  ]);
  assert.deepEqual(
    b.calls.map((c) => c.cmd),
    ["plugin:pylair|python_update_download", "plugin:pylair|python_update_install"],
  );
});

test("errors carry UpdateError with the stable updateCode", async () => {
  const b = fakeBridge({
    invoke: () => {
      throw {
        code: UPDATE_FAILED,
        message: "app is running under macOS App Translocation",
        data: { code: "NotUpdatable" },
      };
    },
  });
  setBridge(b);
  await assert.rejects(update.check(), (e) => {
    assert.ok(e instanceof UpdateError);
    assert.equal(e.updateCode, "NotUpdatable");
    assert.equal(e.code, UPDATE_FAILED);
    return true;
  });
});

test("NeedsApproval surfaces on install", async () => {
  const b = fakeBridge({
    invokeChannel: () => {},
    invoke: (cmd) => {
      if (cmd.endsWith("install"))
        throw { code: UPDATE_FAILED, message: "per-machine", data: { code: "NeedsApproval" } };
      return {};
    },
  });
  setBridge(b);
  await assert.rejects(update.downloadAndInstall(), (e) => {
    assert.ok(e instanceof UpdateError);
    assert.equal(e.updateCode, "NeedsApproval");
    return true;
  });
});

test("relaunch invokes python_update_relaunch", async () => {
  const b = fakeBridge();
  setBridge(b);
  await update.relaunch();
  assert.equal(b.calls[0].cmd, "plugin:pylair|python_update_relaunch");
});
