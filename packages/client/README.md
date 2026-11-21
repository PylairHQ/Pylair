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

**Release candidate:** while there's no stable release, `latest`
tracks the newest RC — pin an exact version for reproducibility:
`npm i @pylair/client@0.1.0-rc.25`.

Pylair builds on [Tauri](https://tauri.app) but is not affiliated with
or endorsed by the Tauri project.

Licensed under MIT OR Apache-2.0 — see LICENSE-MIT and LICENSE-APACHE.
