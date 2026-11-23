# @pylair/client

TypeScript client SDK for Pylair apps: typed calls to exposed Python
functions, streaming, cancellation, lifecycle events, and wire/base64
helpers — everything the webview side of a Pylair app needs.

```ts
import { call, stream } from "@pylair/client";

const sum = await call<number>("add", { a: 1, b: 2 });

for await (const token of stream<string>("generate", { prompt })) {
  // tokens arrive as Python yields them
}
```

Errors surface as `PythonError` subclasses: `LimitExceededError` when a
bounded resource refuses the call (`.limit` names the cap — including
`"window_share"` when a window exceeds its per-window share, with
`data.window` naming the over-share owner), and `PermissionDeniedError`
when the window's capability does not permit the function.

## Runtime health

`health()` returns the host-observed snapshot (state, generation,
restart accounting, liveness, load totals, and the calling window's own
fairness row). `onHealth` delivers `pylair://health` events — emitted
only when a liveness verdict *changes*, so "the runtime is busy" UI
needs one subscription, not a poll loop:

```ts
import { onHealth } from "@pylair/client";

const banner = document.getElementById("busy-banner")!;
const unlisten = await onHealth((c) => {
  banner.hidden = c.liveness.loop !== "busy";
});
```

Both are read-only observation: they subscribe to events and never join
the fairness live set, so a poller reserves no share. A
`pylair:deny-python-health` capability deny is **origin-wide** — it
refuses `health()` on every local webview, not just the window carrying
it.

## App readiness

`health()` answers *host* questions ("is the runtime alive? busy?").
`health().app` answers the app's own: which components are still
warming up. The Python side reports it — a startup hook for slow
setup, progress while it works, and `requires=` so calls wait for the
components they need instead of racing them:

```python
import pylair

@pylair.on_startup
def load_model():
    pylair.loading("model", 0.1, detail="fetching weights")
    # … load …
    pylair.ready("model")

@pylair.expose(requires="model")
def chat(prompt: str) -> str:      # waits until `model` is ready
    ...
```

On the UI side, `health().app` carries the latest report per component
(`phase`, `progress`, `detail`, `ms_ago`) and `onApp` fires
`pylair://app` on each phase transition — render the snapshot once,
then flip components as transitions arrive:

```ts
import { health, onApp } from "@pylair/client";

const app = (await health()).app;
if (app) {
  for (const [name, c] of Object.entries(app.components)) {
    setPhase(name, c.phase);      // "loading" | "ready" | "failed"
  }
}
const unlisten = await onApp((c) => setPhase(c.name, c.phase));
```

`app` is `null` when the runtime has reported nothing (and always on
wrapped `web` runtimes — they can't report), and absent on hosts older
than app readiness; code should treat both as "no app state". The
event fires on phase changes only — `progress` and `detail` are
poll-only, so a progress bar reads `health().app` on its own cadence.

## Blobs

A Python function that returns `pylair.Blob` — generated media, model
output, anything too big for a JSON frame — arrives as a `BlobHandle`;
the bytes never leave the host's filesystem:

```ts
import { call, BlobHandle } from "@pylair/client";

const img = await call<BlobHandle>("render_chart", { query });
imageEl.src = img.url;            // a pylair-blob: URL the host serves
await img.release();              // done with it early — frees the file
```

- `handle.url` — the `pylair-blob:` URL for `<img>`, media (Windows/macOS),
  `fetch`, `showSaveFilePicker` sources. The webview can only ever name an
  id; the host serves it to the owning window only.
- `handle.stream()` — a `ReadableStream<Uint8Array>` of sequential ranged
  reads; it advances by bytes received, so it stays correct however small
  the host's range cap is. A released/dead blob errors the stream with
  the HTTP status (403/404/410).
- `handle.mediaUrl()` — a `blob:` object URL built from `stream()`:
  one copy into the webview, then the *native* media pipeline (instant
  seek — and the path for Linux WebKitGTK, whose custom-scheme media
  pipeline is broken). Capped at 256 MiB; larger throws naming
  `.url`/`stream()`.
- `handle.arrayBuffer()` / `handle.blob()` / `handle.text()` — the whole
  payload through `stream()`.
- `handle.release()` — frees the blob early (`id`, `mime`, `size` stay
  readable). Idempotent; afterwards readers throw a clear "released"
  error and a `mediaUrl()` URL is revoked.

A `pylair.Blob` returned on a host without blob support arrives as
`Uint8Array` (the `$b64` fallback) instead of a handle — the documented
downgrade. Blobs die with their generation: a dev reload or window close
invalidates outstanding handles, which then read as 404s.

**Release candidate:** while there's no stable release, `latest`
tracks the newest RC — pin an exact version for reproducibility:
`npm i @pylair/client@0.1.0-rc.28`.

Pylair builds on [Tauri](https://tauri.app) but is not affiliated with
or endorsed by the Tauri project.

Licensed under MIT OR Apache-2.0 — see LICENSE-MIT and LICENSE-APACHE.
