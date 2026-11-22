# 0002: Existing solutions — should we build on one instead?

Required by docs/maintainers/PROJECT_BRIEF.md section 10 before M1. Surveyed 2026-11-11.

## Candidates

### PyTauri (`pytauri/pytauri`, ~1.4k stars, Apache-2.0, active since 2024)

The most serious alternative. Python bindings for Tauri via PyO3: the Python
interpreter is **embedded in-process** in the app; Python code calls
`builder_factory().build(...)` and owns the Tauri app object. Ships a
generated TypeScript client for IPC, native asyncio/trio/anyio support,
`tauri-cli` packaging integration, and a `pytauri-wheel` path that avoids
writing Rust entirely. This is real, maintained, and genuinely good at
typed IPC.

The architectural bet is different from ours: **in-process embedding vs.
supervised subprocess**. Consequences of their model:

- A Python crash (segfault in a C extension, OOM) takes the whole app down.
  There is nothing to supervise — crash isolation is impossible in-process.
- Cancellation of blocking work can't use process termination; it relies on
  interpreter-level interruption, which cannot safely preempt native code
  holding the GIL (exactly the torch/numpy/native-wheel case).
- Heavyweight native wheels must be embedded into the app build itself —
  the same packaging problem, solved with less room to maneuver than a
  relocatable interpreter tree.
- Their generated-TS-client approach does validate our codegen direction —
  independently arrived at and users respond well to it.

Honest assessment: for async-clean Python codebases where crash isolation
doesn't matter, PyTauri is simpler *today*. For local-AI workloads —
torch-class native deps, long-running cancellable jobs, crash recovery the
UI can observe — a supervised subprocess is the right architecture, and
that is the thing Pylair exists to make easy.

### marcomq/tauri-plugin-python (Tauri v2 plugin)

Calls Python functions from Rust in-process. Two backends: PyO3 (needs a
real CPython/`libpython` present at *build and run* time — does not solve
the no-Python-installed problem) and RustPython (no C extensions → no
numpy/pandas/torch). No supervision, streaming, cancellation, or packaging
story. Not a fit for the core goal.

### olivierlemasle/tauri-plugin-python

Self-described work-in-progress, RustPython-based. Same C-extension
limitation, plus immaturity. Not viable.

### PyInstaller sidecar + localhost HTTP (community status quo)

The pattern Tauri docs and examples (e.g.
`dieharders/example-tauri-v2-python-server-sidecar`) point at today:
PyInstaller-freeze the backend, register it as `externalBin`, talk to it
over a localhost HTTP server (usually FastAPI). Documented pain in those
very repos: can't reliably kill the real sidecar PID (Tauri sees the
bootloader process, not the child), localhost HTTP adds port conflicts and
a network surface, no type sync, no supervision, dev runs a venv while prod
runs a frozen binary — the dev/prod parity problem we target. This status
quo is the strongest evidence for the project: everyone is assembling this
by hand and hitting the same walls.

### pywebview

Python-owns-the-window model: `window.pywebview.api.<fn>()` calls exposed
Python objects, results returned as promises. Packaging is delegated to
PyInstaller/Nuitka (their own docs warn about missing `libpython` and
over-bundling). No TypeScript codegen, no streaming/cancellation
framework, no installer/signing story, dev story is "run Python". Fine for
internal tools; not a shipping-product path, and not a Tauri app.

## Decision

Build from scratch per the brief. No existing option covers the integrated
surface — relocatable bundled runtime + locked env + supervision + typed
IPC + streaming + cancellation + dev/prod parity + signed installers.
PyTauri is the only project with real overlap, and only on the typed-IPC
axis; its in-process model is the wrong tradeoff for local-AI workloads
(native-wheel fragility, uncancellable blocking work, no crash isolation).

## Consequences / watch items

- Re-evaluate at M6 / v0.1: if PyTauri's packaging story matures for
  torch-class apps, reassess.
- Borrow freely where their UX is proven: generated TS client shape,
  decorator-based exposure.
- Our differentiation to keep sharp: supervision + crash recovery,
  process-isolated cancellation, dev/prod parity, one-command install.
