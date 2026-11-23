# 0010: Binary blobs by URL (`pylair.Blob` + a `pylair-blob` URI scheme)

Status: accepted (spike: `spikes/b0-blobs` on the `blob-spike` branch;
CI evidence in §Evidence index)

## Question

`bytes` travel as `{"$b64": "<base64>"}` (`docs/protocol-v1.md` §2,
`python/pylair/src/pylair/_json.py`, `packages/client/src/index.ts`) —
+33% wire size, a full base64 decode plus a JSON parse on both ends, and
a hard ceiling: `MAX_FRAME` (16 MiB default) means a function cannot
return more than ~12 MB of `bytes` at all. AI apps want to return
images, audio and PDFs. `docs/ideas.md` parks "temp-file-handle
transport" for this. Should we implement it, and how?

**Direction (accepted for evaluation):** keep the JSON frame small and
move the bytes over a custom URI scheme served by the host from a
per-generation blob directory. Python writes (or references) a file;
the wire result carries only `{"$blob": {"id", "mime", "size"}}`; the
client turns that into an object whose `.url` is usable in `<img>`,
`<audio>`, `fetch`, `showSaveFilePicker` sources and friends.

The spike (`spikes/b0-blobs`) is a two-window Tauri app serving three
on-disk blobs through `register_asynchronous_uri_scheme_protocol` with
manual Range/206 and owner enforcement on `ctx.webview_label()`. Its
findings below are cited per platform.

## 1. Custom protocol behaviour per webview

**URL form.** `<scheme>://localhost/<path>` on macOS, iOS and Linux;
`http://<scheme>.localhost/<path>` on Windows and Android (tauri
`protocol::origin` in `crates/tauri/src/protocol/mod.rs`; documented on
`Builder::register_uri_scheme_protocol`; `use_https_scheme` flips
Windows to https). Observed live on WebView2: app origin
`http://tauri.localhost`, blob base `http://pylair.localhost`. The
client builds the URL — it can mirror `convertFileSrc`'s platform check
(`navigator.userAgent` → `http://…localhost` on Windows/Android).
Scheme name `pylair-blob` (not bare `pylair`: `pylair://state` etc. are
event *topics*, and a distinct scheme keeps CSP readable).

**Response bodies are buffered, not streamed.**
`register_uri_scheme_protocol` requires
`T: Into<Cow<'static, [u8]>>`; the async responder converts the same way
(`app.rs:2267-2350`). There is no file/reader body type — every
response is materialized in the host process. Design consequence: the
handler reads the file per request, so memory per request equals
response size. That is still strictly better than `$b64` (no +33%
expansion, no JSON parse, no copy into the frame channel), but it makes
host memory a function of *request* size, not just blob size — so the
handler bounds both kinds of response (below). The alternative that
truly streams is a 127.0.0.1 token-guarded HTTP server (rejected for
v1 — §Alternatives).

**Host memory is bounded per request.** Buffered bodies mean a plain
full-body fetch of a 512 MiB blob would hold 512 MiB in the host, and
concurrent requests multiply it. Two caps fix that:

| cap | default | what happens over it |
|---|---|---|
| full-body response | 64 MiB (`PYLAIR_BLOB_MAX_FULL_BODY`) | `416` + `Accept-Ranges: bytes` + a body telling the caller to issue Range requests |
| bytes per range response | 8 MiB (`PYLAIR_BLOB_MAX_RANGE_BYTES`) | the handler returns a **shorter 206** (`Content-Range` names the truncated extent) — legal HTTP; media stacks and readers re-request |

Worst case is now `max(full_cap, range_cap)` per in-flight request —
64 MiB, not blob-size. **Proven on all three engines** (the spike serves
the 21 MiB WAV under a 4 MiB range cap): a full-body `GET` → `416`; an
open `bytes=0-` → `206` carrying exactly the cap; WebView2 and
WKWebView media issued follow-on ranges against the truncated
`Content-Range` and `seeked` fired — short 206s are accepted by both
the media pipelines and `fetch`. WebKitGTK's media failure is
unchanged (its pipeline errors *after* a correctly-served probe — it
never even reaches the second range).

Client consequence: `blob.stream()` (§6) reads a blob as a
`ReadableStream` of ranged chunks, so scripted readers never depend on
full-body responses at all; `.url` stays for element loads and small
fetches.

**Range/206 is not automatic** — custom schemes must parse `Range`
themselves (the built-in `asset://` protocol does the same internally).
Proven on WebView2: `bytes=0-99` → `206` + 100 bytes; a seek in a ~21 MB
`<audio>` issued a second request `bytes=15859712-` → `206` and `seeked`
fired. Media via a custom scheme is real on WebView2.

**Element loads vs scripted reads.**
- `<img>`: works everywhere (WebView2 proven; element loads are the
  well-trodden custom-scheme path on WKWebView/WebKitGTK).
- `<audio>`/`<video>`: WebView2 fine (proven locally and on CI: a 90 s
  seek in a 120 s WAV issued `bytes=15859712-` → `206` and `seeked`
  fired). **WKWebView works** (proven on the macOS leg): `seeked` fired;
  the media pipeline issued a sniffing cascade of tiny ranged probes
  (`bytes=0-1`, `0-8191`, `12-19`, …) then `bytes=21167916-` on seek —
  all 206, all owner-attributed. That many-requests-per-seek pattern is
  the generic URL-scheme pipeline the ~90–300 ms/seek overhead
  measurements describe — functional, not fast. WebKitGTK is the broken
  leg — **proven on the ubuntu leg**: the media element DID issue a
  ranged probe (`bytes=0-1445` → `206`, correctly served and
  attributed), then errored anyway — the failure is inside the
  WebKitGTK media pipeline after delivery (tauri#3725, open since 2022;
  WebKit#146351; gstreamer only reaches http(s)/file).
- `fetch`/`XMLHttpRequest`: the blob origin is cross-origin to the app
  origin on every platform, so responses must carry
  `Access-Control-Allow-Origin` (tauri's own doc warning). **Proven on
  all three engines** — fetch 200 and XHR 200 on WebView2, WKWebView and
  WebKitGTK alike. The historical "fetch cannot load custom scheme at
  all" reports were `https:`-origin pages, not the app's own scheme
  origin (tauri#6962 shows the same: the handler returns 200 and the
  fetch fails solely on missing ACAO).
- Web Workers: `new Worker(blobUrl)` fails everywhere — worker scripts
  must be same-origin, orthogonal to blobs. `fetch` *inside* an existing
  worker is **proven on all three engines** (200, owner label
  attributed).
- `blob:` object URLs: unaffected (the client can always
  `fetch` + `createObjectURL` — this is the Linux media fallback, §6).

**CSP additions.** The default template's CSP needs the scheme in
`img-src`, `media-src` and `connect-src`, and `worker-src 'self'` is
already implied by `default-src 'self'`:

```
connect-src 'self' ipc: http://ipc.localhost pylair-blob: http://pylair-blob.localhost
img-src 'self' data: blob: pylair-blob: http://pylair-blob.localhost
media-src 'self' blob: pylair-blob: http://pylair-blob.localhost
```

(`blob:` for the `mediaUrl()` object URL, and `'self'` on `media-src`
is required — the directive's mere presence drops the `default-src`
fallback bundled media relied on.)

(Both scheme spellings, so one CSP covers all OSes.) The wrap template's
CSP is updated identically for consistency, though wrap pages are remote
and blob-bearing answers never reach them (§2).

## 2. Owner enforcement — the requesting webview's label

Every request is attributed to the webview that issued it, per engine:

- **WebView2 (Windows):** `WebResourceRequested` is registered
  per-webview and carries the captured webview id
  (`src/webview2/mod.rs` in wry); the filter uses
  `COREWEBVIEW2_WEB_RESOURCE_REQUEST_SOURCE_KINDS_ALL` (incl. worker and
  iframe kinds on supported runtimes). **Proven:** element loads,
  fetch, XHR, worker fetches and media Range requests all arrived with
  the correct `webview_label`, and a second window's reads 403'd in
  every form.
- **WKWebView (macOS):** wry creates a scheme-handler object per
  webview (each storing that webview's id) on its
  `WKWebViewConfiguration`; `start_task` reads the id off the handler —
  attribution is structural, not per-request lookup. **Proven on the
  macOS leg**: every request type carried the right label and
  cross-window reads 403'd in every form.
- **WebKitGTK (Linux):** the scheme handler is WebContext-level
  (shared), but wry calls `request.web_view()` on each
  `URISchemeRequest` and reads the stored `WEBVIEW_ID` — per-request
  attribution via `webkit_uri_scheme_request_get_web_view()`
  (WebKitGTK ≥ 2.40). **Proven on the ubuntu leg**: element loads,
  fetch, XHR, worker fetches and even the media probe all carried the
  right label; cross-window reads 403'd in every form. If the lookup
  ever fails the label is empty — enforcement must treat *empty as
  never equal to an owner* (fail closed).

Binding is by **task owner**: `blob.register {id, task_id, …}` arrives
ahead of the response; the host resolves `task_id → owner` from
`TaskMeta.owner`, which already records `window.label()` (decision
0005). The window label is never taken from Python or JS input. Unguessable
ids (128-bit random) are defense-in-depth — owner binding is the actual
guard; a leaked id is useless to another window either way.

**Origin gate — labels are necessary, not sufficient.** A webview keeps
its label if it navigates off the app origin, so owner-by-label alone
would let a foreign page loaded in `main` read `main`'s blobs — and a
blind `Access-Control-Allow-Origin: *` would let it read them over
scripted `fetch` too. The handler therefore checks, in order:

1. `Origin` present → it must equal the app's origin for this platform
   (`tauri://localhost` / `http://tauri.localhost`) — else `403`,
   before the blob is even looked up.
2. `Origin` absent → an element or no-cors load. These carry no
   scripted readback channel (cross-origin canvas taints; `<audio>`
   pixels-in only), so the label check below is the guard — they are
   served, which is also what keeps `<img>`/`<audio>` working.
3. Label must equal the blob's owner (as before).

CORS response headers echo *exactly the app origin* in
`Access-Control-Allow-Origin` with `Vary: Origin` — never `*`. A
foreign-origin fetch therefore fails twice: the handler 403s it, and
even a hypothetical pass would fail the CORS check since the page's
origin isn't in the response.

Every response — errors included — also carries `Accept-Ranges: bytes`,
`Cache-Control: no-store`, and `X-Content-Type-Options: nosniff`. The
last matters for the no-`Origin` path: element loads carry no `Origin`,
so a foreign page sitting in the owner window could pull a text or JSON
blob in through a `<script>` or `<link>` tag and read it; `nosniff`
makes the engine refuse the MIME-mismatched load.

**Proven on all three engines** (run 45121029887): after both windows
finish, the spike navigates `main` to a page served from
`http://127.0.0.1:<port>` — the label stays `main`, the origin becomes
foreign. Every scripted read — `fetch`, ranged `fetch`, XHR, and a
fetch inside a `127.0.0.1`-origin worker — arrives as
`label=main origin=http://127.0.0.1:<port>` → `403`. The same page's
`<img>` load (no `Origin`) is served `200` — expected: pixels only, no
readback. The scheme log carries `origin=` per request so the gate's
decisions are directly visible.

Wrap mode: wrapped pages load a remote/local web UI and get no
`@pylair/client` and no IPC, so they can never learn an id or own a
task — out of scope, and both gates deny them anyway (their origin is
never the app origin).

## 3. Wire changes (additive, protocol-v1)

- **Value tag** `{"$blob": {"id", "mime", "size"}}` — valid anywhere a
  result or `stream.chunk` value is valid. An old client that has never
  seen the tag passes it through as a plain object (degraded, not
  corrupt — pins keep the trio in step anyway).
- **Notification** `blob.register {id, task_id, path, mime, size}` —
  emitted ahead of the frame that carries the matching `$blob` value
  (single writer thread → FIFO makes registration-then-use atomic).
  `path` is host-internal metadata for the registry; it is never
  forwarded to any webview. Notifications are already additive-safe —
  an old host drops unknown ones (protocol §2's notification rule).
- **Negotiation needs no handshake change.** `PYLAIR_BLOB_DIR` set in
  the spawn env *is* the capability bit: the host only sets it when it
  implements the scheme (same pattern as `PYLAIR_APP_DATA_DIR`,
  `spec.rs:146`). Old host + new `pylair`: the dir is unset → `Blob`
  falls back to `{"$b64": …}` when the payload fits the frame cap, or
  the call fails `LIMIT_EXCEEDED` naming the feature — a transparent
  downgrade for small payloads. New host + old `pylair`: `Blob` is
  never used; nothing changes. New host + old client: passes through
  degraded, as above.
- **Dev reload:** the blob dir is per-generation — the restarted runtime
  gets a fresh `blobs/<gen>/`; the host deletes the previous
  generation's dir at teardown. Blob URLs from before the reload die
  with their generation, which is correct — the windows that held them
  are dead too (or hold a `release()`d handle that now 404s cleanly).

## 4. Python side

```python
import pylair

pylair.Blob(data, mime="image/png")            # bytes-like → file
pylair.Blob.from_path(path, mime="audio/wav")  # reference an existing file
```

- `Blob(data)` writes `PYLAIR_BLOB_DIR/<id>` atomically (tmp + rename)
  at encode time. `_json._default` grows a `Blob` branch; the ids a
  serialization pass produced are queued and flushed as `blob.register`
  notifications **before** the carrying frame.
- `Blob.from_path` hardlinks the source into the blob dir (zero-copy,
  same volume in practice) with a `shutil.copy` fallback. Python is
  trusted app code — it may name real paths; the webview may only ever
  name an `id`, so no path ever crosses the web boundary.
- **Hardlink aliasing is handled by verify-at-serve, not copy.**
  Registration records `(size, mtime)` in `blob.register`; the handler
  re-stats the file per request and answers `410 Gone` on mismatch —
  an in-place rewrite of the source can never serve silently-stale
  bytes. Chosen over copying because `from_path`'s whole point is
  avoiding a per-call copy of possibly-huge generated media, and the
  failure mode it guards is both rare and *detectable* (rename-style
  producers — write tmp + rename over — create a new inode, so the
  hardlinked blob keeps the old bytes and never even trips the check;
  only `open(path,'wb')`-style in-place writes do, and the 410 names
  exactly that). The spike proves it: `mut_main` serves `200`, the
  source is rewritten, the next read is `410`. A `copy=True` escape
  hatch remains an additive option if apps prove to need it.
- Isolated and warm workers: workers inherit `os.environ`
  (`PYLAIR_BLOB_DIR` rides with it, like `PYLAIR_APP_DATA_DIR`), and a
  worker's `blob.register` travels its own channel — the host binds it
  to the task's owner regardless of which runtime wrote it.
- Failure path: a blob written but never referenced by a successful
  frame (call raised mid-serialize) is unlinked on the error path;
  anything past that is covered by generation-end sweep and caps —
  bounded residue, never unbounded.
- `release()` in JS maps to a host command `python_release_blob`
  (permission-gated, owner-checked like every command) which the client
  calls with the handle — Python side is passive.

## 5. Inbound direction (UI → Python) — phase 2 sketch

Dropped/picked files give the webview a `File`/`FileSystemFileHandle`.
Sketch only: a host command `python_blob_offer` takes the file
(channel-uploaded or via the plugin's existing temp-file plumbing),
writes it into the generation's `in/` area, and registers
`{id, owner}`; the JS then calls with `{"$blobref": id}` as a param;
`_json` coerces it to a `pylair.IncomingBlob` handle exposing `.path`
and `.open()`. Same id/owner model, plus: the offer must be
permission-gated per function (a callable naming an inbound blob is an
app-declared surface), size-capped like outbound, and path-guaranteed to
live inside `in/` — never a `Blob.from_path`-style app path the JS
influenced. Out of v1 scope; the wire tags and caps are designed so it
slots in without rework.

## 6. Limits and lifetime

| knob | default | over it |
|---|---|---|
| per-blob bytes | 512 MiB (`PYLAIR_MAX_BLOB_BYTES`) | `LIMIT_EXCEEDED` `data.limit="blob_bytes"` |
| total live bytes / generation | 2 GiB (`PYLAIR_MAX_BLOB_TOTAL`) | same, admission-time |
| live handles / window | 256 (`PYLAIR_MAX_BLOBS_PER_WINDOW`) | same, `data.limit="blob_handles"` |

- `release()`: drops the registry entry and unlinks the file;
  idempotent; owner-checked.
- Window close: the existing cancel-on-close reaper (0005) frees the
  window's live blobs — close kills both tasks and their blobs.
- Generation end: the generation's blob dir is deleted wholesale —
  there is no cross-generation lifetime by design (a new runtime is a
  new blob namespace; models that must persist live in
  `PYLAIR_APP_DATA_DIR`, not the blob dir).
- Startup sweep: at spawn the host removes `blobs/gen-*` dirs not owned
  by the current boot (crash residue).
- Frame/stream interplay: `$blob` values are small — the frame cap is
  untouched, and a 500 MB `Blob` response produces one small register
  frame + one small response frame. `stream.chunk` may carry `$blob`
  values; each chunk's register precedes its chunk frame. Blob bytes
  never travel a frame, so `MAX_FRAME`/`MAX_DEPTH` don't apply to them.
- Per-window fairness: live blob bytes join `TaskMeta.owner`'s window —
  the admission formula (0005) gains a `blob_bytes` in_use term so one
  window can't drain the 2 GiB generation pool.
- Client handle API: `handle.url` (scheme URL — `<img>`, `fetch`,
  media on Win/macOS), `handle.stream()`, `handle.mediaUrl()`,
  `handle.release()`.
  - `stream(): ReadableStream<Uint8Array>` — sequential ranged
    `fetch`es (`Range: bytes=pos-end`), advancing by **bytes received**,
    so it tolerates any server range cap without knowing it (server
    shortens → client re-requests). Scripted readers get bounded host
    memory end-to-end regardless of blob size.
  - `mediaUrl(): Promise<string>` — `URL.createObjectURL(await new
    Response(stream()).blob())`: one in-webview copy, then the
    *native* media pipeline (instant seek, works on WebKitGTK — the
    Linux fallback). Its own cap (256 MiB, over → named error
    "too large for mediaUrl — use .url or stream()") keeps the webview
    copy bounded; documented, not magic.
  - `.url` for a plain `fetch` over `PYLAIR_BLOB_MAX_FULL_BODY` gets
    the `416` — the error text points at `stream()`.

## 7. Test plan and PR split

**Rust (crate):** scheme-handler unit tests (200/206/410/416/404/400/403,
empty-label denial, `..`/non-id paths, caps); registry + owner binding
(`task_id → label` via the existing meta map); generation-dir lifecycle
(create on spawn, delete on end, sweep residue); a `window_fairness`-
style two-window integration test with real second webview reading the
other's blob id → 403; origin gate (foreign `Origin` denied, absent
`Origin` element-style served); full-body cap → `416` + Accept-Ranges;
range cap → short `206` with truncated `Content-Range`; mutated
`from_path` source → `410`; release + double-release.

**Python (`pylair`):** codec round-trip (`Blob` → `$blob` tag);
register-before-response ordering asserted on the frame channel;
`PYLAIR_BLOB_DIR` unset → `$b64` fallback / over-cap `LIMIT_EXCEEDED`;
`from_path` hardlink + copy fallback; isolated + warm worker blob
emission bound to the right task; error-path unlink.

**Client (`@pylair/client`):** `$blob` → handle (`url`, `mime`, `size`,
`release()`, `stream()`, `mediaUrl()`); UA-scheme form per platform;
`stream()` resumes correctly on short 206s (mocked truncated
`Content-Range`); `mediaUrl` over its cap → named error; release →
command call; old-tag passthrough.

**Selftest + acceptance:** a `"blob"` selftest check (fixed-payload
`make_blob` decodes to a `BlobHandle`; the real webview fetches the URL
for 200 + exact bytes, a `Range` request for 206 + `Content-Range`,
`stream()` reassembly, an `<img>` load and a `mediaUrl()` `blob:` URL;
`release()` then refuses the URL and a second window's fetch is refused
by owner binding — the id relays through `selftest_blob_*` Python-state
fixtures) alongside `"models"`/`"warm-workers"`; armed on `--local`
legs unconditionally and on registry legs when resolved crate + pylair
+ client ≥ the versions that ship it (same gate pattern —
`PYLAIR_BLOB_EXPECTED`). No media-playback assertion: `mediaUrl()` is
the WebKitGTK coverage.

**PR split:** 1) crate: scheme, registry, `PYLAIR_BLOB_DIR`, notify
handling, caps, cleanup; 2) `pylair`: `Blob`, codec tag, register,
fallback, worker plumbing; 3) client: handle + release + mediaUrl +
selftest check; 4) create-pylair CSP + `docs/blobs.md` + template docs;
5) acceptance wiring. Wire contracts (`protocol-v1.md`, and
`cancellation.md` only if release interacts with cancel semantics)
updated in the same commits.

**Release impact:** all three registries — crate (scheme + host side),
`pylair` (Blob API), npm trio (client handle, templates, CLI docs) — a
full train like 0008/0009.

## Alternatives considered

- **Keep `$b64`, raise `MAX_FRAME`:** fixes nothing real — the parse
  and copy costs dominate, and large payloads shouldn't queue on the
  control channel at all.
- **Chunked stream of `$b64` chunks:** keeps the +33% tax and two
  parses, adds reassembly complexity, and still can't be consumed by
  `<img src>` — media and file-pick needs a URL anyway.
- **127.0.0.1 token-guarded HTTP server:** the *only* design that truly
  streams and uses the native media pipeline on every engine (instant
  Linux video seek, no buffered-body cap on requests). Rejected for v1:
  a listening socket in every shipped app is a materially larger
  security surface (port guessing, token custody, firewall prompts on
  Windows) than plumbing that never leaves webview internals. It
  remains the documented fallback if `mediaUrl()`-style fallback proves
  insufficient in the field — the `$blob`/`blob.register` wire layer is
  identical either way.
- **`asset://` reuse:** Tauri's built-in asset protocol serves static
  scope paths without per-window ownership or dynamic registration —
  can't express owner binding; a custom scheme is required.

## Evidence index

- `spikes/b0-blobs/` — the two-window proof app (`src-tauri`, `web/`),
  its `out/results.json` (local WebView2 run), and `README.md`. The
  CI workflow that ran it headless on all three OSes
  (`.github/workflows/b0-spike.yml`) was removed before merge per the
  spike convention; the runs below are its evidence.
- Runs **45116408180** (ubuntu/windows), **45117258196** (all three
  green, unbounded bodies) and **45121029887** (all three green with
  the bounded-body + origin-gate revision). Artifacts `b0-{os}` carry
  `results.json` per OS:
  - owner label on all request types, cross-window `403` in every form;
  - full-body GET over cap → `416`; open range → short `206`
    (`len == range cap`);
  - media pipelines accept the truncated `Content-Range`: WebView2
    `audioReqs=5 ranged=4`, WKWebView `audioReqs=15 ranged=14`, both
    `seeked`; WebKitGTK `onerror` after a correctly-served probe (the
    pipeline, not the handler — unchanged);
  - `from_path` source rewritten in place → next read `410`;
  - owner window navigated to `http://127.0.0.1` — same `main` label,
    foreign origin — every scripted read `403`.
- wry internals: `src/webview2/mod.rs` (per-webview filter incl. worker
  source kinds), `src/wkwebview/class/url_scheme_handler.rs`
  (per-webview handler instance carrying the id),
  `src/webkitgtk/web_context.rs` (`request.web_view()` → `WEBVIEW_ID`).
- tauri internals: `crates/tauri/src/app.rs:2267-2350` (`Cow` bodies),
  `crates/tauri/src/protocol/mod.rs` (per-OS origin forms),
  `src/plugin.rs:636` (plugins may register schemes).
- Upstream issues: tauri#3725 (WebKitGTK media via custom scheme),
  tauri#6962 (fetch reaches handler, ACAO-gated), wry#946 (fetch/XHR
  blocked from https: origins), WebKit#146351, gstreamer#3021
  (media-scheme support), and the 2026 WKWebView per-seek-overhead
  measurement cited in §1.
