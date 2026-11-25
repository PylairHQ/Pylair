# Live inspector (dev builds only)

The inspector is a dev-only window into the supervisor: what calls are
running, how long they take, how they ended, and what the runtime's
health and warm pools look like — live, while you develop.

```
pylair dev --inspect
```

That sets `PYLAIR_INSPECT=1` for the dev process and prints a line
saying the inspector window will open. `--inspect-payloads` (or
`PYLAIR_INSPECT_PAYLOADS=1`) additionally enables payload previews.
Neither flag has any effect on `pylair build`.

## What you get

![the inspector window](inspector.png)

A separate window titled **pylair inspector** opens next to your app
once it starts. It polls the host every ~250 ms and shows:

- **Calls** — the last 1000 calls (oldest dropped), newest first:
  function, window, mode (`inline`/`isolated`/`warm`), start time,
  latency, outcome, request/response sizes, and for streams the chunk
  count. Filter by function, outcome or window; errors, cancellations
  and limit refusals are highlighted. The copy button on each row puts
  the row's JSON on the clipboard. **Pause** freezes the display.
- **Per-function latency** — call count, error count, p50/p95 latency
  over the recorded window.
- **Health** — the `health()` snapshot: transport and loop verdicts,
  missed pongs, RTT, load totals, restart accounting, app-readiness
  components, and the update state for updater-enabled apps.
- **Warm pools** — per-function pool status (see below).

## Outcomes

Every recorded call ends in exactly one outcome:

| outcome | meaning |
|---|---|
| `ok` | normal result |
| `error <code>` | the wire error code (protocol-v1 §3) |
| `cancelled (verdict)` | `TASK_CANCELLED`, with the cancel verdict when the runtime reports one |
| `limit <name>` | `LIMIT_EXCEEDED`, with the `data.limit` cap name |

Calls still running show as `in flight` (blue) with a live latency.

## Payload previews

Off by default. With `--inspect-payloads` (or
`PYLAIR_INSPECT_PAYLOADS=1`) each row additionally carries a truncated
JSON preview of the input and result — at most ~512 characters.
**Blob bytes are never included**: a `{"$b64": …}` payload is shown as
a size marker (`<N bytes>`). Previews are still dev tooling — don't
enable them around data you wouldn't want in a screenshot.

## Warm pools in health()

Independent of the inspector, `health()` now carries a `pools` object
for every app (debug or release, inspector or not):

```json
"pools": {
  "transcribe": {
    "size": 2, "live": 2, "busy": 1, "idle": 1, "queued": 0,
    "deaths": 0, "crash_loop": false, "idle_ttl_s": 30.0, "reaps": 1
  }
}
```

One entry per warm-pooled function that has been used this generation:
`size` the configured pool size, `live` members alive now, `busy`/`idle`
the split of live members, `queued` calls waiting for a member,
`deaths` consecutive member deaths with no success between them,
`crash_loop` whether the pool stopped after too many deaths,
`idle_ttl_s` the configured per-member idle TTL (`null` = live until
shutdown) and `reaps` how many idle-TTL reaps have happened so far.
The runtime reports this on each pong; `pools` is `null` until the
first pong of a live generation and absent entirely for wrap-mode
(`web`) runtimes.

## How it works (and what it costs)

- The tap lives in the supervisor, recorded at call start and end. It
  is a fixed-size ring — it never allocates unboundedly, and it never
  uses the events channel, so a burst of recorded calls cannot cost
  you real `pylair://` events.
- **Debug builds only.** Everything is behind `cfg(debug_assertions)`:
  a release build compiles a zero-sized shell instead — no ring, no
  page, no window, no command.
- **Off by default even in debug.** Without `PYLAIR_INSPECT=1` the hot
  path cost is one atomic load. With the flag on, recording uses a
  try-lock: a contended sample is dropped rather than ever blocking a
  call.
- The window loads one bundled static HTML/JS page (no framework, no
  network) over a custom URI scheme that answers **only** the inspector
  window's own label — app windows get 403. The single read-only
  command `python_inspect_snapshot` likewise refuses every window
  except the inspector's.
