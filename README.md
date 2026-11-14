# Pylair (working title)

Desktop framework for Python and local AI applications: run one command,
get a Tauri desktop app with a modern web front end and a managed,
bundled Python runtime — no Python install required for end users.

See [PROJECT_BRIEF.md](PROJECT_BRIEF.md) for goals, architecture and
milestones. The name is a placeholder; naming lives in `product.json`.

## Status

Release candidate. `pylair` and `pylair-ollama` are on PyPI,
`tauri-plugin-pylair` is on crates.io; npm packages follow.

Install RCs with **exact pins**, not `--pre`:

```sh
pip install pylair==0.1.0rc9 pylair-ollama==0.1.0rc2
```

`--pre` opts *every* package in the install into prereleases — it can
pull your dependencies into unstable versions and break for reasons
unrelated to this project. Exact pins avoid that.

Each package versions independently across registries — the numbers
won't match (`pylair` at `0.1.0rc9`, npm packages at `0.1.0-rc.15`, the
crate at `0.1.0-rc.11`). Check each registry for its latest RC.

Try it:

```sh
npx create-pylair my-app && cd my-app
npm install && uv sync
npx pylair dev
```

The first `pylair dev` compiles the Rust/Tauri side from source —
expect a few minutes before the window appears; later runs reuse the
build cache and start in seconds.

(`pylair` 0.1.0rc2 may print a cosmetic `writer dropped a frame` line
to stderr at shutdown — harmless; the fix lands in the next RC.)

Milestones: `M0` packaging spike → `M1` supervisor/protocol → `M2` typed
bridge → `M3` streaming/cancellation → `M4` CLI/scaffolder → `M5` Ollama
integration → `M6` reference app + RC.

## Known limitations

- **Invocation is capability-level; task control is owner-scoped.** A
  webview can only call `python_call` if a capability grants it
  `pylair:*` permissions — the template grants them to
  `windows: ["main"]` only, and remote origins get no IPC access unless
  a capability adds `remote.urls`. Within a granted capability, every
  covered webview can *invoke* every `@expose`d function — Pylair adds
  no per-window or per-function ACL on invocation. But once a task
  exists, only its owning webview (the Tauri window label — never a
  JS-supplied id) can cancel it or grant it stream credit, a task id
  from a dead runtime generation controls nothing, and closing the
  owner window cancels its tasks. Call/stream delivery is bound to the
  invoking webview via a per-call IPC channel; broadcast lifecycle
  events (state/exposes) remain readable by any webview, and stderr
  delivery is opt-in per window (see Known limitations); invocation
  scoping beyond capabilities is planned with wrap mode. The exposure exists only if
  you widen a capability (more windows, `remote.urls`) or load untrusted
  content in a privileged window.
- **Inline sync functions can't be interrupted.** A non-isolated sync
  `def` keeps running after cancel — `cancel()` reports
  `requested_may_continue`, the caller gets `TASK_CANCELLED` immediately
  and the result is discarded, but the Python work may run to
  completion. Mark anything long-running `@expose(isolated=True)` — see
  [docs/cancellation.md](docs/cancellation.md).
- **Async cancel is cooperative.** A coroutine blocked in native code
  can't be preempted until control returns to the event loop; a wedged
  runtime is killed via missed pongs instead. `loop_stall_kill` can make
  loop stalls fatal sooner — it's off by default because legitimate
  blocking work (multi-GB model loads) can stall the loop for minutes.
- **A wedged runtime can survive an abrupt host kill on macOS.** If the
  app dies while Python can't read stdin (blocked in native code), the
  stdin-EOF fallback never reaches it and macOS has no parent-death
  signal — the runtime may be left running. On Linux
  `PR_SET_PDEATHSIG` and on Windows the Job Object kill the runtime
  even then — those paths are exercised by the abrupt-kill tests on the
  Windows and Linux CI legs; macOS coverage is stdin-EOF (healthy
  runtimes) plus the isolated worker's own parent watcher.
- **`isolated=True` respawns a worker per call.** Each call re-imports
  its modules — fine for short calls, seconds each with torch-class
  imports. A warm persistent worker (`worker={'mode': 'warm'}`) is
  reserved in the API but not implemented yet.
- **Worker kill covers only the worker's process group.** Descendants
  that stay in the group die with it (SIGTERM-ignoring grandchildren
  included), but a child that deliberately detaches — `setsid`, a new
  process group, a double-fork daemon — escapes the group kill.
- **Resource limits are per runtime, not per window.** Concurrent tasks,
  streams, queued commands, buffered stream bytes (per-stream and total),
  outstanding stream credit, isolated workers and frame size are all
  bounded (`Limits` on `SupervisorConfig`, `PYLAIR_MAX_*` env overrides;
  effective values are logged at startup). A webview that floods the
  bridge can starve every other window — limits exist to keep one
  runtime's failure bounded and explicit, not to guarantee fairness
  between windows. On a hit the call/stream fails with
  `LIMIT_EXCEEDED` (`-32040`), surfaced in JS as `LimitExceededError`
  with `.limit` naming the cap. If the host itself stops draining, the
  Python outbound queue fills, the runtime logs `outbound queue full —
  host not draining` and exits rather than growing without bound —
  callers see `RUNTIME_CRASHED`, not a silent hang.
- **Ints beyond ±2^53 can't cross the bridge.** The call fails with
  `INTERNAL: integer … exceeds the JSON safe-integer range` — an
  explicit error rather than a silently rounded value. Return large
  integers as strings until `{"$bigint"}` tagging lands
  (`docs/ideas.md`).
- **Broadcast events remain readable by every webview.** Call/stream
  data is delivered on a per-call `tauri::ipc::Channel` bound to the
  invoking webview — it cannot be passively observed or forged by
  another window (the old `pylair://stream` bus is gone). But
  `pylair://state` and `pylair://exposes` are still broadcast events:
  `emit_to`-addressed yet receivable by any `Any`-target listener.
  `pylair://stderr` is opt-in per window (`Builder::stderr_delivery`,
  default `["main"]`, `All` restores broadcast): a window outside the
  list gets no stderr through a label-scoped listener, though a
  deliberate `Any`-target `listen` still sees emitted lines — the
  event bus offers no stronger boundary. The template's
  `deny-emit`/`deny-emit-to` entries block forging those broadcasts.
  Task *control* (cancel, stream credit) stays owner-scoped and is
  unaffected.
- **`cargo audit` flags `glib 0.18.5` (RUSTSEC-2024-0429, unsound).**
  It reaches Linux builds only, transitively through Tauri's GTK chain
  (`tauri → wry → webkit2gtk → gtk`); the advisory is tracked upstream
  and disclosed in `docs/security.md` §10 — no fix exists until Tauri
  moves off gtk-rs 0.18.
- **macOS, rc.8: launching through a symlinked path panics at startup.**
  Tauri rejects an executable path containing a symlinked ancestor —
  `/tmp` → `/private/tmp` is the common case — as process-relaunch
  hardening, so `app.path().resource_dir()` errors and
  `tauri-plugin-pylair ≤ 0.1.0-rc.5` crashed with
  `resource_dir: UnknownPath`. Launch the app's real path instead.
  Fixed in the next train: the plugin falls back to resolving resources
  from the canonicalized executable and reports an actionable startup
  error when neither lookup succeeds.

## Layout

| Path | What |
|---|---|
| `crates/tauri-plugin-pylair` | Rust plugin: supervisor, IPC, permissions |
| `packages/client` | `@pylair/client` — typed calls, streams, cancellation |
| `packages/cli` | `pylair` CLI — dev, build, doctor, codegen |
| `packages/create-pylair` | project scaffolder |
| `python/pylair` | Python runtime — `@expose`, protocol server, tasks |
| `integrations/` | optional: ollama (M5), fastapi (later) |
| `templates/` | scaffold templates (not in pnpm workspace, by design) |
| `examples/` | `reference-app` — the real acceptance test (M6) |
| `docs/decisions/` | numbered decision records |
| `product.json` | single source of truth for naming |

## Dev setup

Toolchain (pinned in repo): Node 22, pnpm 10.18.3 (corepack), Rust 1.92.0,
uv (any recent), Python 3.12 via uv. On Windows you also need the Visual
Studio Build Tools "Desktop development with C++" workload.

```sh
pnpm install            # JS workspace
pnpm build              # build all TS packages
cargo build             # Rust workspace
cd python/pylair && uv sync   # Python env (fetches managed CPython)
```

Docs: [security](docs/security.md), [protocol](docs/protocol-v1.md),
[cancellation](docs/cancellation.md), [codegen](docs/codegen.md),
[decisions](docs/decisions/), [ideas](docs/ideas.md).

## License

Dual-licensed under MIT or Apache-2.0 — pick whichever fits your project.
See [LICENSE-MIT](LICENSE-MIT) and [LICENSE-APACHE](LICENSE-APACHE).

Installers produced by `pylair build` are **unsigned** unless you configure
code signing — expect Gatekeeper (macOS) and SmartScreen (Windows)
warnings on unsigned builds. On macOS the bundle is ad-hoc signed only:
to open a downloaded build, right-click the app and choose **Open**, or
run `xattr -dr com.apple.quarantine <app>`. This route is documented from
the security model — it has not been verified on a physical Mac yet.
A first build downloads the bundled Python
runtime and Tauri's installer toolchain (WiX/NSIS on Windows) from
GitHub, so it needs network access once; later builds reuse the caches.
See [docs/build.md](docs/build.md).

Pylair builds on [Tauri](https://tauri.app) but is not affiliated with or
endorsed by the Tauri project. Apps it produces bundle third-party
software (a standalone CPython build and your Python dependencies);
their licenses ship inside each bundle under `THIRD-PARTY-NOTICES`.
