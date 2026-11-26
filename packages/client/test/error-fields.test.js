import assert from "node:assert/strict";
import { beforeEach, test } from "node:test";

import {
  ErrorCode,
  isCancelled,
  PythonError,
  _resetForTests,
  call,
  setBridge,
} from "../dist/index.js";

function fakeBridge() {
  const calls = [];
  const listeners = new Map();
  const channels = new Map();
  let nextTask = 100;
  return {
    calls,
    channels,
    emitTask(task, ev) {
      const cb = channels.get(task);
      assert.ok(cb, `no channel registered for task ${task}`);
      cb(ev);
    },
    async invoke(cmd, args) {
      calls.push({ cmd, args });
      if (cmd === "plugin:pylair|python_cancel") return "cancelled";
      if (cmd === "plugin:pylair|python_state") return "ready";
      return null;
    },
    async invokeChannel(cmd, args, onEvent) {
      calls.push({ cmd, args });
      const task = nextTask++;
      channels.set(task, onEvent);
      return { task, protocol: 2 };
    },
    async listen(event, cb) {
      if (!listeners.has(event)) listeners.set(event, []);
      listeners.get(event).push(cb);
      return () => listeners.get(event).splice(listeners.get(event).indexOf(cb), 1);
    },
  };
}

const streamErr = (task, code, message, data) => ({ task, done: true, error: { code, message, data } });
const tick = () => new Promise((r) => setTimeout(r, 0));

beforeEach(() => _resetForTests());

test("PythonError exposes typed traceback when present", () => {
  const e = new PythonError(-32000, "boom", {
    type: "ValueError",
    traceback: "Traceback...\nValueError: boom\n",
  });
  assert.match(e.traceback, /ValueError: boom/);
});

test("traceback is undefined in the release shape", () => {
  const e = new PythonError(-32000, "boom", { type: "ValueError" });
  assert.equal(e.traceback, undefined);
});

test("suggestions parse from FUNCTION_NOT_FOUND data", () => {
  const e = new PythonError(-32601, "no such function 'ad' - did you mean 'add'?", {
    suggestions: ["add"],
  });
  assert.deepEqual(e.suggestions, ["add"]);
  assert.deepEqual(new PythonError(-32601, "no such function").suggestions, []);
});

test("details carry the structured INVALID_PARAMS list", () => {
  const errors = [{ path: "args.items[1].name", expected: "str", received: "number" }];
  const e = new PythonError(-32602, "param 'items': ...", { errors });
  assert.deepEqual(e.details, errors);
  assert.deepEqual(new PythonError(-32602, "x").details, []);
});

test("isCancelled matches the cancellation code only", () => {
  assert.ok(isCancelled(new PythonError(ErrorCode.TASK_CANCELLED, "cancelled")));
  assert.ok(!isCancelled(new PythonError(-32000, "boom")));
  assert.ok(!isCancelled(new Error("cancelled")));
  assert.ok(!isCancelled(null));
});

test("ErrorCode names the protocol constants", () => {
  assert.equal(ErrorCode.TASK_CANCELLED, -32010);
  assert.equal(ErrorCode.INVALID_PARAMS, -32602);
  assert.equal(ErrorCode.METHOD_NOT_FOUND, -32601);
  assert.equal(ErrorCode.LIMIT_EXCEEDED, -32040);
  assert.equal(ErrorCode.PERMISSION_DENIED, -32050);
  assert.equal(ErrorCode.UPDATE_FAILED, -32060);
  assert.equal(ErrorCode.WORKER_DIED, -32011);
  assert.equal(ErrorCode.RUNTIME_WEDGED, -32012);
});

test("a rejecting error with a traceback is console.errored once", async () => {
  const b = fakeBridge();
  setBridge(b);
  const errors = [];
  const orig = console.error;
  console.error = (m) => errors.push(m);
  try {
    const p = call("boom", {});
    await tick();
    b.emitTask(
      100,
      streamErr(100, -32000, "boom", {
        type: "ValueError",
        traceback: "Traceback (most recent call last):\nValueError: boom\n",
      }),
    );
    await assert.rejects(p);
    assert.equal(errors.length, 1);
    assert.match(errors[0], /ValueError: boom/);
  } finally {
    console.error = orig;
  }
});

test("no console noise without a traceback (release shape)", async () => {
  const b = fakeBridge();
  setBridge(b);
  const errors = [];
  const orig = console.error;
  console.error = (m) => errors.push(m);
  try {
    const p = call("boom", {});
    await tick();
    b.emitTask(100, streamErr(100, -32000, "boom", { type: "ValueError" }));
    await assert.rejects(p);
    assert.equal(errors.length, 0);
  } finally {
    console.error = orig;
  }
});
