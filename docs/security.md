# Security model

This document describes the security boundaries of Pylair as implemented
(`crates/tauri-plugin-pylair`, `python/pylair`, `packages/client`), and —
just as important — what it does **not** claim. Where a control is planned
but not shipped yet, it says so.

## 1. Threat model

Three trust domains, separated by real process boundaries:

```
webview (least trusted)  →  Tauri IPC  →  Rust host (the TCB)
                                           ↓ stdio frames
                                      Python runtime (semi-trusted)
                                           ↓ subprocess
                                      isolated workers (per-call)
```

- **The webview is the least-trusted layer.** We assume the JavaScript
  running in it may be hostile — an XSS payload, a compromised frontend
  dependency, or content the app didn't intend to load. Everything the
  webview can do must survive that assumption.
- **The Python runtime is semi-trusted.** It is the developer's own code,
  but it runs third-party dependencies and processes untrusted input that
  arrived through exposed functions. A buggy or compromised runtime must
  not be able to hang, corrupt, or exhaust the host — it can still do
  anything *its own process privileges* allow (see §8).
- **The Rust host is the trusted computing base.** It enforces
  capabilities, owns the wire protocol, bounds every resource, and
  supervises the child. Its own attack surface is the frame channel, so
  the channel is designed so nothing can corrupt it (§5).

A second machine, or other processes on the user's machine, are out of
scope except where noted (wrap mode's loopback server, §9).

## 2. What the webview can reach — and nothing else

The plugin's entire IPC surface is nine commands:

| command | what it does | default permission |
|---|---|---|
| `python_call` | invoke an exposed function, await result | `pylair:default` |
| `python_call_stream` | invoke a streaming function | `pylair:default` |
| `python_grant_credit` | grant stream credit to an owned task | `pylair:default` |
| `python_cancel` | cancel an owned task | `pylair:default` |
| `python_state` | read lifecycle state | `pylair:default` |
| `python_retry` | respawn a `failed` runtime | opt-in only |
| `python_shutdown` | stop the runtime | opt-in only |
| `python_selftest_report` | end the installed-webview self-test | template grants; inert w/o env flag |
| `python_selftest_b_report` | relay the two-window self-test verdict | opt-in only; no shipped capability |

There are **no** fs, env, shell, exec, or network commands in the plugin.
The only way from the webview into Python is `call`/`call_stream`, and the
only functions reachable that way are ones the developer marked `@expose`
(§3). `python_selftest_report` is additionally inert unless the host
itself was launched with `PYLAIR_SELF_TEST_WEBVIEW`, so granting its
permission by accident opens nothing.

`pylair:default` deliberately excludes `python_retry` and
`python_shutdown` — supervisor control is a power an app opts into with
`pylair:allow-python-retry` / `pylair:allow-python-shutdown`, not part of
the everyday set. `python_selftest_b_report` is likewise opt-in only: the
capability that grants it (`selftest-winb.json`) is carried by the
reference app and injected into scaffolded apps by CI — it is not part of
the shipped template.

## 3. The `@expose` allowlist — no arbitrary execution

The callable allowlist lives in Python (`_registry`), populated only by
the `@expose` decorator at import time. On a `call`, the runtime:

1. looks up `fn` in the registry — anything else fails with
   `FUNCTION_NOT_FOUND` (`-32601`);
2. requires `input` to be a JSON object;
3. validates each argument against the function's signature before
   dispatch (`_json.coerce_kwargs`). Annotated parameters are checked
   strictly — a wrong primitive (`add(a: int, b: int)` called with
   `{"a": "x"}`), a missing required key, or an unexpected extra key is
   `INVALID_PARAMS` (`-32602`), a caller error: the function body never
   runs, on every call mode including isolated workers. Structured
   annotations (dataclasses, containers, `Enum`, `bytes`, `datetime`)
   are still reconstructed by coercion; a reconstruction failure is
   `INVALID_PARAMS` too. Unannotated parameters and annotations with no
   wire mapping pass through unchecked — validation is an input
   contract for typed functions, not a substitute for the allow-list.

There is no eval/exec/getattr-by-wire path anywhere. Even the isolated
worker — which resolves its target via `getattr(module, fn)` — receives a
`fn_name` that already passed the registry check, and a module name the
host set via `PYLAIR_APP_MODULE`, not a wire value.

**Honest corollary:** `@expose` *is* the attack surface. An exposed
function runs with the Python process's full privileges, and its `input`
is JSON supplied by a possibly-hostile webview. The boundary is the
exposed-function allowlist plus Tauri capabilities — **not type
checking**: coercion rebuilds composite shapes but validates neither
primitives nor intent, so exposed functions should validate their inputs
the way a server-side endpoint would. If you expose a function that runs
user-chosen code, you have re-introduced arbitrary execution yourself;
the framework can't prevent that and doesn't try.

## 4. Tauri capabilities gate every command

Pylair is a Tauri plugin and uses Tauri's capability/permission model
unchanged:

- A webview can invoke `python_call` only if a capability grants it
  `pylair:allow-python-call` (directly or via `pylair:default`). The
  scaffolded template's capability covers `windows: ["main"]` only —
  its permissions are `core:default`, `pylair:default`, and
  `pylair:allow-python-selftest-report` (inert, §2).
- **Remote origins get no IPC.** A capability never covers remote URLs
  unless it explicitly adds `remote.urls` — verified in the wrap-mode
  spike: from a remote page `window.__TAURI__` is undefined and every
  `__TAURI_INTERNALS__.invoke` call (pylair commands included) is
  rejected. The decision record makes this probe a permanent
  acceptance-job check, so a future capability change that opens remote
  IPC fails CI.

### Restricting functions per window

By default a capability that grants `pylair:default` lets every window it
covers call every `@expose`d function. To narrow that, put a **command
scope** on the permission. Both call commands, `python_call` and
`python_call_stream`, read the scope and check the function name before
anything else happens (before the window is subscribed, a task id is
allocated, credit is taken or a channel is written to), so a denied call
costs nothing and Python never sees it. The wire protocol between the host
and Python is unchanged; this is a host-side check.

Each scope entry is `{ "name": "..." }` and nothing else (an unknown key
makes the entry malformed, and a malformed entry **fails closed**: every
call it covers is denied rather than the entry being ignored). A `name` is
either an exact function name or a prefix with a single trailing `*`
(`"admin_*"`); a `*` anywhere else is an ordinary character.

Hide a few functions from a window, keeping everything else (recommended
shape, one entry on the set covers both call commands):

```json
{
  "identifier": "main",
  "windows": ["main"],
  "permissions": [
    "core:default",
    { "identifier": "pylair:default", "deny": [{ "name": "admin_*" }] }
  ]
}
```

Give a window only a short list of functions (scope **both** commands the
same way, since a stream call goes through `python_call_stream`):

```json
{
  "identifier": "reports",
  "windows": ["reports"],
  "permissions": [
    { "identifier": "pylair:allow-python-call",        "allow": [{ "name": "report_*" }] },
    { "identifier": "pylair:allow-python-call-stream", "allow": [{ "name": "report_*" }] },
    "pylair:allow-python-state"
  ]
}
```

The decision, applied per call to the entries that cover the calling
window:

1. no entries at all: the call is allowed;
2. any `deny` entry matches: denied, always, whatever else matches;
3. otherwise, if there is any `allow` entry, one must match or the call is
   denied;
4. otherwise (only `deny` entries, none matched): allowed.

**Allow entries narrow, they do not add.** Tauri unions the scope entries
of every capability that covers a window. So a capability that grants
`pylair:default` unscoped to a window and a second capability that gives
the same window `allow: [{ "name": "report_*" }]` do not combine into
"everything": the one allow entry makes it "only `report_*`". Likewise
two capabilities on one window each listing allow entries union their
lists, and deny entries from any of them apply. Two windows are judged
independently, each by its own covering capabilities. A window no
capability covers has no access at all (Tauri's ACL rejects the command
before the plugin runs).

**The `allow: []` trap.** An empty allow list is the same as no entries,
so `{ "identifier": "pylair:default", "allow": [] }` permits **every**
function, the opposite of what it looks like. To withhold calls from a
window, do not grant the permission; to restrict, list names.
`pylair build` and `pylair doctor` reject this configuration. They also
reject a window that scopes `python-call` but not `python-call-stream`
(or the reverse), since the unscoped command is a side door, and a
malformed entry; they warn on a pattern that matches no exposed function
(`pylair build` only, because that needs the app's function list; if it
cannot be loaded the check is skipped with a one-line note) and on a
window whose two commands carry different scopes.

A denied call returns `PERMISSION_DENIED` (`-32050`, protocol §8) with
`data: {"fn", "window"}`; `@pylair/client` raises `PermissionDeniedError`
with `.fn` and `.window`. No runtime warning is logged for a pattern that
matches nothing; that is the CLI's job.

**What this does not hide.** Function *names* are still visible to every
window through the `pylair://exposes` event (and the generated stubs). The
scope stops calls, not discovery; do not put secrets in function names.
The scaffolded template's capability stays open (no scope) on purpose;
add scopes when you add windows or load less-trusted content.

### What stays capability-level, and what is owner-scoped

Finer-grained rules than the per-function allow/deny above (per-argument,
per-tag) are not implemented; see `docs/decisions/0004-per-function-permissions.md`.

**Task control is owner-scoped.** The supervisor records
`{task → call id, owner webview}` per runtime generation, where the owner
is the Tauri-reported window label — never a JS-supplied value:

- `cancel`/`grant_credit` from a non-owner webview → rejected;
- a task id from a dead runtime generation → `not_found` (it can never
  control a new task);
- destroying a window cancels all of its tasks.

**Stream/result delivery is bound to the invoking webview** — it is not
an event at all. `python_call_stream` requires an `onEvent`
`tauri::ipc::Channel` argument; the plugin sends every chunk and the
terminal through it, and `pylair://stream` no longer exists. A second
webview cannot listen to a channel it was never handed, and there is no
bus left to inject forged events into — so the leak and the forgeries
documented in `docs/design/event-scoping.md` are both closed by
construction, not by a permission stopgap. This is enforced by the
installed two-window self-test (`PYLAIR_SELF_TEST_WEBVIEW`): a `win-b`
holding `emit`/`emitTo` sees none of `main`'s call data, never gets a
forged item accepted, and never wins a cancel/grant on `main`'s tasks.

The invoke answers `{task, protocol: 2}` so a mixed-version deploy fails
loudly instead of hanging: an **old client + new plugin** gets Tauri's
`invalid args 'onEvent'` rejection; a **new client + old plugin** gets a
bare task id back (the old command ignored `onEvent`), which the client
recognises, reports as an upgrade-required error, and cleans up by
cancelling the orphaned task.

What remains broadcast: `pylair://state` and `pylair://exposes` go to
webviews that have invoked the bridge (`emit_to` per label) — but any
`Any`-target listener still receives them regardless of the emit
target (Tauri 2.12.5's `match_any_or_filter`), so treat these events
as readable by every webview in the app. They carry lifecycle data,
not call results.

`pylair://stderr` is narrower: stderr carries application log lines a
second window has no business reading, so it is **opt-in per window**
via the plugin builder — `Builder::stderr_delivery(StderrDelivery)`
defaults to `Only(["main"])`, an explicit label list intersects it with
subscribed windows, and `StderrDelivery::All` restores broadcast for
apps that want every window to see logs. Two honest boundaries:
label-scoped listeners are *not* delivered stderr outside the
allow-list, but a deliberate `Any`-target `listen` on
`pylair://stderr` still receives whatever is emitted — the same
event-bus residual as state/exposes, not a sniffing boundary; and a
listed label that never invoked the bridge receives nothing. The
client's `listen` is webview-scoped, so a stock `@pylair/client`
window honours the allow-list — multi-window apps that show the log
in a non-`main` window must pass its label to `stderr_delivery`. The
template
still **denies** `core:event:deny-emit` and `core:event:deny-emit-to`.
Deny rules are evaluated per *origin*, ignoring `windows:` scoping, and
always win over allows — so the entries bar **every** local webview from
`emit`/`emitTo`, not just `main`. A scaffolded webview cannot forge the
remaining broadcast events; an app needing JS-side `emit` cannot get it
back through capabilities while the deny stays — the only paths left
are Rust-side `emit_to` calls. That is also why the self-test's `win-b`
reports through `python_selftest_b_report` (an ACL-checked command that
emits for it) instead of `emitTo` — and why `selftest-winb.json` is not
part of the shipped template: it grants `emit`/`emitTo` to a `win-b`
label and references a permission a freshly scaffolded app may not
resolve. The reference app carries it; CI injects it into scaffolded
apps before building them.

Scope boundaries, stated plainly: ownership checks are the boundary for
`cancel`/`grant_credit`/task reap-on-destroy; capability grants are the
boundary for *starting* work — any webview whose capability covers the
commands can still invoke them and start calls or streams. Channel
delivery does not limit who may invoke; it guarantees only that a
call's data reaches the webview that invoked it.

Reloads are a webview-lifecycle edge the channel cannot see: a fresh
document replaces the JS context but `Channel::send` can keep returning
`Ok` through the engine's eval path — payloads land in a dead context
and nothing calls back. The plugin's `on_page_load` hook closes that
gap server-side: at `PageLoadEvent::Started` — a document-commit event
on every engine (WebView2 `ContentLoading`, WKWebView
`didCommitNavigation`, WebKitGTK `LoadCommitted`), which same-document
navigations like anchors and `history.pushState` never fire — every
task the old document owned is cancelled, bounded by a task-id
watermark so a call the new document raced in before the reap lands is
spared. The client's `pagehide` hook remains as a best-effort belt.

## 5. Malformed input, in both directions

The frame channel is a private fd pair the host wired at spawn — nothing
else holds the write end, so "messages" can only come from the peer, and
the only attacks are malformed frames and resource exhaustion.

**Runtime → host.** Any byte that isn't a well-formed frame (oversized
length prefix, bad JSON, wrong shape) is a protocol violation: all
in-flight calls fail with `PROTOCOL_VIOLATION` (`-32032`), the process is
killed and reaped, and normal crash/restart accounting applies — a peer
that can't hold the protocol is treated as buggy-or-hostile, not
negotiated with. The handshake is stricter still: the first frame must be
`hello` within 15 s, and a `protocol` version mismatch goes straight to
`failed` with no restart (retrying can't fix version skew).

**Host → runtime.** A corrupt host isn't the threat model, but a corrupt
channel can't wedge the runtime either: bad JSON → `PARSE_ERROR`;
non-object or wrong-shaped messages → `INVALID_REQUEST`; unknown methods
→ `FUNCTION_NOT_FOUND`; bad call inputs → `INVALID_PARAMS`. A length
prefix over `MAX_FRAME` (default 16 MiB) makes the runtime treat the host
as gone — it cancels tasks and exits rather than allocate on demand.
Dispatch itself cannot die: any handler exception becomes an `INTERNAL`
error for that one call.

**stdout hygiene** keeps the channel honest at the source. Before any
third-party import runs, the runtime dups real stdout to a private fd
(the frame channel) and points fd 1 at stderr — so `print()`, C-extension
writes, and library logging land in the log stream and can never corrupt
a frame. The host drains stderr unconditionally and drops oldest lines
with a `[dropped N lines]` marker under backpressure, so a flooding
runtime can't deadlock on a full pipe either. Both sides use binary mode
on Windows so CRLF translation can't corrupt length prefixes.

## 6. Bounded everything

A hostile or buggy peer must not be able to hang or OOM the other side.
Every queue, buffer, and counter is bounded (`Limits` on
`SupervisorConfig`, env-overridable via `PYLAIR_MAX_*`, logged at
startup): concurrent tasks and streams, queued commands, per-stream and
total buffered stream bytes, outstanding stream credit, isolated workers,
and frame size. A hit fails the offending call/stream with
`LIMIT_EXCEEDED` (`-32040`, `data.limit` names the cap) — one failure,
bounded and explicit, never a silent stall.

Runtime-side mirrors: an inbound bound applies backpressure through the
OS pipe; the outbound queue is frame-and-byte bounded, and when it fills
the runtime concludes the host is gone, drains, and exits — callers see
`RUNTIME_CRASHED`, not a hang.

Two more bounded paths matter:

- **Control frames have their own lane.** `cancel`, `stream.credit`, and
  `shutdown` drain ahead of data frames, so a stream flood cannot starve
  or silently drop control.
- **Liveness is enforced, not assumed.** The host pings every 5 s; pongs
  are emitted by the runtime's reader thread (not the event loop), so a
  missed pong means the *process* is truly hung. Three consecutive misses
  → killed as `crashed`. Event-loop stalls are reported separately via
  `loop_stale_ms` as a diagnostic; `loop_stall_kill` (off by default)
  makes a long stall fatal for apps that prefer it.

Known gap: limits are per-runtime, not per-window — a webview that floods
the bridge can starve other windows of the same runtime.

## 7. Process boundaries and orphan prevention

"No child outlives the host" is a mechanism, not a hope (protocol §6):

- **stdin EOF is the portable guarantee** — a dead host closes the pipe,
  the runtime drains and exits, killing its isolated workers first.
- **Windows:** the runtime sits in a Job Object with
  `KILL_ON_JOB_CLOSE`; the kernel kills the whole tree when the last
  handle closes. The runtime is created suspended and resumed only after
  it is in the job, so nothing it spawns can start outside it. Residual:
  if the host itself dies in the microseconds between spawn and
  assignment, the still-suspended runtime cannot see stdin EOF and is
  left behind as an inert process. Fallback if the job cannot be
  created: `taskkill /T`. Details:
  [design/windows-job-object-race.md](design/windows-job-object-race.md).
- **Linux:** process-group kill plus `PR_SET_PDEATHSIG` on both the
  runtime and each worker — the kernel kills them even if the host dies
  before any cleanup runs.
- **macOS:** no parent-death signal exists. A *healthy* runtime exits on
  stdin EOF and isolated workers carry their own parent watcher, but a
  runtime wedged in native code can survive an abrupt host kill — the
  documented gap (README → Known limitations).

Isolated workers (`@expose(isolated=True)`) are per-call subprocesses in
their own process group/session so exactly one task's tree can be
killed on cancel. The group kill reaches SIGTERM-ignoring grandchildren
but not descendants that deliberately detach (`setsid`, double-fork) —
documented, accepted.

## 8. What the runtime process itself can do

Pylair does **not** sandbox Python. The runtime — and anything it imports
— runs with the app's normal user privileges and can read the filesystem,
environment, and network. The security claims are about the *edges*:

- the webview can't reach Python except through `@expose`d functions;
- the host can't be corrupted, hung, or exhausted by the runtime;
- no process outlives the host.

Environment hygiene narrows the edge further: packaged spawns scrub the
`PYTHON*` config vars (`PYTHONHOME`, `PYTHONSTARTUP`, `PYTHONEXECUTABLE`,
`PYTHONUSERBASE`, `PYTHONINSPECT`, `PYTHONPLATLIBDIR`, `PYTHONWARNINGS`,
`PYTHONBREAKPOINT`) and set `PYTHONNOUSERSITE`/`PYTHONUTF8`, so the
bundled interpreter can't be redirected by whatever launched the app.
`PYTHONDONTWRITEBYTECODE=1` stops the runtime writing `__pycache__`
next to its code — the bundle ships precompiled pycs (see
`docs/build.md`), and the install dir is not necessarily writable —
and app code inherits it: a packaged app that wants a writable cache
must put it somewhere appropriate itself.
`PYTHONSAFEPATH=1` keeps the child's cwd off `sys.path` — a `json.py` or
`pylair/` in the launch directory would otherwise shadow the stdlib or
the runtime itself for `python -m` — and the packaged cwd is pinned to
the bundled resources dir rather than the directory the app was
launched from. That dir is not necessarily writable (a signed macOS
`.app`, an installed package): app code should resolve the OS app-data
directory for files it writes, never relative paths off cwd. Isolated
workers inherit all of this. AppImage `APPDIR` entries are
stripped from `LD_LIBRARY_PATH`/`PATH` (exact-prefix match — sibling dirs
survive). Dev overrides (`PYLAIR_APP_MODULE`, `PYLAIR_PYTHON`,
`PYLAIR_PYTHONPATH`) are honored **in debug builds only** — a packaged
app ignores them, so the launching environment can't swap which
interpreter or module runs. On Windows the interpreter spawns with
`CREATE_NO_WINDOW` so the GUI app shows no stray console; set
`PYLAIR_CONSOLE` (any value) in the launching environment to keep the
window for debugging — it grants no extra privilege, just visibility.

## 9. Wrap mode (W1 landed; W2 and W3 pending)

Wrap mode shows a Python web framework's own UI (Gradio, Streamlit,
FastAPI...) in the window, served on `127.0.0.1` on a random port. Its
posture, per `docs/decisions/0003-wrap-mode.md`, and where each piece
stands:

| Property | Status |
|---|---|
| The launcher and the supervisor's `Web` runtime kind (`python -m pylair.wrap`, `ready {url}`, `ready_timeout`) | **Done (W1).** Wire contract in `docs/protocol-v1.md` section 4.1. |
| The ready url is validated before use: `http`, host exactly `127.0.0.1`, explicit port, no credentials, same origin as `http://127.0.0.1:<port>` (`validate_ready_url`, built on `same_origin`) | **Done (W1).** Anything else is a protocol violation: the runtime is killed and restarted. |
| The server's stdout can never corrupt the frame channel | **Done (W1).** Frame fd hygiene plus a closed stdin and logs-only stdout/stderr for the server; covered by a fault-injection test. |
| The server and everything it spawned die with the launcher | **Done (W1).** POSIX: the server shares the launcher's process group, which the supervisor kills and reaps (the launcher also walks the group, escalating SIGTERM to SIGKILL). Windows: the launcher sits in the supervisor's Job Object with `KILL_ON_JOB_CLOSE`. Covered by tests on both. Proven per OS, not across them: see the CI legs. |
| Hung-server detection | **Done (W1).** The launcher probes the server over HTTP and exits non-zero after 3 consecutive misses. Pings alone cannot see a wedged server. |
| In-window navigation locked to the app origin, other links to the system browser | **W2.** `same_origin` exists and is tested; the navigation and new-window handlers are not registered yet. |
| The wrapped page gets zero Tauri IPC | **Verified in the W0 spike** (all `plugin:*` invokes rejected from the remote origin); the permanent acceptance-job check is **W2/W3**. |
| Adapters force `share=False`, disable tunnels and framework telemetry | **W3.** Not built. `command` mode runs whatever the app does. |
| The server listens on loopback only | **Not enforced by W1.** The launcher chooses and passes a loopback *port* (`{port}`, `PYLAIR_PORT`) and probes `127.0.0.1`, but a `command` that binds `0.0.0.0` is reachable from the network. Recipes and adapters must pass `127.0.0.1` as the host. |

- The wrapped page is a **remote origin, so it gets no Tauri IPC**. A
  compromised wrapped page cannot reach `python_*` or any plugin command.
  `same_origin` is parse-based, so `evil.com` subdomains, `user@host`
  tricks and default-port confusion all fail.
- **Stated exposure, no more:** a loopback server is reachable by other
  local processes and by pages in the user's browser, the same exposure as
  `streamlit run` on a developer machine. The port is picked by binding
  `127.0.0.1:0`, reading it and closing it, so another local process can in
  principle take it before the server binds (the server then exits, the
  launcher exits non-zero, and the restart picks a new port). A
  token-checking reverse proxy is documented future hardening, and is
  insufficient alone, since the app's own port stays open.
- A web runtime exposes no `@expose`d functions: every `call` answers
  `FUNCTION_NOT_FOUND`, so the allowlist model of section 3 is unchanged.
  `python_state` returns `{state, url}` for a web runtime and the bare
  string for the bridge kind.

## 10. Supply chain and distribution

- The bundled interpreter is a pinned python-build-standalone build plus
  a `uv`-locked environment — the app doesn't depend on whatever Python
  the user's machine happens to have.
- `pylair build` output is **unsigned** unless the app developer
  configures code signing; expect Gatekeeper/SmartScreen warnings, as
  documented in the README. Bundles ship `THIRD-PARTY-NOTICES` for the
  CPython build and Python dependencies they contain.
- Signing keys and secrets never live in the repo; CI signing uses
  repository secrets (project brief §11).
- `cargo audit` reports **RUSTSEC-2024-0429** (`glib 0.18.5`, unsound) —
  a transitive dependency of Tauri's Linux GTK chain
  (`tauri → wry → webkit2gtk → gtk`), so it affects Linux builds only.
  There is no fix we can apply until Tauri moves off gtk-rs 0.18; the
  advisory is tracked upstream and disclosed here as accepted residual
  risk.
- `cargo audit` also reports **RUSTSEC-2024-0370** (`proc-macro-error`,
  unmaintained). It arrives through the same Linux-only chain as `glib`
  (`tauri → gtk → glib-macros`), so it affects Linux builds only, and no
  action is possible until Tauri moves off gtk-rs 0.18. Accepted
  residual risk for the same reason.

## 11. Reporting

Report vulnerabilities through GitHub's private vulnerability reporting
on this repository — not in public issues or PRs. Pre-1.0, honest bug
reports about the boundaries above are as welcome as exploits.
