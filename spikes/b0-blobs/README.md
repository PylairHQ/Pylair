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

- **Windows / WebView2** (local + CI, tauri 2.12 / wry 0.57): all checks
  green — `http://pylair.localhost` URL form; `<img>` load; fetch/XHR
  200 with ACAO; worker fetch 200; `Range: bytes=0-99` → 206; `<audio>`
  loads via `bytes=0-` and a 90 s seek in the 120 s WAV issued
  `bytes=15859712-` → `206`; every request type carries the correct
  `webview_label`; the second window's reads of `main`'s blobs 403 in
  every form. See `out/results.json` (committed for evidence).
- **Linux / WebKitGTK** (CI, ubuntu-22.04 + xvfb): `pylair://localhost`
  URL form; img/fetch/XHR/worker-fetch all fine with ACAO; owner label
  attributed on every request type; cross-window reads 403.
  `<audio>` is the known-broken leg — the element issued a ranged probe
  (`bytes=0-1445` → `206`, served correctly) then errored anyway; the
  failure is WebKitGTK's media pipeline (tauri#3725 / WebKit#146351),
  not the handler.
- **macOS / WKWebView** (CI): `pylair://localhost` URL form; img/fetch/
  XHR/worker-fetch all fine with ACAO; owner attribution + 403s hold;
  audio seek works — the media pipeline issues many tiny ranged probes
  (`bytes=0-1`, `0-8191`, `12-19`, …) then `bytes=21167916-` on seek.
- CI runs: `45116408180`, `45117258196` (workflow `b0-spike.yml`);
  per-OS `results.json` in the run artifacts.
