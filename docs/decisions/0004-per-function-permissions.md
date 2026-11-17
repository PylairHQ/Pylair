# 0004: Per-function permissions

Decided 2026-11-17. Spike: branch `spike/fn-scope` (`spikes/fn-scope/`,
real-ACL evidence in its README); shipped on `fn-permissions`.

## Question

Until now every window covered by a capability that grants
`pylair:default` could call every `@expose`d function. How should an app
restrict which functions a given window may call?

## Options

1. **Tauri command scopes (chosen).** The permission entry in the
   capability carries `allow`/`deny` entries; the plugin's two call
   commands read `CommandScope<FnScope>` and decide before doing anything.
2. **A tag on the Python side** (`@expose(tags=["admin"])`) plus a
   window-to-tags mapping somewhere. The author states the intent next to
   the function, but the mapping still has to live in a capability or a
   builder call, the Python process would have to learn about windows, and
   the decision would be made after the call reached Python.
3. **Both**: tags in Python that capabilities refer to.

## Decision

Option 1, host-side only.

- The capability file is already where Tauri apps say what a window may
  do, it is the file reviewers read, and Tauri already unions it per
  window and rejects uncovered windows before our code runs. A second
  mechanism would be a second place to be wrong.
- A host-side check means a denied call never touches the supervisor: no
  task id, no credit, no channel traffic, no frame to Python. The
  host-to-Python wire protocol does not change, so there is nothing to
  version and nothing for an old runtime to misread.
- The scope entry is `{ "name": ... }` with `deny_unknown_fields`: exact
  name or a single trailing `*` prefix. A narrow grammar is easy to
  explain, easy to check statically (the CLI does) and leaves room to add
  keys later. A malformed entry fails closed.
- Decision rule: no entries allows; a matching deny always wins; if any
  allow exists one must match. Tauri unions scope entries across the
  capabilities covering a window, so an allow entry from any capability
  narrows the window; this is documented rather than worked around.
- Errors: `PERMISSION_DENIED = -32050` with `data {fn, window}`, a
  permanent entry in the protocol table, surfaced as
  `PermissionDeniedError` in `@pylair/client`.
- The pitfalls that cannot be fixed in the plugin are caught in the CLI
  (`pylair build`, `pylair doctor`): `allow: []` (means no entries, i.e.
  everything), a window scoping only one of the two call commands,
  malformed entries, and patterns matching no exposed function. The
  runtime logs no "matches nothing" warning.

## Deferred

- **Python-side tags**, if apps ask for the intent to live next to the
  function. They would be sugar over this mechanism (a tag expands to
  names at build time), not a second enforcement path.
- **Per-window filtering of `pylair://exposes`.** Names remain visible to
  every window; the scope stops calls, not discovery. Filtering needs the
  event routing to know the scope, and the generated stubs would differ
  per window.
- Argument-level scopes.

## Consequences

- The template capability stays open; restricting is opt-in.
- The installed two-window self-test (`PYLAIR_SELF_TEST_WEBVIEW`) now
  proves a per-window denial on every OS: `selftest-winb.json` denies
  `add` for `win-b`, which must get `PERMISSION_DENIED` through both
  commands while `main` still calls it.
- A real-ACL test crate (`crates/fn-scope-acl-tests`) embeds capability
  files through `tauri_build` and drives the commands through the mock
  runtime's IPC, so scope parsing and union behaviour are tested against
  Tauri's own ACL, not a model of it.
