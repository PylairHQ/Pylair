# Blobs (`pylair.Blob` → `BlobHandle`)

Large binary payloads — generated images and audio, model output,
screenshots, PDFs — don't belong in a JSON frame. A `bytes` return
travels base64-in-JSON (+33% size) inside the 16 MiB frame cap, copied
through every hop. A `Blob` instead lands in a per-generation directory
the host owns, crosses the bridge as a 60-byte `{"$blob": ...}` tag, and
the front end reads it back through a `pylair-blob` URL the host serves
with HTTP range support — zero copies on the bridge, real streaming on
the way out.

Return a `Blob` whenever a payload is bigger than a string should be, or
when the front end wants a URL (`<img>`, `<video>`, `fetch`, a save
picker) rather than an in-memory value. Return `bytes` for small
payloads — under a few hundred KiB the base64 path is simpler and the
handle buys nothing.

## Python: making one

```python
from pylair import Blob, expose

@expose
def render_chart(query: str) -> Blob:
    png = draw(query)                      # bytes you already have
    return Blob(png, mime="image/png")

@expose
def screenshot() -> Blob:
    return Blob.from_path("/tmp/shot.png", mime="image/png")
```

- `Blob(data, mime=...)` — the payload is written to the blob dir
  atomically (tmp file + rename) when the result is serialized. `mime`
  defaults to `application/octet-stream` and must look like a media type.
- `Blob.from_path(path, mime=..., copy=False)` — **hardlinks** the file
  into the blob dir: zero-copy, the served bytes are the same inode.
  Two consequences: the hardlink is free and instant for multi-GB
  files, and **a rewrite of the source invalidates the handle** — the
  host re-stats size+mtime at serve time and answers `410 Gone` if they
  moved (`copy=True` — or a filesystem that can't hardlink, which falls
  back to a copy automatically — isolates the blob from later edits).
- A `Blob` is just a description of its source; every send mints a fresh
  id and file, so the same object can be returned repeatedly, cached at
  module level, and sent from concurrent calls.
- Blobs made inside isolated or warm workers work identically — the
  worker registers through the parent against the caller's task.

A `Blob` may only leave through a **call result or a stream chunk** —
frames that carry a task id, because the host binds the handle's owner
to the window that invoked the task. A `Blob` inside an event or
notification is refused: the frame is dropped and the error is logged
(the developer sees that blobs can only come back from calls/streams).

## TypeScript: the handle

```ts
import { call, type BlobHandle } from "@pylair/client";

const chart = await call<BlobHandle>("render_chart", { query });
imgEl.src = chart.url;              // pylair-blob://… (http://… on Windows)
await chart.release();              // done early — frees the host file
```

A `{"$blob": ...}` anywhere in a result or chunk — top level or nested
in objects/arrays — decodes to a `BlobHandle`:

- `.url` — the URL the host serves for this id. Use it for `<img>`,
  `fetch`, media elements on Windows/macOS, `showSaveFilePicker`
  sources. The webview can only *name* an id; the host decides whether
  to serve it.
- `.stream()` — a `ReadableStream<Uint8Array>` of sequential `Range`
  fetches that advances by the bytes actually received, so it stays
  correct however small the host's per-range cap is. A released or dead
  blob fails the stream with the HTTP status in the message.
- `.mediaUrl()` — fetches the whole payload through `stream()` and
  returns a `blob:` object URL, capped at 256 MiB (larger throws — use
  `.url` or `stream()`). **This is the path for media on Linux**:
  WebKitGTK's custom-scheme media pipeline can't play `pylair-blob://`
  directly, so `<video>`/`<audio>` there want the object URL (instant
  seek once built). On Windows/macOS just use `.url`.
- `.arrayBuffer()` / `.blob()` / `.text()` — read the whole payload
  into the webview through `stream()`.
- `.release()` — frees the host file early. Idempotent; afterwards
  readers throw a clear "released" error and an outstanding
  `mediaUrl()` object URL is revoked.

## Lifetime and limits — why `release()` matters

Registration binds a handle to the **owning window** and to the current
**generation** (one runtime lifetime — a dev reload or supervisor
restart starts a new one). The host enforces:

- **512 MiB per blob**, **2 GiB per generation**, **256 live handles
  per window** — past a limit the registration is refused and the call
  fails rather than growing the dir.
- Serve-time caps: a full-body GET answers at most 64 MiB
  (`PYLAIR_BLOB_MAX_FULL_BODY`, else 416 pointing at `Range`), and a
  single `Range` response at most 8 MiB
  (`PYLAIR_BLOB_MAX_RANGE_BYTES`, truncated `206` — `stream()` just
  issues more requests).
- The per-generation directory lives under the app **cache** dir and is
  deleted when the generation ends — handles die with it (later reads
  404/410). Closing a window releases its handles.

Nothing else reclaims files, so an app that produces blobs steadily and
never releases will hit the 2 GiB generation budget. `release()` is
free, idempotent, and the right end for any blob the UI is done with.

## Older hosts: the `$b64` fallback

Blob support is capability-gated on `PYLAIR_BLOB_DIR`, which the host
exports at spawn. An older host — or one where the dir failed to
resolve — leaves it unset, and a returned `Blob` then serializes as
base64 exactly like `bytes` (`{"$b64": …}`): the front end receives a
`Uint8Array`, not a handle, and app code should tolerate both. If the
payload can't fit the frame cap that way, the call fails with
`LIMIT_EXCEEDED` naming `PYLAIR_BLOB_DIR` — never a silent drop.

## Security in three sentences

A `pylair-blob` URL names an id the webview cannot guess at — 128-bit
ids over files the host controls — and the host serves it only to the
window that owned the returning task, only for requests carrying the
app's own `Origin` header. Everything else gets a short answer: 403
foreign origin or wrong window, 404 unknown id, 410 released or stale.
There is no path traversal surface: the served path is the one
registered, inside the generation dir, re-stat'd on every request.

## More

The full protocol — registration ordering, owner binding, the exact
decision tree, worker forwarding — is the decision record:
[docs/decisions/0010-blob-urls.md](decisions/0010-blob-urls.md).
