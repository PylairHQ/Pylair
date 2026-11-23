# b0-blobs spike — throwaway evidence for decision 0010

Proof code backing `docs/decisions/0010-blob-urls.md`. Not product code —
the implementation PRs rewrite this as the real handler + test suite.

A minimal Tauri app that registers a `pylair` URI scheme serving four
on-disk blobs (`img_main`, `audio_main`, `mut_main` — owned by window
`main`; `img_other` — owned by window `other`) with manual Range/206,
bounded response bodies (1 MiB full-body cap → `416`; 4 MiB range cap →
short `206`), an Origin gate (foreign `Origin` → `403` before lookup;
absent `Origin` = element load, label-checked only), registered
`(size, mtime)` verify-at-serve (`410` on in-place rewrite), and owner
enforcement on `ctx.webview_label()`. Two windows each run a
browser-side battery (img, audio seek, fetch, XHR, Web Worker fetch,
cross-window reads, bad paths, caps) and report via `invoke`; after both
report, `main` is navigated to a foreign `http://127.0.0.1` page — same
label, foreign origin — whose scripted reads must all `403`. Every
scheme request logs `label` + `origin`, so the results show whether
attribution survives element loads, fetch, and worker fetches.

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
  (`bytes=0-1`, `0-8191`, `12-19`, …) then `bytes=21167916-` on seek,
  and accepts short `206`s under the 4 MiB range cap
  (`audioReqs=15 ranged=14`, `seeked`).
- **Bounded bodies + origin gate (run `45121029887`, all three legs
  green):** full-body GET over the 1 MiB cap → `416` + `Accept-Ranges`;
  `bytes=0-` on the 21 MiB WAV → `206` carrying exactly 4 MiB
  (truncated `Content-Range`, re-requested by the media stacks);
  in-place rewrite of `mut_main`'s source → `200` then `410`;
  `main` navigated to `http://127.0.0.1:<port>` — `label=main
  origin=http://127.0.0.1:<port>` → `403` on fetch, ranged fetch, XHR
  and a `127.0.0.1`-origin worker fetch; the same page's `<img>`
  (no `Origin`) is served — pixels only, no readback.
- CI runs: `45116408180`, `45117258196`, `45121029887` (workflow
  `b0-spike.yml`, removed before merge per the spike convention);
  per-OS `results.json` in the run artifacts.
