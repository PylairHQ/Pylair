# Pylair

Desktop apps powered by Python, without asking users to install Python.

Pylair is a framework for Python developers who want to ship a real
desktop application. It combines a Tauri shell, a managed and bundled
CPython runtime, and a typed bridge so your web front end can call Python
functions directly. If you have a Python app, an agent, a LangChain
workflow or an Ollama integration, one command gives you a dev loop with
a live window, and one more gives you native installers for Windows,
macOS and Linux that work on machines with no Python installed.

## Quickstart

```sh
npx create-pylair@latest my-app
cd my-app
npm install
uv sync
npx pylair dev
```

The first `pylair dev` compiles the Rust/Tauri side from source, so
expect a few minutes before the window appears; later runs reuse the
build cache and start in seconds. `npx pylair build` produces the
installer for your platform.

## Two modes

**Bridge mode** treats your Python functions as the app. Decorate them
with `@expose`, call them from TypeScript with generated types, stream
results and cancel them. The front end is an ordinary web app you own.

**Wrap mode** turns an existing Python web app into the app. A Gradio,
Streamlit, FastAPI, Flask, NiceGUI, Dash or Panel app serves on
`127.0.0.1` under the same supervisor and shows in the Pylair window:

```sh
npx pylair wrap app.py     # turns an existing project into a wrap app
npx pylair dev
```

or `npx create-pylair my-app --mode wrap` for a new one. The Gradio and
Streamlit adapters pin the loopback bind, `share=False` and telemetry
off; `[tool.pylair.wrap]` is the config. Full guide, recipes and known
limitations: [docs/wrap.md](docs/wrap.md).

## Features

- **Typed bridge and codegen.** `@expose`d Python functions callable
  from TypeScript; `pylair codegen` emits the client surface from your
  signatures ([docs/codegen.md](docs/codegen.md)).
- **Streaming with backpressure.** Per-stream credit on the data lane,
  so a fast producer cannot outrun the front end.
- **Cancellation with defined semantics.** Every execution mode's
  guarantee is specified and tested
  ([docs/cancellation.md](docs/cancellation.md)).
- **Per-function permissions.** Capabilities can restrict which
  functions a window may call.
- **Per-window fairness.** One window cannot starve the others of queue,
  task, stream or worker capacity.
- **Runtime health.** `health()` returns the host-observed snapshot and
  `onHealth` fires when a liveness verdict changes.
- **App readiness.** `@on_startup` hooks report `loading`/`ready`/
  `failed` per component, `@expose(requires=…)` gates calls on them,
  and `onApp` fires on phase changes.
- **Warm isolated workers.** `@expose(worker={"mode": "warm"})` keeps a
  loaded model resident in a persistent per-function pool — pay the
  import once, call in microseconds; POSIX cancels interrupt the call
  and keep the worker
  ([docs/cancellation.md](docs/cancellation.md)).
- **Model downloads.** `pylair.models` fetches multi-GB models over
  https into the app data dir — sha256-verified, resumable,
  readiness-gated — with no Python or downloader for the user to
  install ([docs/models.md](docs/models.md)).
- **Blob payloads.** `pylair.Blob` returns large binaries as host-served
  URLs — no base64 in the frame, HTTP range streaming on the way out,
  per-window ownership enforced by the host
  ([docs/blobs.md](docs/blobs.md)).
- **Bundled CPython, native installers.** End users install nothing
  Python-related; `pylair build` produces Windows, macOS and Linux
  installers on clean machines.
- **Ollama integration.** The optional `pylair-ollama` package rides the
  normal bridge.

## Status

Release candidate. `pylair` and `pylair-ollama` are on PyPI,
`tauri-plugin-pylair` is on crates.io, and `@pylair/client`,
`@pylair/cli` and `create-pylair` are on npm.

Install RCs with **exact pins**, not `--pre`:

```sh
pip install pylair==0.1.0rc16 pylair-ollama==0.1.0rc3
```

`--pre` opts *every* package in the install into prereleases. It can
pull your dependencies into unstable versions and break for reasons
unrelated to this project. Exact pins avoid that.

Each package versions independently across registries: the numbers
won't match (`pylair` at `0.1.0rc16`, npm packages at `0.1.0-rc.30`, the
crate at `0.1.0-rc.23`). Check each registry for its latest RC.

## Documentation

- [docs/](docs/): the index of guides and specs, including the
  [protocol spec](docs/protocol-v1.md),
  [cancellation semantics](docs/cancellation.md),
  [codegen](docs/codegen.md), [packaging and installers](docs/build.md)
  and the [wrap guide](docs/wrap.md).
- Auto-update: [docs/updates.md](docs/updates.md) — **not available yet**;
  the doc ships ahead of the feature (decision 0011).
- Security model: [docs/security.md](docs/security.md); reporting a
  vulnerability: [SECURITY.md](SECURITY.md).
- Design history: [docs/decisions/](docs/decisions/).
- Contributing: [CONTRIBUTING.md](CONTRIBUTING.md).

## Known limitations

- **Invocation is capability-level; per-function scopes are opt-in.** A
  webview needs a capability granting `pylair:*` to call Python at all
  (the template grants `main` only), and every covered webview can call
  every `@expose`d function unless you add `allow`/`deny` entries
  ([docs/security.md](docs/security.md)). Function names stay visible
  via `pylair://exposes`. Task control is owner-scoped: only the owning
  window can cancel its task or grant it stream credit.
- **Inline sync functions can't be interrupted.** A non-isolated sync
  `def` keeps running after cancel; mark long-running work
  `@expose(isolated=True)` ([docs/cancellation.md](docs/cancellation.md)).
- **Async cancel is cooperative.** A coroutine blocked in native code
  can't be preempted; `loop_stall_kill` can make stalls fatal but is off
  by default (legitimate loads can stall the loop for minutes).
- **macOS is CI-tested only.** Windows and Linux are the hand-verified
  platforms. macOS builds, ad-hoc signs and passes the same suite, but
  no maintainer runs it day to day.
- **A wedged runtime can survive an abrupt host kill on macOS.** macOS
  has no parent-death signal; Linux `PR_SET_PDEATHSIG` and Windows Job
  Objects cover that case.
- **`isolated=True` respawns a worker per call.** Fine for short calls;
  `worker={"mode": "warm", "pool": N}` keeps a persistent per-function
  pool instead — on POSIX a cancel interrupts the call first and a
  surviving worker serves the next call; Windows is kill-only
  (CTRL_BREAK cannot reach a console-less worker)
  (see docs/cancellation.md).
- **Worker kill covers the worker's process group.** A descendant that
  deliberately detaches (`setsid`, a new process group) escapes it.
- **Ints beyond ±2^53 can't cross the bridge.** Return large integers as
  strings until `{"$bigint"}` tagging lands ([docs/ideas.md](docs/ideas.md)).
- **Broadcast events are readable by every webview.** Call and stream
  data travels on a per-call channel bound to the invoking webview, but
  `pylair://state` and `pylair://exposes` are broadcast; `pylair://stderr`
  is opt-in per window (`Builder::stderr_delivery`, default `["main"]`).
- **Health and readiness are observable only.** `health()` is a snapshot
  and `health().app` reports what the app declared; neither can steer the
  runtime, and a pre-readiness host or package reports `app: null`.
- **`cargo audit` flags `glib 0.18.5` (RUSTSEC-2024-0429).** It reaches
  Linux builds transitively through Tauri's GTK chain and is disclosed in
  [docs/security.md](docs/security.md); no fix exists until Tauri moves
  off gtk-rs 0.18.
- **Resource limits bound the runtime, with per-window fairness.**
  Concurrent tasks, streams, queued commands and workers are capped and
  split per live window; over-share calls get `LIMIT_EXCEEDED`
  (`data.limit = "window_share"`). `PYLAIR_MAX_*` env vars tune the caps.

## Layout

| Path | What |
|---|---|
| `crates/tauri-plugin-pylair` | Rust plugin: supervisor, IPC, permissions |
| `packages/client` | `@pylair/client`: typed calls, streams, cancellation |
| `packages/cli` | `pylair` CLI: dev, build, doctor, codegen |
| `packages/create-pylair` | project scaffolder + `templates/` shipped in its tarball |
| `python/pylair` | Python runtime: `@expose`, protocol server, tasks |
| `integrations/` | optional: ollama, fastapi |
| `examples/reference-app` | the real acceptance app |
| `docs/` | specs, guides and `decisions/`; `docs/maintainers/` for maintainer docs |
| `product.json` | single source of truth for naming |

## Milestones

`M0` packaging spike → `M1` supervisor/protocol → `M2` typed bridge →
`M3` streaming/cancellation → `M4` CLI/scaffolder → `M5` Ollama
integration → `M6` reference app + RC. The full brief lives at
[docs/maintainers/PROJECT_BRIEF.md](docs/maintainers/PROJECT_BRIEF.md).

## License

Dual-licensed under MIT or Apache-2.0. Pick whichever fits your project.
See [LICENSE-MIT](LICENSE-MIT) and [LICENSE-APACHE](LICENSE-APACHE).

Installers produced by `pylair build` are **unsigned** unless you configure
code signing: expect Gatekeeper (macOS) and SmartScreen (Windows)
warnings on unsigned builds. On macOS the bundle is ad-hoc signed only:
to open a downloaded build, right-click the app and choose **Open**, or
run `xattr -dr com.apple.quarantine <app>`. This route is documented from
the security model; it has not been verified on a physical Mac yet.
A first build downloads the bundled Python
runtime and Tauri's installer toolchain (WiX/NSIS on Windows) from
GitHub, so it needs network access once; later builds reuse the caches.
See [docs/build.md](docs/build.md).

Pylair builds on [Tauri](https://tauri.app) but is not affiliated with or
endorsed by the Tauri project. Apps it produces bundle third-party
software (a standalone CPython build and your Python dependencies);
their licenses ship inside each bundle under `THIRD-PARTY-NOTICES`.
