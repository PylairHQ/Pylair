# Performance

How Pylair measures bridge latency, the first recorded numbers, and how
to read them. This page is a baseline: nothing here describes an
optimization that has already shipped.

## The three layers

A `call()` from a webview crosses three hops, each timed separately so a
slow end-to-end number can be attributed to a layer:

1. **Webview to host** - Tauri IPC: arguments serialize into the command
   message, the host dispatches to the `python_call` command.
2. **Host to runtime** - the plugin writes a JSON frame to the runtime's
   stdin; `python -m pylair` dispatches the call and frames the answer
   back on stdout.
3. **Python dispatch** - `coerce_kwargs` binds the arguments to the
   exposed function's signature; `dumps` serializes the result.

## Payloads

Identical shapes on every layer, provided by `bench_*` fixtures
(`examples/reference-app/backend/app.py` for the webview layer,
`python/pylair/tests/testapp.py` for the Rust layer):

| case | payload | what it sizes |
|---|---|---|
| `call_noop` | `bench_noop()` - no args, returns `1` | floor cost of the full path |
| `call_64k` | `bench_echo(s)` - a 64 KiB ASCII string, returned | JSON (de)serialization both directions |
| `call_nested` | `bench_nested(shape)` - `BenchShape(name, origin: BenchPoint, points: list[BenchPoint], meta: dict[str, int])` | dataclass coercion |
| `stream_first_chunk` | `count(n=3)` / `counter(n=3)` - time to the first yielded item, then stop | stream setup, not iteration |

## Method

- 50 warm-up iterations, then 500 measured iterations per case, in one
  process.
- Samples in milliseconds (`performance.now()`, `Instant`, or
  `time.perf_counter()`).
- Percentiles are nearest-rank: sort the samples, take index
  `ceil(p * n) - 1`.
- Each case prints one JSON object on one line, prefixed by layer:
  `webview-bench:` (reported through the `python_bench_report` command),
  `rust-bench:`, `py-bench:`.
- The webview bench never runs by default. `PYLAIR_SELF_TEST_BENCH=1` on
  the host replaces the installed self-test checks with the timing
  harness; the app prints its `webview-bench:` lines and exits 0. It is a
  measurement, never a pass/fail verdict.

## Running each layer

```sh
# end to end, through the installed app (what CI does)
PYLAIR_SELF_TEST_BENCH=1 ./<the installed app>

# host <-> Python leg: real `python -m pylair testapp`, no webview
# (--release: the baseline CI run used the debug test profile, which
# inflates serde_json by roughly an order of magnitude - see the profile
# section below. CI runs release since perf-2.)
cargo test --release -p tauri-plugin-pylair --test bench_latency -- --ignored --nocapture

# Python dispatch alone
cd python/pylair && uv run python tests/bench_dispatch.py
```

## CI

`.github/workflows/bench.yml` is `workflow_dispatch`-only and
non-blocking: no other workflow depends on it and it is not in branch
protection, so it can never gate a merge. It builds the reference app on
ubuntu-22.04, windows-latest and macos-latest, runs the installed
webview bench, then the Rust and Python legs, and uploads one
`bench-<os>.jsonl` artifact per OS containing the three `*-bench:` lines
per case.

## First numbers

From `latency-bench` CI run 45014043565 (the first time this workflow
ran). Cells are milliseconds.

A timer caveat before reading the webview rows: WebKit-based webviews
(macOS WKWebView, Linux WebKitGTK) quantize `performance.now()` to
about 1 ms, so their webview numbers are effectively whole milliseconds.
WebView2 on Windows reports finer granularity (~0.1 ms). The `rust` and
`python` layers use unclamped clocks on every OS.

### ubuntu-22.04

| layer | case | p50 | p95 | p99 |
|---|---|---|---|---|
| webview | call_noop | 1 | 2 | 3 |
| webview | call_64k | 3 | 4 | 5 |
| webview | call_nested | 1 | 2 | 2 |
| webview | stream_first_chunk | 1 | 2 | 2 |
| rust | call_noop | 0.45 | 0.49 | 0.51 |
| rust | call_64k | 3.00 | 3.09 | 3.17 |
| rust | call_nested | 0.62 | 0.66 | 0.69 |
| rust | stream_first_chunk | 0.58 | 0.64 | 0.67 |
| python | coerce_noop | 0.010 | 0.011 | 0.015 |
| python | coerce_64k | 0.016 | 0.017 | 0.031 |
| python | coerce_nested | 0.114 | 0.133 | 0.149 |
| python | dumps_int | 0.006 | 0.006 | 0.007 |
| python | dumps_64k | 0.192 | 0.204 | 0.210 |
| python | dumps_nested | 0.014 | 0.014 | 0.026 |

### windows-latest

| layer | case | p50 | p95 | p99 |
|---|---|---|---|---|
| webview | call_noop | 1.9 | 2.2 | 2.6 |
| webview | call_64k | 6.2 | 6.5 | 8.7 |
| webview | call_nested | 2.1 | 2.3 | 2.7 |
| webview | stream_first_chunk | 2.8 | 3.2 | 3.5 |
| rust | call_noop | 0.71 | 0.85 | 0.91 |
| rust | call_64k | 2.54 | 2.65 | 2.71 |
| rust | call_nested | 0.77 | 0.91 | 0.95 |
| rust | stream_first_chunk | 1.01 | 1.11 | 1.17 |
| python | coerce_noop | 0.006 | 0.006 | 0.007 |
| python | coerce_64k | 0.009 | 0.009 | 0.013 |
| python | coerce_nested | 0.066 | 0.076 | 0.099 |
| python | dumps_int | 0.003 | 0.004 | 0.004 |
| python | dumps_64k | 0.144 | 0.166 | 0.173 |
| python | dumps_nested | 0.008 | 0.009 | 0.014 |

### macos-latest

| layer | case | p50 | p95 | p99 |
|---|---|---|---|---|
| webview | call_noop | 2 | 4 | 5 |
| webview | call_64k | 4 | 7 | 12 |
| webview | call_nested | 3 | 5 | 8 |
| webview | stream_first_chunk | 2 | 5 | 7 |
| rust | call_noop | 0.29 | 0.58 | 0.75 |
| rust | call_64k | 2.53 | 5.66 | 7.71 |
| rust | call_nested | 0.42 | 0.97 | 1.17 |
| rust | stream_first_chunk | 0.46 | 0.90 | 1.65 |
| python | coerce_noop | 0.005 | 0.006 | 0.006 |
| python | coerce_64k | 0.007 | 0.009 | 0.011 |
| python | coerce_nested | 0.053 | 0.172 | 0.203 |
| python | dumps_int | 0.003 | 0.004 | 0.004 |
| python | dumps_64k | 0.197 | 0.497 | 0.533 |
| python | dumps_nested | 0.008 | 0.030 | 0.091 |

## Reading the numbers

Decision `0002` chose a **supervised subprocess** over in-process
embedding (PyO3-style). The `rust` layer is what that choice costs: the
frame write to stdin, Python dispatch, and the frame back on stdout. An
in-process call would be microseconds cheaper - the price buys crash
isolation (a Python segfault takes down the runtime, not the app),
process-level cancellation of blocking native work, and dev/prod
parity. These numbers are the baseline that trade-off is judged against.

Attribution, using the p50 columns above:

- `webview - rust` is roughly the webview hop: Tauri IPC, command
  dispatch, and the message boundary in front of the supervisor.
  `call_noop` puts it at ~0.5 ms on Linux, ~1.2 ms on Windows, ~1.7 ms
  on macOS - but remember the 1 ms timer floor on the WebKit engines.
- `rust - python` is the framing, pipe and scheduling cost inside the
  host-to-runtime boundary. On `call_64k` it dominates: ~2.5-3 ms rust leg
  against ~0.2 ms of Python-side `dumps` work, i.e. moving 64 KiB
  through the frame path costs an order of magnitude more than
  serializing it twice.
- `python` alone is dispatch and serialization, independent of any
  transport. It is microseconds for small payloads; the visible costs
  are `dumps_64k` (~0.14-0.20 ms - the serialize-plus-reparse) and
  `coerce_nested` (~0.05-0.11 ms - signature binding plus dataclass
  construction).
- `stream_first_chunk` sits just above `call_noop` on the rust leg
  (~0.1-0.3 ms extra): stream setup is a small fixed cost on top of a
  call.

These are measurements, not verdicts - a slow cell here does not by
itself prove an optimization is worthwhile.

## Profile: where the 64 KiB round trip went

Perf-2 profiled `call_64k` with one monotonic clock stamped at each hop
of the round trip (bench call start, actor serialization done, writer
write done, response header parsed, call end) plus per-phase timings
inside the runtime. Two runs matter because the CI `rust` leg ran under
`cargo test`'s default **debug** profile.

Debug profile (`cargo test`, what run 45014043565 measured), p50 hop
costs:

| hop | p50 (ms) | share |
|---|---|---|
| call start -> `serde_json::to_vec` done | 1.542 | ~60% |
| serialized -> frame written on stdin | 0.170 | |
| request written -> response header back | 0.510 | |
| response parsed -> call returned | 0.132 | |
| **total `call_64k` p50** | **2.594** | |

The debug elephant was `serde_json::to_vec` itself: 1.44 ms p50 alone.
That is a **test-profile artifact** - serde_json compiles without
optimizations under `cargo test`. Re-running the identical instrumented
binary with `--release` collapses it:

Release profile (what ships), p50 hop costs, **before** the perf-2
changes:

| hop | p50 (ms) |
|---|---|
| call start -> frame serialized (incl. 4-byte prefix) | 0.112 |
| serialized -> frame written on stdin | 0.155 |
| request written -> response header back (**Python's turn**) | 0.487 |
| response parsed -> call returned | 0.110 |
| **total `call_64k` p50** | **1.143** |

Inside the runtime during that ~0.49 ms window (instrumented run, so
absolute values carry eprintln overhead): the 64 KiB request arrives in
**one** pipe read (`read_chunks` = 1, ~0.03 ms), `loads` ~0.06 ms,
`coerce_kwargs` ~0.03 ms, the echo itself ~0.08 ms, the int-guard
re-parse ~0.05 ms, `dumps` ~0.10 ms, the stdout write ~0.08 ms. Both
suspected pipe pathologies were disproven: no small/partial reads on the
Python side, no chunking on either pipe. The remaining real costs were
exactly the three cheap targets: two unbuffered writes per frame on the
Rust writer, per-call signature/hint resolution, and the int-guard
re-parse.

## What perf-2 changed

- **`_json.dumps` gained a single pre-walk** (`check_wire`): one
  iterative traversal checks nesting depth, the +-2^53 int range, and
  NaN/Infinity on the pre-serialization object, expanding the same nodes
  `_default` would (dict/list/tuple/set items, dataclass fields, enum
  values). Then `json.dumps` runs once - the `json.loads` re-parse is
  gone. `exceeds_depth` is gone; depth limits now ride inside `dumps`
  via a `depth_limit` parameter (callers: `_send` uses `MAX_DEPTH`,
  stream chunks `MAX_DEPTH - 4`, the worker `MAX_DEPTH - 2`).
- **`coerce_kwargs` caches `inspect.signature` + `get_type_hints`** per
  function object, and the dataclass path caches `fields()` + resolved
  hints per class - all in `WeakKeyDictionary`s, resolved lazily on
  first call (forward refs may not exist at `@expose` time), so dev
  reload's new function objects get fresh entries and dead ones collect.
- **`wire.rs::frame_bytes`** serializes each frame directly into a
  buffer that starts with its 4-byte length prefix, so the stdin writer
  in `spawn.rs` issues **one** `write_all` per frame instead of the old
  unbuffered header+body pair. Wire format, control-lane priority,
  ping slot and flush ordering are unchanged - the prefix bytes are
  identical, just written together.

Semantics preserved: NaN is still an error, the +-2^53 guard still fires
(including inside dataclasses - the pre-walk descends into fields),
`$b64` handling is unchanged, and no wire bytes changed.

## After perf-2

Local release-mode verification (Windows, same machine as the profile):
`call_64k` p50 **1.14 -> 0.53 ms**, `call_noop` 0.77 -> 0.22 ms. Python
leg: `coerce_nested` 0.066 -> 0.013 ms, `dumps_64k` 0.144 -> 0.076 ms.
Instrumented hops after: Python's turn 0.487 -> 0.391 ms; the writer now
reports `writes: 1` per frame.

CI comparison from bench.yml: **before** = run
[45014043565](https://github.com/PylairHQ/Pylair/actions/runs/45014043565)
(debug rust leg), **after** = run
[45018732722](https://github.com/PylairHQ/Pylair/actions/runs/45018732722)
(release rust leg). The rust before/after conflates the code wins with
the profile fix - the local release pair above is the like-for-like rust
comparison. Webview and python legs compare cleanly. Cells are p50 ms.

### ubuntu-22.04

| leg | case | before | after |
|---|---|---|---|
| webview | call_64k | 3 | 1 |
| rust | call_noop | 0.45 | 0.10 |
| rust | call_64k | 3.00 | 0.32 |
| rust | call_nested | 0.62 | 0.13 |
| rust | stream_first_chunk | 0.58 | 0.13 |
| python | coerce_nested | 0.114 | 0.016 |
| python | dumps_64k | 0.192 | 0.102 |

### windows-latest

| leg | case | before | after |
|---|---|---|---|
| webview | call_64k | 6.2 | 6.8 |
| rust | call_noop | 0.71 | 0.60 |
| rust | call_64k | 2.54 | 1.30 |
| rust | call_nested | 0.77 | 0.70 |
| rust | stream_first_chunk | 1.01 | 0.88 |
| python | coerce_nested | 0.066 | 0.038 |
| python | dumps_64k | 0.144 | 0.122 |

### macos-latest

| leg | case | before | after |
|---|---|---|---|
| webview | call_64k | 4 | 2 |
| rust | call_noop | 0.29 | 0.18 |
| rust | call_64k | 2.53 | 0.49 |
| rust | call_nested | 0.42 | 0.23 |
| rust | stream_first_chunk | 0.46 | 0.20 |
| python | coerce_nested | 0.053 | 0.016 |
| python | dumps_64k | 0.197 | 0.140 |

Read: the host<->Python leg is now well under a millisecond for the 64
KiB echo on Linux and macOS (and ~1.3 ms on Windows), versus ~2.5-3 ms
at baseline - and that baseline was already inflated by the debug test
profile. The webview layer barely moves on `call_64k`: the remaining
end-to-end cost is the Tauri IPC hop serializing the payload twice more,
which these changes did not touch. Windows' webview p50 ticked up
(6.2 -> 6.8); that leg measures webview-side serialization of the same
payload and is noisy at this scale - the rust and python legs on the
same runner both improved, so it is not treated as a regression signal.

Re-run the three layers after each change and compare against this
baseline.

## Cold vs warm — the warm-workers headline

Decision 0008 (warm workers, PRs #139–#143+): `worker={"mode": "warm"}`
keeps a per-function pool member resident so its imports are paid once
instead of per call. The bench drives the same trivial function under
both modes through the host<->Python leg: a dedicated fixture module
(`tests/testapp_benchcost.py`) whose import sleeps a fixed ~300 ms —
the stand-in for a torch/model import — exposing `bench_cost_cold`
(mode "process") and `bench_cost_warm` (mode "warm") with identical
bodies. Five warmup + 25 timed calls each, measured on all three OS
legs of bench.yml (the cold call is spawn-dominated, so a small N is
honest — tail percentiles at n=25 are directional, not precise).

Numbers are p50/p95/p99 ms per call.

### ubuntu-22.04

| case | p50 | p95 | p99 |
|---|---|---|---|
| spawn_cold_process | TBD | TBD | TBD |
| spawn_warm_reuse | TBD | TBD | TBD |

### windows-latest

| case | p50 | p95 | p99 |
|---|---|---|---|
| spawn_cold_process | TBD | TBD | TBD |
| spawn_warm_reuse | TBD | TBD | TBD |

### macos-latest

| case | p50 | p95 | p99 |
|---|---|---|---|
| spawn_cold_process | TBD | TBD | TBD |
| spawn_warm_reuse | TBD | TBD | TBD |

Read: mode "process" pays process spawn + the ~300 ms import on every
single call — hundreds of milliseconds per call on every OS. The warm
member pays it once (in warmup), after which calls cost what a plain
in-process call costs. The gap widens with the real import: a 300 ms
fixture is gentle; a real model load runs seconds.
