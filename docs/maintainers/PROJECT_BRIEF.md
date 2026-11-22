# Pylair (working title): Project Brief

Read this whole document before writing any code. It describes what we are building, why, the order to build it in, and the rules for working on it. When something here is ambiguous, pick the simplest option that satisfies the acceptance criteria, write down the decision in `docs/decisions/`, and keep moving.

## 1. One-paragraph summary

Pylair is a desktop framework for Python and local AI applications. A developer with a Python app, an agent, a LangChain workflow or an Ollama integration should be able to run one command and get a working desktop application with a modern web front end, then run one more command and get a signed, installable build that works on a machine with no Python installed. Tauri provides the shell. Pylair provides everything between the shell and the developer's Python code: packaging, process supervision, a typed bridge, streaming, cancellation and diagnostics.

The name is a placeholder. "Pylair" implies official affiliation with the Tauri project, so do not hard-code the name in places that are expensive to change. Use a single constants file for the product name, CLI binary name and package scope.

## 2. The problem

Today, shipping a Python AI app as a desktop product means solving all of this by hand:

- Bundling a Python interpreter and dependencies (including native wheels such as numpy and torch) so users do not need Python.
- Starting, stopping, monitoring and restarting a Python process from a Rust/Tauri shell.
- Defining an IPC protocol and keeping Python and TypeScript types in sync.
- Streaming tokens or progress from long-running Python work to the UI.
- Cancelling that work safely.
- Building installers for Windows, macOS and Linux, including signing and notarization.

Each of these is a few days of work, and together they are why most Python AI projects never get a real desktop app. Pylair solves them once, in one coherent developer experience.

## 3. Goals and non-goals

### Goals (v0.1)

1. `npx create-pylair my-app` scaffolds a working project.
2. `pylair dev` runs the Python backend and the front end with hot reload for both.
3. `pylair build` produces an installer that runs on a clean machine with no Python.
4. Python functions can be exposed with a decorator and called from TypeScript with generated types.
5. Async generators in Python stream to the front end. Any running task can be cancelled.
6. If the Python process crashes, the supervisor restarts it and the UI is told what happened.
7. Dev and packaged builds behave identically from the developer's point of view.

### Non-goals (v0.1)

- Offline sync, hosted observability, cloud deployment, auto-update infrastructure, plugin marketplace.
- Supporting every Python packaging tool. We pick one path and make it excellent.
- Mobile targets.
- A required local HTTP server. FastAPI support is an optional integration, never a dependency of the core.
- Being a general Tauri replacement. We build on Tauri, we do not fork it.

## 4. Architecture

```
React / Svelte / Vue         (developer's UI)
        |
Pylair client SDK (TypeScript)   typed calls, streams, cancellation, diagnostics
        |
Tauri app + Rust core        windows, process management, security, packaging
        |
Managed Python runtime       lifecycle, environment, crash recovery
        |
Developer's Python code      plain functions, agents, LangChain, Ollama, optional FastAPI
```

Layers and responsibilities:

| Layer | Responsibility |
|---|---|
| TS client SDK (`@pylair/client`) | Typed `python.<fn>()` calls, `AsyncIterable` streams, `AbortSignal` cancellation, runtime status events |
| Rust core (Tauri plugin) | Spawn and supervise the Python runtime, own the IPC channel, enforce which commands the front end may call, surface diagnostics |
| Python runtime package (`pylair`) | `@expose` decorator, protocol server, task runner, cancellation, schema export |
| CLI (`pylair`) | `dev`, `build`, `doctor`, `codegen` |
| Scaffolder (`create-pylair`) | Project templates |

### Design decisions (starting positions, challenge them with evidence)

**Python distribution.** Use a relocatable standalone interpreter (python-build-standalone) plus a locked environment built with `uv`, copied into the app bundle as a resource. Do not default to PyInstaller-style single-file freezing. Reasons: native wheels behave normally, smaller surprises with torch and friends, simpler crash recovery and updates, easier to debug. Evaluate PyInstaller only as a later opt-in.

**Transport.** Default to JSON-RPC 2.0 style messages over stdio between Rust and the Python process, with length-prefixed framing. Keep the transport behind a trait/interface so a Unix socket or Windows named pipe can replace it later without touching the SDK. No HTTP in the default path.

**Streaming.** Python async generators map to a stream of chunk messages followed by a terminal message (done or error). The TS side exposes them as `AsyncIterable`. Define backpressure behaviour explicitly, even if v0.1 just buffers with a documented limit.

**Cancellation.** This is the hardest correctness problem in the project, so design it first.
- Async tasks: cancel via `asyncio.Task.cancel()`.
- Blocking or CPU-bound sync functions cannot be killed safely inside a thread. For functions marked as `isolated=True`, run them in a worker subprocess that the supervisor can terminate.
- Define what "cancelled" guarantees for each mode (resources released, partial results discarded or delivered) and write that into the docs.

**Type generation.** Source of truth is Python type hints and pydantic models. Export a JSON Schema, then generate TypeScript types and a typed `python` client from it. Generated files are committed to the app project, not hidden.

**Crash recovery.** The Rust supervisor owns restart policy: exponential backoff with a cap, a crash-loop breaker, and clear events to the UI (`starting`, `ready`, `crashed`, `restarting`, `failed`). In-flight calls fail with a typed error, they do not hang.

**Local AI.** Ollama detection and model status are an optional integration package, not core. It should report: installed or not, running or not, available models, and basic health. Bundling a model runtime (for example llama.cpp) is a later decision.

**Security.** The front end can only call functions that were explicitly exposed. No arbitrary command execution. Respect Tauri's capability and permission model. Document the threat model briefly in `docs/security.md`.

## 5. Proposed API (illustrative, not final)

Python:

```python
from pylair import expose

@expose
async def summarize(text: str) -> str:
    return await my_model.summarize(text)

@expose
async def chat(prompt: str):
    async for token in my_model.stream(prompt):
        yield token

@expose(isolated=True)
def heavy_job(path: str) -> dict:
    ...
```

TypeScript:

```ts
import { python } from "@pylair/client";

const summary = await python.summarize({ text: "Summarize this document" });

const controller = new AbortController();
for await (const token of python.chat({ prompt: "Hello" }, { signal: controller.signal })) {
  render(token);
}
```

Treat this as a starting sketch. If a better shape emerges while building, change it and record why.

## 6. Repository layout (proposed)

```
/
  README.md
  PROJECT_BRIEF.md
  docs/
    decisions/          one short markdown file per decision
    security.md
    cancellation.md
  crates/
    tauri-plugin-pylair/   Rust plugin: supervisor, IPC, permissions
  packages/
    client/                    @pylair/client (TypeScript)
    cli/                       pylair CLI
    create-pylair/         scaffolder
  python/
    pylair/                Python runtime package
  integrations/
    ollama/                    optional
    fastapi/                   optional
  templates/
    react-ts/
    svelte-ts/                 later
  examples/
    reference-app/             the validation app (section 8)
  .github/workflows/           CI incl. clean-machine install tests
```

Use a monorepo with pnpm workspaces for JS, a Cargo workspace for Rust, and `uv` for Python. Pin toolchain versions in the repo.

## 7. Milestones

Work in this order. Do not start a milestone until the previous one meets its exit criteria. Open a PR per milestone with a short write-up of what was learned.

### M0: Feasibility spike (packaging first)

The riskiest part goes first. Build a throwaway Tauri app that bundles a standalone Python plus a small locked environment, spawns it as a sidecar, and round-trips one message.

Exit criteria:
- Installer builds on Windows, macOS and Linux in CI.
- The installed app runs on a clean VM with no Python installed on each OS.
- Then repeat with a heavyweight dependency (torch CPU or similar) and record bundle size, build time and any failures. Write the findings to `docs/decisions/0001-python-packaging.md`.

If M0 cannot be made reliable with a torch-class dependency, stop and report. That is a project-level risk and the decision on whether to continue is the owner's.

### M1: Supervisor and protocol

- Rust plugin that spawns, monitors and restarts the Python runtime.
- Framing, request/response, error model, versioned protocol handshake.
- Lifecycle events exposed to the front end.
- Fault injection tests: kill the process mid-call, hang on startup, emit malformed frames.

### M2: Typed bridge

- `@expose` decorator, schema export, TS codegen.
- `python.<fn>()` calls with typed arguments, results and errors.
- Same behaviour in `dev` and in a packaged build (add a test that proves it).

### M3: Streaming and cancellation

- Async generator streaming, `AsyncIterable` on the TS side.
- `AbortSignal` cancellation for async tasks, process-isolated cancellation for blocking tasks.
- Documented guarantees in `docs/cancellation.md` and tests for each mode.

### M4: CLI and scaffolder

- `create-pylair` with a React + TypeScript template.
- `pylair dev`, `build`, `doctor` (preflight checks for toolchains, signing config, disk space, Ollama if configured).
- Clear, actionable error messages. A first-time user should never see a raw Rust or Python stack trace without a plain-language explanation above it.

### M5: Local AI integration

- Optional Ollama package: detect, connect, list models, report status, stream completions through the normal bridge.

### M6: Reference app and release candidate

Build the reference app described next, harden, write docs, and tag v0.1.0-rc.

## 8. The reference application (the real acceptance test)

One example app in `examples/reference-app/` that does all of the following, and is also the end-to-end test suite's target:

1. Runs a real Python AI workflow (for example a LangChain or plain-Python agent calling a local Ollama model).
2. Streams output token by token into a React UI.
3. Cancels a running task from the UI, safely, with resources released.
4. Recovers from a deliberately crashed Python process, with the UI showing the state transitions.
5. Installs and runs on clean Windows, macOS and Linux machines with no Python installed.

If the reference app works reliably, the framework is real. If it does not, nothing else matters yet.

## 9. Success metrics for v0.1

- Time from `npx create-pylair` to a running dev window: under 5 minutes on a normal connection, excluding first-time toolchain installs.
- Time from a working dev app to an installable build: one command, under 15 minutes on CI.
- Clean-machine install test passes on all three OS targets for the reference app.
- Cancellation test suite passes with no leaked processes.
- Crash recovery test suite passes, including crash loops.
- A developer can add an exposed Python function and call it from TS with full types in under 2 minutes.

## 10. Competitive context

There are existing ways to bundle Python with Tauri (for example PyTauri and tauri-plugin-python) and to talk to a Python backend from a webview (for example pywebview). Before M1, spend a short time evaluating the current state of these and write a one-page comparison in `docs/decisions/0002-existing-solutions.md`: what they do well, what they lack, and whether we should build on any of them instead of from scratch. Our claim is that the integrated experience of supervision, streaming, cancellation, recovery and dev/prod parity is substantially better. The comparison should test that claim honestly. If an existing project already covers most of it, say so.

## 11. Working agreements for Devin

- Work milestone by milestone. Keep PRs small and focused, one concern each.
- Every PR includes tests. Anything touching the Rust supervisor or the protocol needs fault-injection tests, not just happy-path tests.
- CI must run on Windows, macOS and Linux from the first PR that has code. Clean-machine install tests are not optional and not deferred.
- Prefer boring, well-maintained dependencies. Justify any new dependency in the PR description.
- Record non-obvious choices in `docs/decisions/` as short numbered files: context, decision, consequences.
- Do not invent features outside the goals above. Put ideas in `docs/ideas.md` instead.
- Error messages are part of the product. Write them for someone who has never seen Rust.
- When blocked on a decision that is genuinely the owner's (naming, licensing, scope changes, anything that changes the goals), stop and ask with a short summary of options and a recommendation. Everything else, decide and document.
- Do not commit secrets, signing certificates or API keys. Signing in CI uses repository secrets that the owner will configure. Until then, build unsigned and note it.

## 12. Open questions for the owner

1. Final name, and licensing (suggest MIT or Apache-2.0).
2. Minimum supported OS versions.
3. Whether Linux packaging targets AppImage, deb, or both first.
4. Whether to bundle an inference runtime in a later release or keep Ollama as an external dependency.
5. Budget for code signing (Apple Developer account, Windows certificate).

## 13. Definition of done for v0.1

All goals in section 3 are met, the reference app passes on clean machines for all three OS targets, docs cover getting started, the API, cancellation semantics, packaging and troubleshooting, and the comparison with existing solutions is written and honest.
