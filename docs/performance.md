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
cargo test -p tauri-plugin-pylair --test bench_latency -- --ignored --nocapture

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

Known targets for later work (identified, deliberately not implemented
in the baseline PR):

- the stdin writer task in `supervisor/spawn.rs` is unbuffered and does
  two writes per frame;
- `coerce_kwargs` runs `inspect.signature` and `get_type_hints` on every
  call;
- `dumps` re-parses its output for the integer guard.

Re-run the three layers after each change and compare against this
baseline.
