# b0-blobs spike — throwaway evidence for decision 0010

Proof code backing `docs/decisions/0010-blob-urls.md`. Not product code —
the implementation PRs rewrite this as the real handler + test suite.

A minimal Tauri app that registers a `pylair` URI scheme serving three
on-disk blobs (`img_main`, `audio_main` — owned by window `main`;
`img_other` — owned by window `other`) with manual Range/206 support and
owner enforcement on `ctx.webview_label()`. Two windows each run a
browser-side battery (img, audio seek, fetch, XHR, Web Worker fetch,
cross-window reads, bad paths) and report via `invoke`; the app exits
once both have reported. Every scheme request logs the attributed
`webview_label`, so the results show whether attribution survives
element loads, fetch, and worker fetches.

Run it:

```sh
cd spikes/b0-blobs/src-tauri
cargo build
# Linux: xvfb-run -a
PYLAIR_B0_OUT=<repo>/spikes/b0-blobs/out ./target/debug/b0-blobs
```

Results land in `spikes/b0-blobs/out/results.json` (per-window report +
the full scheme-request log).

## Results

- **Windows / WebView2** (local run, tauri 2.12 / wry 0.57): all checks
  green — `http://pylair.localhost` URL form; `<img>` load; fetch/XHR
  200 with ACAO; worker fetch 200; `Range: bytes=0-99` → 206; `<audio>`
  loads via `bytes=0-` and seeks fire `seeked`; every request type
  (element, fetch, XHR, worker fetch, media Range) carries the correct
  `webview_label`; the second window's reads of `main`'s blobs 403 in
  every form. See `out/results.json` (committed for evidence).
- **macOS / WKWebView** and **Linux / WebKitGTK**: the same binary is
  exercised by `.github/workflows/b0-spike.yml` (workflow_dispatch on
  this branch) — results in run artifacts; expected findings are noted
  in the decision doc (fetch needs ACAO; media via custom scheme is the
  weak leg, see tauri#3725 / WebKit#146351).
