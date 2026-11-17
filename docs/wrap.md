# Wrap mode

Wrap mode puts a Python web framework's own UI in the Pylair window:
Gradio, Streamlit, FastAPI, Flask, NiceGUI, Dash, Panel, or any other
framework that serves HTTP. The framework's server runs on `127.0.0.1`
on a random port, under the same supervisor that runs the bridge
runtime: it is restarted on crash, probed for hangs, and killed with
the app.

Where bridge mode wraps your Python *functions* in a Tauri window, wrap
mode wraps a *server*. Pick wrap mode when the app already has a UI in a
Python web framework; pick bridge mode when you are building a custom
front end over `@expose`d Python functions.

## Getting started in five minutes

### A new project

```sh
npx create-pylair my-app --mode wrap --local /path/to/pylair-checkout
cd my-app
npm install && uv sync
npx pylair dev
```

You get a FastAPI + uvicorn sample app (`app/main.py`, `framework =
"command"`), the window shell, and the wrap config in `pyproject.toml`.
`pylair dev` runs `tauri dev`: the window shows a local starting page
until the server answers, then navigates to it.

The `--local` flag is required for now: `create-pylair --mode wrap`
refuses without it because wrap mode is not in the published packages
yet. Once a release ships it, drop `--local` and the command works like
the bridge scaffold.

### An existing project

```sh
npx pylair wrap app.py          # or: pylair init --mode wrap app.py
npm install && uv sync
npx pylair dev
```

`pylair wrap` reads your dependency manifest (`pyproject.toml`, ideally
with `uv.lock`; `requirements.txt` is the warned fallback), guesses the
framework from it when it can (gradio, streamlit), writes
`[tool.pylair]` / `[tool.pylair.wrap]` into `pyproject.toml`, and adds
the window shell, never overwriting a file that exists. A bare
`app.py` with no manifest stops with the reason and offers `uv init`;
Pylair never guesses dependencies from imports. The same `--local`
gate applies, since it runs `create-pylair --mode wrap` under the hood.

```text
pylair wrap [entry.py] [--framework command|gradio|streamlit]
             [--cmd <argv...>] [--source <dir>] [--dir <path>] [--local <repo>]
```

Everything after `--cmd` is the server command verbatim. `entry` and
`--cmd` pick the framework for you (`--framework` overrides); `--source`
defaults to the directory the entry lives in, else `app`.

### Verify and ship

```sh
npx pylair doctor            # config, manifest, framework import, shell, no remote IPC
npx pylair build             # native installer with bundled Python
npx pylair build --stage-only   # stage and verify only, no tauri build
```

`pylair build` stages `source` into `resources/app/`, prunes the bundled
environment, then proves it can still work: it imports `pylair.wrap` and
your framework (`gradio`, `streamlit`, or the `-m` module of a command)
with the *bundled* interpreter. An over-pruned bundle fails on your
machine, not a user's. `--stage-only` stops before `tauri build`.
`pylair doctor` checks the config, the manifest, that the framework
imports in the project venv (and the bundled one after a build), the
window shell files, `"create": false` on the main window, and that no
capability grants a remote url. `docs/build.md` has the pipeline.

## The window

- Until the server answers on its port, the window shows a local
  starting page (`ui/index.html`). When the launcher reports `ready`,
  it navigates to the server's URL. If the server dies, it shows a
  local restarting page (`ui/restarting.html`), and navigates back on
  the next `ready` (a restart comes with a new random port).
- A **Navigate** menu carries **Back**, **Reload** and **Home** with
  keyboard shortcuts, so a full-page navigation can never strand you.
- The wrapped page is sent none of the runtime's lifecycle events
  (`pylair://state`, `pylair://stderr`).

## Downloads, links and navigation

- **Downloads** show a native Save As dialog by default
  (`downloads = "ask"`), or go straight into the user's Downloads
  folder (`downloads = "downloads"`, never overwriting an existing
  file). The suggested name is reduced to its last path component, and
  a small note in the page says where the file went. Cancelling the
  dialog cancels the download.
- **In-window navigation** is locked to the app origin: the exact
  `http://127.0.0.1:<port>` origin the launcher reported for the
  current server generation. The local pages are always allowed.
- **External links** (`target=_blank`, `window.open`, a framework's
  link button) never open a second window: an app-origin link opens in
  place; `http`, `https` and `mailto` elsewhere open in the system
  browser; every other scheme is refused.

## Config reference

```toml
[tool.pylair]
mode = "wrap"
source = "app"            # the directory the server runs in; staged as resources/app/
requirements = "requirements.txt"
python-version = "3.12"
# prune = ["extra/deny-glob"]

[tool.pylair.wrap]
framework = "command"     # required: "command" | "gradio" | "streamlit"
cmd = ["python", "-m", "uvicorn", "main:app", "--host", "127.0.0.1", "--port", "{port}"]
# entry = "app.py"        # required for gradio/streamlit, forbidden for command
# ready-path = "/"        # optional; defaults per framework (below)
# ready-timeout = 120     # seconds, 1..3600, default 120
# downloads = "ask"       # "ask" = Save As dialog | "downloads" = Downloads folder
```

Validation is strict: unknown keys, the wrong entry/cmd pairing, a
`ready-path` that does not start with `/`, or an out-of-range
`ready-timeout` all fail with a message that names the key. A `cmd`
with no `{port}` or that binds `0.0.0.0` passes but produces a
`pylair doctor` / `dev` / `build` warning.

`{port}` inside `cmd` is replaced by the port the launcher picked, and
`PYLAIR_PORT` is exported to the server either way. When the framework
takes its port in Python code (NiceGUI, Dash), read `PYLAIR_PORT` from
the environment instead.

## The launcher

`pylair dev` and `pylair build` generate this command line from the
config; you only need it directly for debugging or an unusual harness:

```text
python -m pylair.wrap
    [--framework {command|gradio|streamlit}]
    [--entry app.py | --cmd <argv...>]
    [--ready-path P]
    [--probe-interval S] [--probe-timeout S] [--max-misses N]
```

- `--framework command` is the default and runs `--cmd`; bare `--cmd`
  with no `--framework` is the same thing. The two adapter frameworks
  take `--entry` and reject `--cmd`.
- `{port}` inside `--cmd` is replaced with the port the launcher chose,
  and `PYLAIR_PORT` is exported to the server either way. A bare
  `python` in the command resolves to the launcher's own interpreter,
  which in a packaged app is the bundled one.
- The launcher waits for TCP, then a non-5xx answer on the ready path
  (default per framework below), reports `ready {url}`, then probes
  every 10 s and exits after 3 consecutive misses so the supervisor
  restarts it.
- Exit codes: `2` bad usage, `3` start failure (no free port or spawn
  failed), `4` the server exited, `5` unhealthy after probe misses.

Default ready paths: `command` `/`, `gradio` `/config`, `streamlit`
`/_stcore/health`. These are cheap endpoints that answer without a page
session, so a slow first render is never a probe miss; `ready-path`
overrides them when you need something else.

## Recipes

Every recipe binds `127.0.0.1`. A command-mode server that binds
`0.0.0.0` is reachable from the network: the launcher cannot force the
host, so the bind is the recipe's job and yours.

### Gradio

```toml
[tool.pylair.wrap]
framework = "gradio"
entry = "app.py"
```

The adapter runs your entry with `runpy` after patching
`gradio.Blocks.launch` so `server_name=127.0.0.1`,
`server_port=$PYLAIR_PORT`, `inbrowser=False`, `share=False` and a
blocking launch win over whatever the script passes. `Interface` and
`ChatInterface` inherit `Blocks.launch`, so one patch covers all three.
A script that defines `demo` without calling `launch()` is launched by
the adapter. `GRADIO_ANALYTICS_ENABLED=False` is set before the import.

### Streamlit

```toml
[tool.pylair.wrap]
framework = "streamlit"
entry = "app.py"
```

The adapter runs `streamlit run` with `--server.address 127.0.0.1`,
`--server.port $PYLAIR_PORT`, `--server.headless true`,
`--browser.gatherUsageStats false`, `--server.fileWatcherType none`
and `--server.scriptHealthCheckEnabled true`, plus the usage-stats
environment flag. Multipage apps work unchanged: Streamlit discovers
`pages/` next to the entry.

### FastAPI (command)

```toml
framework = "command"
cmd = ["python", "-m", "uvicorn", "app:app", "--host", "127.0.0.1", "--port", "{port}"]
```

### Flask (command)

```toml
framework = "command"
cmd = ["python", "-m", "flask", "--app", "app", "run", "--host", "127.0.0.1", "--port", "{port}"]
```

### NiceGUI (command)

NiceGUI takes the port in code, so read it from the environment:

```python
import os
from nicegui import ui

ui.run(host="127.0.0.1", port=int(os.environ["PYLAIR_PORT"]))
```

```toml
framework = "command"
cmd = ["python", "app.py"]
```

### Dash (command)

Same pattern:

```python
import os

app.run(host="127.0.0.1", port=int(os.environ["PYLAIR_PORT"]))
```

```toml
framework = "command"
cmd = ["python", "app.py"]
```

### Panel (command)

```toml
framework = "command"
cmd = ["python", "-m", "panel", "serve", "app.py", "--address", "127.0.0.1", "--port", "{port}"]
```

## Threat model

The short version; `docs/security.md` section 9 carries the honest
status table.

- The wrapped server listens on `127.0.0.1` on a random port. Other
  local processes, and web pages open in the user's own browser, can
  reach it. That is the same exposure as `streamlit run` or `gradio` on
  a developer machine, and we do not claim more.
- The wrapped page is a remote origin and gets zero Tauri IPC: every
  `plugin:*` invoke from it is rejected, and `pylair doctor` fails a
  capability that grants a remote url. A compromised page cannot reach
  the filesystem or shell through Tauri, and it is sent none of the
  runtime's lifecycle events.
- The Gradio and Streamlit adapters force `share=False` and disable
  framework telemetry. `command` mode runs whatever you give it; the
  launcher cannot see inside it.
- The port is picked by binding `127.0.0.1:0` and closing it, so a
  local process can in principle claim it first. The server then exits,
  the launcher exits non-zero, and the restart picks a new port.
- A token-checking reverse proxy is documented future hardening, and
  alone it would not be enough, since the app's own port stays open.

## Known limitations

- **Streamlit sessions do not survive a restart.** Sessions are
  in-memory: when the server restarts, the page reloads with no error
  or reconnect UI and filled inputs come back empty. This is inherent
  to Streamlit.
- **A command-mode server that binds `0.0.0.0` is reachable from the
  network.** The launcher picks a loopback port but cannot force the
  host; every recipe above binds `127.0.0.1`, and yours must too.
- **`http.server`-based servers can stall at startup on some macOS
  machines.** A hostname lookup between bind and listen can take
  seconds; the launcher logs that it is still waiting and the ready
  timeout covers it.
- **Gradio `launch()` with positional arguments conflicts with the
  forced settings.** The patch forces `server_name`, `server_port`,
  `inbrowser`, `share` and `prevent_thread_lock` as keywords; a script
  passing them positionally raises `TypeError: got multiple values`.
  Call `launch()` with keywords or none.
- **Supported majors are gradio 5.x and streamlit 1.x.** Other majors
  log a warning and still run; a missing framework is a clean error,
  not a traceback.
- **`create-pylair --mode wrap` needs `--local` until the first release
  that ships wrap mode** (it ships with 0.1.0); `pylair wrap` inherits
  the same gate because it runs the scaffolder.
