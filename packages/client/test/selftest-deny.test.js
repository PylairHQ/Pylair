// The self-test's per-function-permission verdict. The real probe runs in an
// installed webview (win-b); this pins the rule the owner applies to win-b's
// report: both call commands must come back PERMISSION_DENIED naming the
// function and win-b, and a missing report must fail rather than pass.
import { test } from "node:test";
import assert from "node:assert/strict";
import { judgeDenyProbe, judgeFnPermission } from "../dist/selftest.js";

const denied = (over = {}) => ({
  denied: true,
  code: -32050,
  fn: "add",
  window: "win-b",
  ...over,
});
const ok = () => ({ viaCall: denied(), viaRaw: denied() });

test("both commands denied for the right function and window passes", () => {
  assert.equal(judgeDenyProbe(ok(), "add"), null);
});

test("a missing report fails (an old client or a lost report must not pass)", () => {
  assert.match(judgeDenyProbe(undefined, "add"), /no deny probe/);
  assert.match(judgeDenyProbe({}, "add"), /no deny probe/);
});

test("a call that succeeded fails, naming which command let it through", () => {
  const r = judgeDenyProbe({ viaCall: { denied: false }, viaRaw: denied() }, "add");
  assert.match(r, /python_call_stream|call\(\)/);
  assert.match(r, /not denied/);
  const raw = judgeDenyProbe({ viaCall: denied(), viaRaw: { denied: false } }, "add");
  assert.match(raw, /python_call\b/);
});

test("a different failure is not a denial", () => {
  const r = judgeDenyProbe(
    { viaCall: { denied: false, error: "boom", code: -32601 }, viaRaw: denied() },
    "add",
  );
  assert.match(r, /not denied/);
  assert.match(r, /-32601/);
});

test("a denial naming the wrong function or window fails", () => {
  assert.match(judgeDenyProbe({ viaCall: denied({ fn: "other" }), viaRaw: denied() }, "add"), /other/);
  assert.match(
    judgeDenyProbe({ viaCall: denied(), viaRaw: denied({ window: "main" }) }, "add"),
    /main/,
  );
});

test("a denied flag with a different code fails", () => {
  assert.match(judgeDenyProbe({ viaCall: denied({ code: -32040 }), viaRaw: denied() }, "add"), /-32040/);
});

// ---- the check's overall verdict, including "the plugin predates this" ----

test("unsupported plugin: skipped with the reason, never a failure", () => {
  // Whatever win-b reported (here: it got through, as it must on an old crate).
  const through = { viaCall: { denied: false }, viaRaw: { denied: false } };
  const v = judgeFnPermission(false, through, "add", true);
  assert.equal(v.ok, true);
  assert.equal(v.check, "skipped: resolved plugin predates per-function permissions");
  assert.equal(judgeFnPermission(false, undefined, "add", false).ok, true);
});

test("supported and denied: ok", () => {
  const v = judgeFnPermission(true, ok(), "add", true);
  assert.deepEqual(v, { ok: true, check: "ok" });
});

test("supported and not denied: FAIL", () => {
  const through = { viaCall: { denied: false }, viaRaw: denied() };
  const v = judgeFnPermission(true, through, "add", true);
  assert.equal(v.ok, false);
  assert.match(v.check, /^FAIL: .*not denied/);
});

test("supported but no report from win-b: FAIL (a stale capability file must not pass)", () => {
  const v = judgeFnPermission(true, undefined, "add", true);
  assert.equal(v.ok, false);
  assert.match(v.check, /^FAIL: .*no deny probe/);
});

test("supported, denied, but main could not call the function: FAIL", () => {
  const v = judgeFnPermission(true, ok(), "add", false);
  assert.equal(v.ok, false);
  assert.match(v.check, /^FAIL: .*main could not call add/);
});
