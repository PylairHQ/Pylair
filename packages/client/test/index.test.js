import { test, beforeEach } from "node:test";
import assert from "node:assert/strict";
import {
  python,
  call,
  stream,
  cancel,
  setBridge,
  PythonError,
  LimitExceededError,
  _resetForTests,
} from "../dist/index.js";

/**
 * Fake IPC bridge: records invokes, lets tests push events. Per-call
 * delivery — `invokeChannel` captures the callback bound at invoke time
 * and `emitTask` delivers to that call only (what `Channel::send` does).
 */
function fakeBridge() {
  const calls = [];
  const listeners = new Map();
  const channels = new Map(); // task -> onEvent
  let nextTask = 100;
  return {
    calls,
    channels,
    emit(event, payload) {
      for (const cb of listeners.get(event) ?? []) cb(payload);
    },
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
      assert.equal(cmd, "plugin:pylair|python_call_stream");
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

const streamDone = (task, result) => ({ task, done: true, result });
const streamErr = (task, code, message) => ({ task, done: true, error: { code, message } });
const streamChunk = (task, seq, value) => ({ task, seq, value, done: false });
const tick = () => new Promise((r) => setTimeout(r, 0));

beforeEach(() => _resetForTests());

test("call resolves with the terminal result", async () => {
  const b = fakeBridge();
  setBridge(b);
  const p = call("add", { a: 1, b: 2 });
  await tick(); // let invokeChannel resolve, task=100
  b.emitTask(100, streamDone(100, 3));
  assert.equal(await p, 3);
});

test("call() declares stream:false, stream() declares stream:true", async () => {
  const b = fakeBridge();
  setBridge(b);
  const p = call("add", { a: 1 });
  await tick();
  assert.equal(
    b.calls.find((c) => c.cmd === "plugin:pylair|python_call_stream").args.stream,
    false,
    "call() must declare stream:false so the host takes no stream slot",
  );
  b.emitTask(100, streamDone(100, 2));
  await p;

  // stream() is an async generator — nothing runs until first next().
  const s = stream("counter", { n: 1 })[Symbol.asyncIterator]();
  const first = s.next();
  await tick();
  const invoke = b.calls.filter((c) => c.cmd === "plugin:pylair|python_call_stream").at(-1);
  assert.equal(invoke.args.stream, true, "stream() must declare stream:true");
  b.emitTask(101, streamDone(101, null));
  await first;
});

test("old plugin's bare task ack errors loudly and cancels the orphan", async () => {
  const b = fakeBridge();
  // An old python_call_stream ignores `onEvent` and returns a bare task
  // id — the task runs with no channel to report on.
  b.invokeChannel = async (cmd, args) => {
    b.calls.push({ cmd, args });
    return 7;
  };
  setBridge(b);
  await assert.rejects(
    call("add", { a: 1, b: 2 }),
    /older than the client.*tauri-plugin-pylair/,
  );
  await tick();
  const cancels = b.calls.filter((c) => c.cmd === "plugin:pylair|python_cancel");
  assert.deepEqual(
    cancels.map((c) => c.args),
    [{ task: 7 }],
    "the orphaned task must be cancelled",
  );
});

test("stream also rejects an old plugin's bare task ack", async () => {
  const b = fakeBridge();
  b.invokeChannel = async (cmd, args) => {
    b.calls.push({ cmd, args });
    return 9;
  };
  setBridge(b);
  await assert.rejects(
    (async () => {
      for await (const _ of stream("counter", { n: 1 })) void _;
    })(),
    /older than the client.*tauri-plugin-pylair/,
  );
});

test("call rejects with PythonError on runtime error", async () => {
  const b = fakeBridge();
  setBridge(b);
  const p = call("boom");
  await tick();
  b.emitTask(100, streamErr(100, -32000, "kaboom"));
  await assert.rejects(p, (e) => e instanceof PythonError && e.code === -32000);
});

test("abort signal sends python_cancel for the task", async () => {
  const b = fakeBridge();
  setBridge(b);
  const ac = new AbortController();
  const p = call("slow", {}, { signal: ac.signal });
  await tick();
  ac.abort();
  await tick();
  assert.ok(b.calls.some((c) => c.cmd === "plugin:pylair|python_cancel" && c.args.task === 100));
  // runtime then reports TASK_CANCELLED as the terminal
  b.emitTask(100, streamErr(100, -32010, "task cancelled"));
  await assert.rejects(p, (e) => e instanceof PythonError && e.code === -32010);
});

test("stream yields chunks then ends on terminal", async () => {
  const b = fakeBridge();
  setBridge(b);
  const it = stream("counter", { n: 3 });
  const collected = [];
  const consume = (async () => {
    for await (const v of it) collected.push(v);
  })();
  await tick();
  b.emitTask(100, streamChunk(100, 0, "a"));
  b.emitTask(100, streamChunk(100, 1, "b"));
  b.emitTask(100, streamChunk(100, 2, "c"));
  b.emitTask(100, streamDone(100, { done: true }));
  await consume;
  assert.deepEqual(collected, ["a", "b", "c"]);
});

test("stream throws PythonError on terminal error", async () => {
  const b = fakeBridge();
  setBridge(b);
  const it = stream("counter");
  const consume = (async () => {
    for await (const _ of it) { /* consume */ }
  })();
  await tick();
  b.emitTask(100, streamErr(100, -32031, "runtime crashed"));
  await assert.rejects(consume, (e) => e instanceof PythonError && e.code === -32031);
});

test("stream tops up pull credit as consumer drains", async () => {
  const b = fakeBridge();
  setBridge(b);
  const it = stream("infinite");
  const consume = (async () => {
    let i = 0;
    for await (const _ of it) if (++i >= 40) break;
  })();
  await tick();
  for (let s = 0; s < 40; s++) b.emitTask(100, streamChunk(100, s, s));
  await consume;
  const grants = b.calls.filter((c) => c.cmd === "plugin:pylair|python_grant_credit");
  assert.ok(grants.length >= 1 && grants.every((g) => g.args.task === 100));
});

test("each call only ever sees its own channel events", async () => {
  // The channel is bound at invoke time: an event for task 100 can
  // never reach task 101 — there is no shared bus to leak across.
  const b = fakeBridge();
  setBridge(b);
  const a = call("add");
  const c = call("add");
  await tick();
  b.emitTask(100, streamDone(100, "A"));
  b.emitTask(101, streamDone(101, "B"));
  assert.equal(await a, "A");
  assert.equal(await c, "B");
});

test("cancel() forwards task id and returns the outcome", async () => {
  const b = fakeBridge();
  setBridge(b);
  assert.equal(await cancel(77), "cancelled");
  assert.deepEqual(b.calls.at(-1), { cmd: "plugin:pylair|python_cancel", args: { task: 77 } });
});

test("state() and onState() expose lifecycle", async () => {
  const b = fakeBridge();
  setBridge(b);
  assert.equal(await python.state(), "ready");
  const seen = [];
  const off = await python.onState((s) => seen.push(s));
  b.emit("pylair://state", { from: "ready", to: "restarting", detail: "x" });
  assert.equal(seen[0].to, "restarting");
  off();
});

test("wire JSON: Uint8Array <-> $b64 inside call/stream", async () => {
  const b = fakeBridge();
  setBridge(b);
  // top-level + nested bytes encode on the way out
  const p = call("roundtrip", {
    data: new Uint8Array([0, 1, 2]),
    nested: { blob: new Uint8Array([255]) },
  });
  await tick();
  assert.deepEqual(b.calls[0].args.input.data, { $b64: "AAEC" });
  assert.deepEqual(b.calls[0].args.input.nested.blob, { $b64: "/w==" });
  // $b64 tags decode on the way back
  b.emitTask(100, streamDone(100, { $b64: "AgEA" }));
  const result = await p;
  assert.ok(result instanceof Uint8Array);
  assert.deepEqual([...result], [2, 1, 0]);
});

test("wire JSON: stream chunks decode per item", async () => {
  const b = fakeBridge();
  setBridge(b);
  const it = stream("gen");
  const collected = [];
  const consume = (async () => {
    for await (const v of it) collected.push(v);
  })();
  await tick();
  b.emitTask(100, streamChunk(100, 0, { $b64: "aGk=" }));
  b.emitTask(100, streamDone(100, { done: true }));
  await consume;
  assert.ok(collected[0] instanceof Uint8Array);
  assert.deepEqual([...collected[0]], [104, 105]); // "hi"
});

test("python namespace exposes the API surface", () => {
  for (const k of [
    "call",
    "stream",
    "cancel",
    "retry",
    "state",
    "shutdown",
    "onState",
    "onStderr",
    "onExposes",
  ]) {
    assert.equal(typeof python[k], "function", `python.${k}`);
  }
});

/**
 * Bridge whose channel fires events BEFORE the invoke resolves — what
 * the Rust side can do: the forwarder starts sending as soon as Python
 * replies, racing the invoke response that carries the task id. The
 * callback is bound at invoke time, so nothing is lost.
 */
function earlyBridge(eventsFor) {
  const b = fakeBridge();
  const inner = b.invokeChannel.bind(b);
  b.invokeChannel = async (cmd, args, onEvent) => {
    const task = await inner(cmd, args, onEvent);
    if (cmd === "plugin:pylair|python_call_stream") {
      for (const ev of eventsFor(task)) onEvent(ev);
    }
    return task;
  };
  return b;
}

test("call resolves when the terminal arrives before invoke resolves", async () => {
  const b = earlyBridge((t) => [streamDone(t, 42)]);
  setBridge(b);
  const r = await Promise.race([
    call("fast"),
    new Promise((_, rej) => setTimeout(() => rej(new Error("call stayed pending")), 500)),
  ]);
  assert.equal(r, 42);
});

test("call rejects when an early terminal is an error", async () => {
  const b = earlyBridge((t) => [streamErr(t, -32000, "early boom")]);
  setBridge(b);
  await assert.rejects(
    Promise.race([
      call("fast"),
      new Promise((_, rej) => setTimeout(() => rej(new Error("call stayed pending")), 500)),
    ]),
    (e) => e instanceof PythonError && e.code === -32000,
  );
});

test("stream keeps early chunks and the early terminal, in order", async () => {
  const b = earlyBridge((t) => [streamChunk(t, 0, "a"), streamChunk(t, 1, "b"), streamDone(t, null)]);
  setBridge(b);
  const got = [];
  const run = (async () => {
    for await (const v of stream("fast")) got.push(v);
  })();
  await Promise.race([
    run,
    new Promise((_, rej) => setTimeout(() => rej(new Error("stream stayed pending")), 500)),
  ]);
  assert.deepEqual(got, ["a", "b"]);
});

test("breaking out of a stream cancels the producer", async () => {
  const b = fakeBridge();
  setBridge(b);
  const it = stream("counter", { n: 1000 });
  const first = it.next();
  await tick();
  b.emitTask(100, streamChunk(100, 0, "a"));
  assert.equal((await first).value, "a");
  await it.return(undefined); // what `break` does
  assert.ok(
    b.calls.some((c) => c.cmd === "plugin:pylair|python_cancel" && c.args.task === 100),
    "abandoning the iterator must send python_cancel",
  );
});

test("a stream that finished normally does not send a redundant cancel", async () => {
  const b = fakeBridge();
  setBridge(b);
  const it = stream("counter", { n: 1 });
  const first = it.next();
  await tick();
  b.emitTask(100, streamChunk(100, 0, "a"));
  await first;
  const second = it.next();
  b.emitTask(100, streamDone(100, null));
  assert.equal((await second).done, true);
  assert.ok(!b.calls.some((c) => c.cmd === "plugin:pylair|python_cancel"));
});

test("call with timeoutMs rejects with a deadline error and cancels the task", async () => {
  const b = fakeBridge();
  setBridge(b);
  const p = call("slow", {}, { timeoutMs: 50 });
  await assert.rejects(p, (e) => e instanceof PythonError && e.code === -32013);
  assert.ok(
    b.calls.some((c) => c.cmd === "plugin:pylair|python_cancel" && c.args.task === 100),
    "an expired deadline must cancel the producer",
  );
});

test("a call that finishes before its deadline is unaffected", async () => {
  const b = fakeBridge();
  setBridge(b);
  const p = call("fast", {}, { timeoutMs: 500 });
  await tick();
  b.emitTask(100, streamDone(100, "ok"));
  assert.equal(await p, "ok");
  await new Promise((r) => setTimeout(r, 20));
  assert.ok(!b.calls.some((c) => c.cmd === "plugin:pylair|python_cancel"));
});

test("stream with timeoutMs throws the deadline error and cancels", async () => {
  const b = fakeBridge();
  setBridge(b);
  const it = stream("slow", {}, { timeoutMs: 50 });
  await assert.rejects(it.next(), (e) => e instanceof PythonError && e.code === -32013);
  assert.ok(b.calls.some((c) => c.cmd === "plugin:pylair|python_cancel"));
});

test("invoke rejection with LIMIT_EXCEEDED surfaces LimitExceededError", async () => {
  const b = fakeBridge();
  // Saturated host: python_call_stream rejects with the serialized ErrorObj.
  const realChannel = b.invokeChannel.bind(b);
  b.invokeChannel = async (cmd, args, onEvent) => {
    if (cmd === "plugin:pylair|python_call_stream")
      throw {
        code: -32040,
        message: "too many in-flight tasks",
        data: { limit: "max_concurrent_tasks", max: 64 },
      };
    return realChannel(cmd, args, onEvent);
  };
  setBridge(b);
  await assert.rejects(
    call("anything"),
    (e) =>
      e instanceof LimitExceededError &&
      e.limit === "max_concurrent_tasks" &&
      e.max === 64 &&
      e.code === -32040,
  );
});

test("stream terminal LIMIT_EXCEEDED throws LimitExceededError with .limit", async () => {
  const b = fakeBridge();
  setBridge(b);
  const it = stream("flood");
  const first = it.next(); // generators start on next(): binds the channel, task=100
  await tick();
  b.emitTask(100, {
    task: 100,
    done: true,
    error: {
      code: -32040,
      message: "stream buffer cap",
      data: { limit: "stream_buffer_bytes", max: 16777216 },
    },
  });
  await assert.rejects(
    first,
    (e) =>
      e instanceof LimitExceededError &&
      e.limit === "stream_buffer_bytes" &&
      e.code === -32040,
  );
});

// ---- rc.15 PR I: input typing, call/stream mismatch, abort hygiene ----

/** A rejected-in-microtask promise vs a still-pending marker — lets the
 *  red phase fail cleanly where unfixed code would leave the call
 *  hanging instead of rejecting synchronously. */
const settledNow = (p) =>
  Promise.race([p.catch((e) => e), tick().then(() => "still-pending")]);

test("toWire rejects a Date naming the path, before any invoke", async () => {
  const b = fakeBridge();
  setBridge(b);
  const p = call("f", { when: new Date(0) });
  const out = await settledNow(p);
  assert.ok(
    out instanceof TypeError && /input\.when is a Date/.test(out.message),
    `expected TypeError naming input.when, got ${out}`,
  );
  assert.equal(b.calls.length, 0, "rejected input must not reach the bridge");
});

test("toWire rejects Map, Set, RegExp, Promise, non-Uint8Array views", async () => {
  const b = fakeBridge();
  setBridge(b);
  const cases = [
    [{ m: new Map() }, /input\.m is a Map/],
    [{ s: new Set([1]) }, /input\.s is a Set/],
    [{ r: /x/ }, /input\.r is a RegExp/],
    [{ p: Promise.resolve(1) }, /input\.p is a Promise/],
    [{ f: new Float64Array(2) }, /input\.f is a Float64Array/],
    [{ ab: new ArrayBuffer(4) }, /input\.ab is an ArrayBuffer/],
  ];
  for (const [input, re] of cases) {
    const p = call("f", input);
    const out = await settledNow(p);
    assert.ok(out instanceof TypeError && re.test(out.message), `${re} got ${out}`);
  }
  assert.equal(b.calls.length, 0);
});

test("toWire rejects function, symbol, bigint values by name", async () => {
  const b = fakeBridge();
  setBridge(b);
  const cases = [
    [{ fn: () => 1 }, /input\.fn is a function/],
    [{ sym: Symbol("s") }, /input\.sym is a symbol/],
    [{ big: 10n }, /input\.big is a bigint/],
  ];
  for (const [input, re] of cases) {
    const p = call("f", input);
    const out = await settledNow(p);
    assert.ok(out instanceof TypeError && re.test(out.message), `${re} got ${out}`);
  }
  assert.equal(b.calls.length, 0);
});

test("toWire names the nested path", async () => {
  const b = fakeBridge();
  setBridge(b);
  const p = call("f", { outer: { list: [1, { when: new Date(0) }] } });
  const out = await settledNow(p);
  assert.ok(/input\.outer\.list\[1\]\.when is a Date/.test(out?.message ?? ""), `got ${out}`);
});

test("toWire rejects circular input naming the path", async () => {
  const b = fakeBridge();
  setBridge(b);
  const cyc = { a: {} };
  cyc.a.back = cyc;
  const p = call("f", cyc);
  const out = await settledNow(p);
  assert.ok(out instanceof TypeError && /circular/.test(out.message), `got ${out}`);
});

test("class instances serialize own enumerable props; empty ones throw", async () => {
  const b = fakeBridge();
  setBridge(b);
  class Point {
    constructor(x, y) {
      this.x = x;
      this.y = y;
    }
    dist() {
      return Math.hypot(this.x, this.y);
    }
  }
  const p = call("f", { pt: new Point(3, 4) });
  await tick();
  assert.deepEqual(b.calls.at(-1).args.input, { pt: { x: 3, y: 4 } });
  b.emitTask(100, streamDone(100, "ok"));
  await p;

  class Empty {}
  const q = call("f", { e: new Empty() });
  const out = await settledNow(q);
  assert.ok(out instanceof TypeError && /input\.e.*Empty/.test(out.message), `got ${out}`);
});

test("plain objects, null-proto objects, arrays, Uint8Array still pass", async () => {
  const b = fakeBridge();
  setBridge(b);
  const nullo = Object.create(null);
  nullo.k = "v";
  const p = call("f", { o: { a: 1 }, arr: [1, "x"], nullo, bytes: new Uint8Array([1, 2]) });
  await tick();
  const input = b.calls.at(-1).args.input;
  assert.deepEqual(input.o, { a: 1 });
  assert.deepEqual(input.arr, [1, "x"]);
  assert.deepEqual(input.nullo, { k: "v" });
  assert.ok(input.bytes && typeof input.bytes === "object" && "$b64" in input.bytes);
  b.emitTask(100, streamDone(100, null));
  await p;
});

test("call() on a stream function fails 'use stream()' and cancels the task", async () => {
  const b = fakeBridge();
  setBridge(b);
  const p = call("counter", { n: 3 });
  await tick();
  // A chunk event means the function is a generator — call() must say so.
  b.emitTask(100, streamChunk(100, 0, "v0"));
  const outcome = await Promise.race([
    p.then(
      () => "resolved",
      (e) => e,
    ),
    new Promise((r) => setTimeout(() => r("still-pending"), 200)),
  ]);
  assert.ok(
    outcome instanceof Error && /stream\(\)/.test(outcome.message) && /counter/.test(outcome.message),
    `expected a 'use stream()' rejection, got ${outcome}`,
  );
  await tick();
  assert.ok(
    b.calls.some((c) => c.cmd === "plugin:pylair|python_cancel" && c.args.task === 100),
    "the mismatched task must be cancelled, not left running",
  );
});

test("a long-lived AbortSignal keeps zero listeners after 1000 settled calls", async () => {
  const { getEventListeners } = await import("node:events");
  const b = fakeBridge();
  setBridge(b);
  const ac = new AbortController();
  let task = 100;
  for (let i = 0; i < 1000; i++) {
    const p = call("f", {}, { signal: ac.signal });
    await tick();
    b.emitTask(task, streamDone(task, i));
    await p;
    task++;
  }
  assert.equal(
    getEventListeners(ac.signal, "abort").length,
    0,
    "abort listeners must be removed when each call settles",
  );
});

test("a long-lived AbortSignal keeps zero listeners after settled streams", async () => {
  const { getEventListeners } = await import("node:events");
  const b = fakeBridge();
  setBridge(b);
  const ac = new AbortController();
  let task = 100;
  for (let i = 0; i < 200; i++) {
    const it = stream("g", {}, { signal: ac.signal });
    const next = it.next();
    await tick();
    b.emitTask(task, streamDone(task, null));
    await next;
    task++;
  }
  assert.equal(getEventListeners(ac.signal, "abort").length, 0);
});
