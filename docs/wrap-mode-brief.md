# Wrap mode: brief for Devin

Read this whole document, plus `docs/maintainers/PROJECT_BRIEF.md`, `docs/protocol-v1.md` and `docs/build.md`, before writing code. Wrap mode is a second way to use Pylair. Everything in `docs/maintainers/PROJECT_BRIEF.md` section 11 (working agreements) applies here too.

Sequencing: this is independent of M6 (the reference app) and can run on its own branch. Do not let it delay the M6 release candidate, which is the proof of Pylair's core claim.

## 1. What wrap mode is

Today Pylair's "bridge mode" is for developers building a custom React UI that calls `@expose`d Python functions. Wrap mode is for the much larger group who already have a web UI written in Python (Gradio, Streamlit, NiceGUI, Dash, Panel, FastAPI/Flask) and just want a signed, double-click desktop app with no Python install.

    pylair wrap app.py        # or: pylair init --mode wrap
    pylair dev                # app in a native window, with reload
    pylair build              # installer for the host OS

Same bundled interpreter, same locked env, same supervision, same installers. The UI is the user's own web framework, shown in the Tauri window. No React, no Rust, no typed bridge required.

Why it matters: it is the lowest-friction on-ramp to Pylair, and it keeps working where other tools stop. Existing tools cover pieces only: `streamlit-desktop-app` freezes with PyInstaller (Python-version-limited), `stlite` runs Streamlit in WebAssembly (no native wheels, so no torch), `streamlitrunner` is a pywebview dev runner, `pyappdist` builds installers without a window or supervision, Pake wraps a URL without bundling Python. Nobody combines bundled runtime + supervised server + native window + signed installers across frameworks.

## 2. Decisions already made (record them in `docs/decisions/0003-wrap-mode.md`)

1. **Frameworks, in order:** a generic `command` mode first (any program that serves HTTP on a port we give it), then convenience adapters for **Gradio** and **Streamlit**. NiceGUI, Dash, Panel, FastAPI and Flask are covered by `command` mode and get documented recipes, not adapters, in v1.
2. **One tool, two modes.** `[tool.pylair] mode = "bridge" | "wrap"`. `build`, `dev`, `doctor` and the supervisor are shared. `pylair wrap` is sugar for `init --mode wrap`.
3. **Dependencies must come from a manifest** in v1 (`pyproject.toml` with `uv.lock`, or `requirements.txt` as the warned fallback). A bare `app.py` gets a helpful error that offers to run `uv init`. Inferring dependencies from imports is out of scope.
4. **Security posture:** loopback only, random port, no Tauri IPC exposed to the wrapped page, navigation locked to the app origin, tunnels and sharing disabled. Documented honestly as the same exposure as running `streamlit run` locally. See section 6.

If any of these turn out to be wrong in the spike, stop and report rather than quietly changing them.

## 3. Architecture

The control channel stays the existing stdio protocol. The data channel is HTTP between the window and the app.

    Tauri window (WebviewUrl::External http://127.0.0.1:PORT)
            |  HTTP + WebSocket
    user's web app (Gradio / Streamlit / any server)   <- child process
            ^  launched by
    python -m pylair.wrap   (launcher shim; speaks protocol over stdio)
            ^  supervised by
    Rust supervisor (spawn, kill tree, ping, restart, events)

### 3.1 Launcher shim (`python/pylair/wrap/`)

- Entry: `python -m pylair.wrap --framework {gradio|streamlit|command} --entry app.py [--cmd ...]`.
- Reuses the runtime's stdout hygiene (private frame fd, fd 1 pointed at stderr) and reader-thread pongs. Wrap mode must not regress any M1 behaviour.
- Picks a free loopback port (bind port 0, read it, close, retry on bind failure up to 5 times), exports it as `PYLAIR_PORT`, starts the app, and polls `GET /` (or `ready-path`) on `127.0.0.1` until it returns a non-5xx response.
- Sends the normal `hello` immediately, then a new notification `ready {url}` once the HTTP probe succeeds. Add `ready` to `docs/protocol-v1.md`.
- Server exit, in any form, makes the launcher exit non-zero so the supervisor restarts it.
- Child tree: the launcher owns the server process, in its own process group (Job Object on Windows), so killing the launcher kills the server and anything it spawned. Test for leaked grandchildren, as in M3.

### 3.2 Supervisor changes (`crates/tauri-plugin-pylair`)

- New runtime kind `Web`. States stay `starting -> ready -> crashed -> restarting -> failed`, plus `unresponsive`.
- `Ready` is reached on `ready {url}`, not on `hello`. Use a separate `ready_timeout` (default 120s, configurable), because model loading at import time legitimately takes a long time. `hello_timeout` stays 15s.
- Health: protocol pings as today, plus an optional HTTP probe every 10s. HTTP probe failure counts as a miss, not an instant kill.
- Crash/restart reuses backoff and the crash-loop breaker. Fatal states (protocol mismatch) behave as before.
- Surface the child's stderr tail in the lifecycle events so the UI can show startup logs.

### 3.3 Window behaviour (plugin or template)

- Show a local "starting" page (bundled HTML) with a live log tail until `Ready`, then navigate to the app URL. Never show a blank window.
- On crash or restart, show a local "restarting" page with the reason, and navigate back when `Ready` returns. Do not leave the user on a dead connection page.
- Navigation policy: the window may only navigate within the app origin. Any other link opens in the system browser. `target=_blank` and `window.open` follow the same rule.
- Remote-origin pages get no Tauri IPC capability. Verify this with a test: from the wrapped page, `window.__TAURI__` must be absent or unusable.
- Downloads and file uploads must work. See the compatibility matrix in section 4.

### 3.4 Adapters

- **command:** `cmd = ["python", "-m", "myapp", "--port", "{port}"]`. `{port}` and `$PYLAIR_PORT` substitution; configurable `ready-path`, default `/`.
- **gradio:** run the user's entry with `runpy.run_path(..., run_name="__main__")` after patching `gradio.Blocks.launch` to force `server_name=127.0.0.1`, `server_port=PYLAIR_PORT`, `inbrowser=False`, `share=False`, and to block in the foreground. Set `GRADIO_ANALYTICS_ENABLED=False`. Handle both `demo.launch()` scripts and scripts that expose a `demo` object without launching.
- **streamlit:** run the Streamlit CLI/bootstrap with `--server.address 127.0.0.1 --server.port $PYLAIR_PORT --server.headless true --browser.gatherUsageStats false --server.fileWatcherType none` and the user's entry. Multipage apps must work.
- Adapters pin supported major versions of the framework and say so in docs. Unsupported version: warn, don't crash.

## 4. W0: spike first (do this before anything else)

Goal: find out whether real Gradio and Streamlit apps behave acceptably in all three native webviews, and whether the supervision model works for a long-starting HTTP server. Keep it throwaway, like the M0 spike, in `spikes/w0-wrap/`.

Build three sample apps: a Gradio chat app (text, file upload, file download, microphone input, image display), a Streamlit app (widgets, file uploader, download button, multipage), and a FastAPI app served in `command` mode. Run each under a hand-wired launcher and the existing supervisor, in a Tauri window, on Windows, macOS and Linux.

Compatibility matrix to fill in for each app and OS (pass / works with a fix / fails), written into `docs/decisions/0003-wrap-mode.md`:

- page loads and WebSockets / server-sent events stay connected
- text input, long streamed output
- file upload dialog, drag-and-drop upload
- file download (does a save dialog appear, where does the file go)
- microphone and camera permission prompts (macOS needs usage-description strings in the bundle; verify)
- clipboard copy and paste
- external link click opens the system browser
- window close kills the whole server tree; no orphan processes
- kill -9 the server: window shows the restarting page and recovers

Also measure and record: time from launch to `ready` for each app, installer size, and memory.

Exit criteria: the matrix is complete, every failure has either a documented workaround or an explicit "known limitation" entry, and a recommendation on whether W1 should proceed unchanged.

Kill criterion: if file upload or download cannot be made to work in one of the three webviews without invasive hacks, stop and report to the owner before building W1. Those two features are table stakes for these frameworks.

## 5. Milestones after the spike

- **W1: launcher + supervisor.** Section 3.1 and 3.2, with fault-injection tests: server never becomes ready (timeout), server crashes after ready, server hangs (HTTP probe fails), stray stdout from the app, port conflict retry, grandchild leak on kill.
- **W2: CLI + config + window.** `[tool.pylair]` wrap keys (below), `pylair wrap`, `init --mode wrap`, `dev`, `build`, `doctor` checks (framework installed, manifest present, port probe), starting and restarting pages, navigation policy, no-IPC test. Post-prune smoke test must import the framework with the bundled interpreter.
- **W3: adapters + acceptance.** Gradio and Streamlit adapters, recipes for NiceGUI, Dash, Panel and FastAPI as documented `command` configs, and the acceptance job below.
- **W4: docs.** `docs/wrap.md` (getting started in five minutes, per-framework recipes, threat model, known limitations from the W0 matrix), README section.

### Config

    [tool.pylair]
    mode = "wrap"

    [tool.pylair.wrap]
    framework = "streamlit"        # "gradio" | "streamlit" | "command"
    entry = "app.py"               # for adapters
    cmd = ["python", "-m", "myapp", "--port", "{port}"]   # for command mode
    ready-path = "/"
    ready-timeout = 120            # seconds

Window title, size, icon and bundle identity continue to come from `tauri.conf.json`, as in bridge mode.

## 6. Security

Document this in `docs/wrap.md` and `docs/security.md`:

- The wrapped server listens on `127.0.0.1` on a random port. Other local processes, and web pages in the user's browser, can attempt to reach it. This is the same exposure as `streamlit run` or `gradio` on a developer's machine. We do not claim more.
- The window gives the page no Tauri IPC, so a compromised page cannot reach the filesystem or shell through Tauri.
- Adapters disable public sharing/tunnels (`share=False`, no ngrok/Gradio share links) and telemetry.
- Hardening to evaluate later, not now: a Rust-side reverse proxy with a per-launch token and Host-header checks, and Unix-socket/named-pipe listening for frameworks that support it (uvicorn does). Note in the decision record that a proxy alone does not protect the app's own port.

## 7. Acceptance test (W3 exit)

Extend the existing `acceptance` CI job with a wrap matrix. For each sample app (Gradio, Streamlit, FastAPI `command`):

1. Scaffold with `pylair wrap`, install deps, `pylair build`.
2. Install on a clean machine with no Python (same clean-container / silent-install approach as bridge mode).
3. Run the installed app's `--self-test`, extended for wrap mode: start the real supervisor via the release spec, wait for `ready`, `GET /` through the bundled interpreter's network stack, and assert a marker string in the response.
4. Crash test: kill the server, confirm the supervisor reports `crashed -> restarting -> ready` and `GET /` succeeds again.
5. On Linux under xvfb, a GUI smoke test: the window loads the page and the DOM contains the marker (use whatever mechanism the existing GUI CI variant uses).

## 8. Out of scope

- Inferring dependencies from imports; frameworks beyond the three modes above (recipes only).
- Calling `@expose`d functions from the wrapped page (hybrid mode). Interesting later, but it needs IPC on a remote origin, which we deliberately avoid.
- Auth proxy, multi-window, system tray, auto-update.
- Any change to bridge-mode behaviour. If wrap work needs a change to shared code (supervisor, build), keep it backward compatible and covered by the existing tests.

## 9. When to stop and ask the owner

- The W0 kill criterion fires, or the matrix shows a failure that changes the scope.
- A decision in section 2 needs to change.
- You find the supervisor model cannot support a long-starting HTTP server without protocol changes beyond adding `ready`.
- Anything that changes names, licensing or public API.
