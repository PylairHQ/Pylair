# 0003: Wrap mode — go/no-go after the W0 spike

Decided 2026-11-11. Spike: `spikes/w0-wrap` (launcher + matrix in its
README; manual checklist in CHECKLIST.md).

## Question

Can Pylair support **wrap mode** — displaying an existing Python web UI
(FastAPI, Gradio, Streamlit) in a stock Tauri webview, served on
127.0.0.1 by a supervised launcher — without forking the framework or
the webview?

## Evidence (macOS, all three frameworks)

Everything below was observed live on macOS; the full matrix is in
`spikes/w0-wrap/README.md`.

**Works with no product code:**

- Page load + streaming transports: FastAPI SSE, Gradio queue/websocket,
  Streamlit `_stcore` websocket — all held and interactive.
- Native file-picker uploads through the webview on all three.
- Clipboard copy/paste.
- Launch→ready ≈ 1–3 s per app.
- kill -9 on the server → launcher respawns on a new port → READY →
  webview re-navigates. Verified on all three frameworks.
- App quit reaps the whole server tree (0 survivors) on all three.
- **IPC isolation is a hard pass.** From the remote page `__TAURI__` is
  undefined; `__TAURI_INTERNALS__.invoke` exists but every call —
  `plugin:event|emit`, `plugin:pylair|python_call`,
  `plugin:pylair|python_shutdown`, and a bogus baseline — is rejected.
  Capabilities only ever scope to `URL: local`; remote origins get zero
  usable IPC.

**Known-fixable gaps (all wry handler registrations, not platform
limits):**

- `target="_blank"` / `link_button` external links are silently
  swallowed — no `new_window_req_handler` is registered.
- Downloads land in `~/Downloads` silently with no dialog; FastAPI/Gradio
  additionally *navigate* to display the content, stranding the user
  (no Back, no wired Cmd+R). Streamlit's `download_button` gives zero
  feedback at all.
- Any full-page navigation to an error/JSON response strands the user —
  no navigation recovery exists in a bare webview.
- Gradio had one transient health-probe miss during first paint — the
  launcher's `GET /` probe is too heavy during hydration.

**Framework behavior worth documenting:**

- Streamlit kill -9 recovery: the page simply reloads with **no error
  or reconnect UI** — and the session is gone. Filled inputs come back
  empty; Streamlit sessions are in-memory. After a restart users see a
  page reload that silently forgets their work. This is inherent to
  Streamlit, not a defect we can fix.

## Decision

**Go.** Wrap mode ships as a supported configuration.

Every "does it work at all" question came back yes on macOS, the
security boundary (remote content cannot reach plugin IPC) held
without any special-casing, and startup is 1–3 s. The gaps are all in
wry handler registrations we must add in W1 — none are platform or
framework limits.

## UX decisions for W1

1. **Downloads:** register `download_requested` and show a native
   **Save As dialog by default**, with a config option to save straight
   to `~/Downloads`. A silent save with no feedback is the worst option
   (Streamlit demonstrates it). After saving, reveal the file or show a
   notification.
2. **External links:** register `new_window_req_handler` → open in the
   **system browser**. Register `navigation_handler` to **lock in-place
   navigation to the app origin** (exact loopback origin match — see
   P1-8's structured READY message and origin validation).
3. **Navigation recovery:** add **Back, Reload and Home** to the window
   menu with keyboard shortcuts, so a full-page navigation can never
   strand the user on a dead page.

## W1 implementation notes carried forward

- Health probe: do **not** treat a slow `GET /` during hydration as
  unhealthy. Use TCP liveness or a framework-specific cheap path
  (Streamlit: `/_stcore/health`), apply a grace period after startup,
  and only kill on several consecutive failures.
- Readiness/navigation: replace stderr-scraped readiness with a
  structured `READY` message carrying the URL; validate the exact
  loopback origin before navigating (P1-8).
- The IPC probe becomes a permanent check in the acceptance job so a
  future capability change that opens remote IPC fails CI.

## W2 implementation notes

The three UX decisions above landed in W2 (plugin `wrap` module, the wrap
template, `pylair wrap`), with these findings from building them:

- **The window is built from `RunEvent::Ready`, off the main thread.** A
  window built in plugin `setup` blocks on an acknowledgement from the event
  loop, which has not started: the app hangs before showing anything (found
  by running it, not by tests). The template declares the window with
  `"create": false` so the plugin can build it with the download and
  new-window hooks, which only a window builder carries.
- **Navigation lock = plugin `on_navigation` + window `on_new_window`.** A
  `file:` url never reaches the handler at all: the webview blocks a web page
  from loading one. A custom scheme does, in place and as a new window, so
  the self-test uses that.
- **Feedback after a download is a small note in the page**, not a native
  notification (which would add a plugin dependency) or a modal dialog.
- **Back, Reload and Home** are a "Navigate" menu on top of Tauri's default
  menu (so Copy and Paste still work on macOS), with Alt+Left / Cmd+[,
  Ctrl/Cmd+R, Alt+Home / Cmd+Shift+H. Pressing Back from the app to the
  starting page returns to the app at once, so history cannot trap the user
  on a dead page.
- **`pylair://state` and `pylair://stderr` are not delivered while the
  wrapped page is showing**; a local page that loads late is replayed the
  last state and the server's last 200 log lines.
- **The ready url is checked for control characters and backslashes**, not
  only parsed: the W0 Windows `\r` bug (a JSON-escaped CR navigating to
  `/r`) is now refused at the source instead of normalised.

## W3 implementation notes

- Health paths are per-framework, picked so a slow first-page hydration
  is never a probe miss. Gradio: `GET /config`, the small JSON blob the
  app builds once at launch (no session, no template render); `GET /`
  on gradio can stall during hydration, which is exactly what W0 warned
  about. Streamlit: `GET /_stcore/health`, the runtime's own cheap
  liveness handler.
- Gradio `share` cannot be disabled by config alone, so the adapter
  patches `Blocks.launch`: `share=False`, `server_name=127.0.0.1`,
  `server_port=$PYLAIR_PORT`, `inbrowser=False` and blocking win over
  whatever the entry passes. `Interface`/`ChatInterface` inherit
  `Blocks.launch`, so one patch covers all three.
- Telemetry: gradio reads `GRADIO_ANALYTICS_ENABLED` at import time, so
  the adapter sets `False` before importing. Streamlit gets both the
  `--browser.gatherUsageStats false` flag and the
  `STREAMLIT_BROWSER_GATHER_USAGE_STATS=false` env.
- Streamlit's `--server.scriptHealthCheckEnabled true` exposes
  `/_stcore/script-health-check`, a GET that runs the entry once in a
  throwaway session. It exists so tests (and later `doctor`) can prove
  the app script itself runs; it re-runs the script per request, so it
  is deliberately not the steady-state health probe.
- Supported majors are pinned and documented: gradio 5.x, streamlit 1.x
  (`pylair.wrap._shared.SUPPORTED_MAJORS`, mirrored by the `wrap` test
  dependency group). Anything else warns and still runs; a missing
  framework is a clean error, not a traceback.

Post-release fix (rc.21 hand check): Tauri's native drag-drop handler is
disabled on the wrap window (`wrap_window_config` sets
`drag_drop_enabled = false` in `install_window`). The handler's only job
is emitting `DragDropEvent`s over IPC, which a wrapped page cannot use,
and on Windows it replaces WebView2's handler so an HTML5 drop never
reached the page. A config flag, not a builder call, so the window unit
test can pin it. Bridge windows keep the handler: bridge pages use IPC.

## Gates

Done-gates for *declaring wrap mode done* (not for starting W1):

- Windows and Linux interactive rows in the matrix (VM or CI-driven).
- Health-probe tuning above, verified against a slow-hydrating app.

Not gates: mic/camera prompts (no fixture exposes them yet — test when
a real app needs it) and the session-loss caveat (documented, inherent).
