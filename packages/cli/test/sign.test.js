import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { runHdiutil } from "../dist/build/sign.js";

/** A stubbed hdiutil: queued results per call, every call recorded. */
function ops(results) {
  const calls = [];
  const warns = [];
  const sleeps = [];
  const queue = [...results];
  return {
    calls,
    warns,
    sleeps,
    run: (args) => {
      calls.push(args);
      return queue.length > 1 ? queue.shift() : queue[0];
    },
    warn: (s) => warns.push(s),
    sleep: (ms) => sleeps.push(ms),
  };
}
const fail = (out, code = 1) => ({ code, out });
const ok = () => ({ code: 0, out: "" });

describe("hdiutil retries only the transient lock errors", () => {
  const create = ["create", "-volname", "App", "-srcfolder", "/s", "-ov", "-format", "UDZO", "/a.dmg"];

  test("transient then success: one warn, one second of backoff", () => {
    const o = ops([fail("hdiutil: create failed - Resource busy"), ok()]);
    runHdiutil(o, create);
    assert.equal(o.calls.length, 2);
    assert.deepEqual(o.warns.length, 1);
    assert.match(o.warns[0], /retrying in 1s \(attempt 2\/5\)/);
    assert.deepEqual(o.sleeps, [1000]);
  });

  test("transient every time: five attempts, backoff 1, 2, 4, 8 s, then the last error", () => {
    const o = ops([fail("hdiutil: create failed - Resource busy")]);
    assert.throws(() => runHdiutil(o, create), /hdiutil create exited 1[\s\S]*Resource busy/);
    assert.equal(o.calls.length, 5);
    assert.equal(o.warns.length, 4);
    assert.deepEqual(o.sleeps, [1000, 2000, 4000, 8000]);
  });

  test("a non-transient failure throws at once: no retry, no warn", () => {
    const o = ops([fail("hdiutil: create failed - error -5341")]);
    assert.throws(() => runHdiutil(o, create), /exited 1/);
    assert.equal(o.calls.length, 1);
    assert.deepEqual(o.warns, []);
    assert.deepEqual(o.sleeps, []);
  });

  test("attach: a stale mount of the same image is detached before retrying", () => {
    const image = "/builds/app.dmg";
    const attach = ["attach", "-nobrowse", "-readonly", "-mountpoint", "/tmp/mnt", image];
    const info = {
      code: 0,
      out: [
        "================================================",
        "image-path      : /other/other.dmg",
        "mount-point\t: /Volumes/Other",
        "================================================",
        `image-path      : ${image}`,
        "mount-point\t: /Volumes/App",
        "mount-point\t: /tmp/mnt",
      ].join("\n"),
    };
    const o = ops([fail("hdiutil: attach failed - Resource temporarily unavailable"), info, ok(), ok(), ok()]);
    runHdiutil(o, attach);
    const verbs = o.calls.map((c) => c[0]);
    assert.deepEqual(verbs, ["attach", "info", "detach", "detach", "attach"]);
    const mounts = o.calls.filter((c) => c[0] === "detach").map((c) => c[1]);
    assert.deepEqual(mounts, ["/tmp/mnt", "/Volumes/App"]);
    assert.deepEqual(o.sleeps, [1000]);
  });

  test("attach: non-transient failure detaches nothing", () => {
    const attach = ["attach", "-mountpoint", "/tmp/mnt", "/a.dmg"];
    const o = ops([fail("hdiutil: attach failed - error 49153")]);
    assert.throws(() => runHdiutil(o, attach));
    assert.deepEqual(o.calls.map((c) => c[0]), ["attach"]);
  });
});
