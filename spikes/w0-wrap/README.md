# W0 wrap spike

Throwaway validation for wrap mode (docs/wrap-mode-brief.md §4). Two
questions: do real Gradio/Streamlit/command apps behave acceptably in the
three native webviews, and does the existing supervisor model work for a
long-starting HTTP server?

## Layout

- `launcher.py` — hand-wired stand-in for `python -m pylair.wrap`. Speaks
  the supervisor frame protocol on a private fd (hello immediately,
  pongs, clean shutdown, non-zero exit on server death), redirects its
  own fd 1 to stderr for hygiene, picks a free loopback port, exports
  `PYLAIR_PORT`, substitutes `{port}` in the command, polls `GET /`
  until non-5xx, then emits `READY <url>` on stderr (the supervisor
  forwards stderr lines as events — no protocol change needed).
- `apps/fastapi_app.py` — command-mode sample (marker, upload, download,
  SSE stream, external link).
- `apps/gradio_app.py` — chat + file up/download + image (calls launch()
  directly; adapters are W3).
- `apps/streamlit_app.py` — widgets + uploader + download_button.
- `src-tauri/` — minimal Tauri app: window loads `public/index.html`
  ("starting"), spawns the launcher under the real supervisor, and calls
  `webview.navigate()` to the emitted `READY` url.

## Run

    uv venv .venv && uv pip install --python .venv/bin/python fastapi uvicorn python-multipart
    pnpm install
    pnpm dev:fastapi      # or dev:gradio / dev:streamlit with those deps

## Verified so far (macOS, arm64)

Deps pinned: **gradio 5.49.1, streamlit 1.51.0** (+ fastapi/uvicorn/
python-multipart) in the spike-local `.venv`.

- launcher standalone: hello frame on the wire fd (saved before the fd-1
  redirect), server logs + `READY <url>` on stderr, ready-probe pass.
- Hung-server proof (the W1 HTTP-probe rationale): server answers the
  first request then holds connections open forever — pongs still pass
  (they come from the launcher, not the server), but 3 failed health
  probes → launcher exits 5 for supervisor restart.
- `kill_tree`: whole process group reaped including a SIGTERM-ignoring
  grandchild (`test_kill_tree.py`) — escalation enumerates group
  members, not just the leader.
- Design finding: the server must share the launcher's process group
  (no setsid) or a supervisor-side killpg can't reach it.
- `os._exit` after kill_tree — daemon stdin reader holds the buffer lock
  at shutdown; sys.exit deadlocks (all exit paths after the reader
  thread starts use it).
- `cargo check` clean for the Tauri spike against the real plugin.
- IPC-leak probe wired into the fastapi page (page POSTs the Tauri
  surface it sees to /ipc-probe → stderr + /tmp/w0-ipc.json); real
  webview run still pending — see CHECKLIST.md.

## macOS real-webview runs (arm64, tauri dev)

All three apps launched under the real supervisor and navigated to their
loopback URL. Findings that matter beyond the spike:

- **Plugin bug found and fixed** (crates commit): `supervisor::spawn`
  used bare `tokio::spawn` inside plugin `setup`, which runs on the main
  thread with no reactor entered — every app panicked at launch
  ("there is no reactor running"). `spawn` now returns
  `(Handle, actor)` and the caller drives the future via
  `tauri::async_runtime`. Scaffolded apps would have hit the same panic.
- **Event payloads arrive JSON-quoted**: `emit(event, line)` serializes
  the String, so `Event::payload()` is `"READY ..."` with literal
  quotes. Strip or deserialize before matching (spike fix).
- **IPC probe — hard pass.** From the remote page:
  `__TAURI__` undefined; `__TAURI_INTERNALS__.invoke` exists but every
  attempt is REJECTED — `plugin:event|emit`: "not allowed on ... URL:
  http://127.0.0.1:PORT/ (allowed on: URL: local)"; `python_call`,
  `python_shutdown` and a bogus baseline: "pylair.X not allowed.
  Plugin not found". Remote origins get zero usable IPC; capabilities
  only ever scope to local URLs. Recorded in /tmp/w0-ipc.json.
- **kill -9 the server** → `launcher: server exited` → supervisor
  restart → new port → READY → window re-navigates. Full cycle seconds.
- **App quit (SIGTERM/quit event)** → stdin EOF → launcher kill_tree →
  zero survivors (verified fastapi + streamlit).
- **SSE** (fastapi `/stream`): EventSource connection held, ticks
  rendered.
- **Streamlit**: `_stcore` websocket transport live (6+ established
  conns), zero health-probe misses.
- **Gradio**: page loads, multi-connection session live. One
  health-probe miss during first paint (recovered, never reached 3) —
  W1 should probe a cheap path or tolerate a slow first minute.
- Launch→ready ≈ 1-3s per app after the one-time cargo build.

## Windows (win64, WebView2) runs

Branch `w0b-windows`. Deps identical to the macOS run. All automation
lives in `test_windows_containment.py` (tree kill under the real
supervisor) and `test_windows_apps.py` (per-app launch timing + IPC
probe). Everything below was run on a real Windows desktop session.

Drift fixed to run at all:

- venv interpreter path: `.venv\Scripts\python.exe` on Windows.
- Event scoping landed since the macOS run: the plugin now `emit_to`s
  stderr events only to SUBSCRIBED window labels. A wrap window never
  calls a bridge command, so it is never subscribed. The spike now
  subscribes "main" via the managed `Registry` and listens on the
  window object itself (`app.listen` globally receives nothing: emit_to
  only reaches listeners on the window target). Wrap mode's W1 design
  needs the same: subscribe host-side before relying on stderr events.
- `/tmp` paths -> `tempfile.gettempdir()`; optional `W0_READY_FILE` /
  `W0_PID_FILE` env hooks in the launcher for headless drivers.

Two real bugs found by running it:

- **`\r` poisons the READY URL.** stderr event payloads are JSON
  strings; on Windows the line ends `\r`, which arrives as a literal
  backslash-r. `trim_matches('"')` leaves it, and `url::Url::parse`
  treats `\` as `/` - the webview navigated to `/r` and got a 404 page.
  Fixed by `serde_json::from_str::<String>` on the payload. Invisible
  on macOS because lines end `\n`.
- **hung_server fixture lied on Windows.** `close()` on a socket with
  the client's request still unread is abortive (RST): the "ok" 200
  never reached the launcher's readiness probe. Fixed by recv()ing the
  request first, and by retaining accepted sockets (rebinding `conn`
  GC-closed each prior connection on every platform, so "held open"
  was actually "reset" everywhere).

Verified:

- launcher standalone: hello on the saved frame fd, `READY` on stderr,
  clean exit 0.
- Hung-server probe: READY, then health probes 1/3, 2/3, 3/3 at ~5s
  intervals, launcher exits 5 for supervisor restart.
- **Job Object tree kill** (`test_windows_containment.py`): kill the
  server PID alone and the orphan grandchild still dies - the
  launcher's own `taskkill /T` targets the already-dead server pid, so
  only the Job Object closing on monitor end accounts for the reap.
  Supervisor restarts (new launcher/server/orphan, second READY), then
  `taskkill /F` on the exe reaps the whole new tree via
  KILL_ON_JOB_CLOSE. PASS.
- `test_kill_tree.py` (the CI row): stdin EOF -> `taskkill /T` ->
  orphan grandchild dead, launcher exit 0. PASS.
- **Graceful close**: `taskkill /PID` without `/F` posts WM_CLOSE;
  app exits 0, stdin EOF -> launcher kill_tree -> 0 survivors.
- **Server kill -> recovery**: `taskkill /F` the server, supervisor
  restarts on a new port, `W0-NAVIGATE` fires, and the page's own
  EventSource re-requests `/stream` - the webview really reloaded.
- **IPC probe - hard pass, same as macOS.** On the remote page:
  `__TAURI__` undefined, `__TAURI_INTERNALS__.invoke` exists (WebView2
  injects internals on all origins) but every attempt is REJECTED:
  `plugin:event|emit`: "not allowed on ... URL: http://127.0.0.1:PORT/
  (allowed on: URL: local)"; `python_call`, `python_shutdown`, bogus:
  "pylair.X not allowed". Remote origins get zero usable IPC on
  WebView2 too.
- Launch -> READY: fastapi 1.8s, streamlit 5.0s, gradio 31.4s (import
  time; the window sits on "starting" that long - W1 needs a progress
  affordance or a slower-first-minute tolerance).

Not answered without hands (see WINDOWS-CHECKLIST.md): file-picker and
drag-drop upload, the download save-dialog/destination, external links,
clipboard - the interactive half of the kill criterion.

## Compatibility matrix

| check | fastapi mac | fastapi win | fastapi linux | gradio mac | gradio win | gradio linux | streamlit mac | streamlit win | streamlit linux |
|---|---|---|---|---|---|---|---|---|---|
| page load + WS/SSE stays connected | pass (SSE held) | pass (SSE held) | | pass (chat works) | | | pass (ws conns) | | |
| text input + long streamed output | pass (SSE ticks) | pass (SSE ticks) | | pass (chat echo) | | | pass (rerun) | | |
| file upload dialog + drag-drop | pass (native picker) | | | pass (native picker) | | | pass (native picker) | | |
| file download (save dialog, destination) | quirk¹ | | | quirk¹ | | | silent² | | |
| mic/camera permission prompts | n/a in sample | n/a in sample | | n/a in sample | | | n/a | | |
| clipboard copy/paste | pass | | | pass | | | pass | | |
| external link → system browser | fail³ | | | fail³ | | | fail³ | | |
| window close kills server tree | pass (0 survivors) | pass (graceful + hard kill, 0 survivors) | | pass | pass (hard kill, 0 survivors) | | pass (0 survivors) | pass (hard kill, 0 survivors) | |
| kill -9 → restarting page → recover | pass (re-nav, 3x) | pass (re-nav, SSE reconnects) | | pass (re-nav) | | | pass⁴ | | |
| IPC from remote page | pass (all REJECTED) | pass (all REJECTED) | | (same origin policy) | | | (same) | | |
| launch→ready time | ~1-3s | 1.8s | | ~2s | 31.4s | | ~2s | 5.0s | |
| installer size / memory | | | | | | | | | |

¹ download: file lands in ~/Downloads silently (no save dialog) AND the
  webview navigates to display the content — user stranded with no
  back/reload. W1: register a download handler + keep navigation on the
  app origin.
² streamlit download_button: downloads silently to ~/Downloads with ZERO
  feedback — no dialog, no toast. Mechanically fine, UX trap.
³ external link (target=_blank / link_button): silently swallowed —
  wry drops new-window requests without a registered
  new_window_req_handler. W1: handler → shell.open in system browser,
  plus navigation_handler blocking off-origin in-place nav.
⁴ streamlit kill -9: server killed → launcher respawned on a new port →
  the page simply reloaded, no error/reconnect UI shown. State is LOST:
  the filled name input came back empty — Streamlit sessions are
  in-memory, so every restart is a fresh session. Users see a page
  reload that silently forgets their work.
Also noted: a full-page navigation to an error/JSON response strands
the user — no back button, Cmd+R not wired.

Fill with pass / works-with-fix / fails + notes; results and the go/no-go
recommendation land in docs/decisions/0003-wrap-mode.md.
