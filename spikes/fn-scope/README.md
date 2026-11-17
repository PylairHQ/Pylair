# fn-scope spike: per-function permissions via Tauri command scopes

Branch `spike/fn-scope`. Not for merge. Question: can a capability restrict
which `@expose`d functions a window may call, using command scopes on
`pylair:allow-python-call` and `pylair:allow-python-call-stream`?

Answer: yes, with four design changes (see Recommendation).

## What is here

- `crates/tauri-plugin-pylair/src/fn_scope.rs`: `FnScope { name }`, the pure
  matcher (`pattern_matches`, `permitted`, `unmatched_patterns`), the check,
  and the observation log the tests read.
- `commands.rs`: both call commands take a `CommandScope<FnScope>` and run
  the check before `subscribed(...)`, so a denial touches nothing.
- `protocol.rs` and `docs/protocol-v1.md`: `PERMISSION_DENIED = -32050`.
- This crate: tauri-build compiles `capabilities/*.json` and the plugin's
  permission files into the real ACL; `tests/fn_scope_acl.rs` drives the real
  commands through Tauri's real IPC path (`Webview::on_message`) on the mock
  runtime. Each scenario owns one window label.

## Run

Matcher tests (any OS): `cargo test -p tauri-plugin-pylair --lib fn_scope`.

Harness (not built on Windows, same webview-link constraint as the plugin's
other mock-window tests; run on Linux):

```sh
cd spikes/fn-scope
cargo test --test fn_scope_acl -- --test-threads=1 --nocapture | grep -E '^(OUT|OBS)'
```

## Evidence files

- `evidence/01-baseline-before-change.txt`: 16 harness tests failing against
  the stubs (denied calls reach the supervisor, handlers record nothing).
- `evidence/02-malformed-entry-before-change.txt`: the fail-closed test
  failing before the check existed.
- `evidence/03-passing-run-observations.txt`: the full passing run with, per
  call, what the handler saw (`OBS`) and what came back (`OUT`).
- `evidence/04-mutations.txt`: three mutations and which tests catch them.

## Results

| Q | Answer | Evidence (test name) |
|---|---|---|
| 1 | Yes. An object entry reaches the handler's `CommandScope`. tauri-build accepts scope entries on a permission that declares no scope of its own. | `q1_object_deny_entry_reaches_the_handler_and_denies`, `q1_allow_list_present_and_no_match_is_denied` |
| 2 | A scoped deny beats the unscoped `pylair:default` grant. A scoped allow next to it does not widen it, it narrows it to the allow list. | `q2_scoped_deny_beats_the_unscoped_grant_in_one_capability`, `q2_a_scoped_allow_next_to_the_unscoped_grant_narrows_it` |
| 3 | Scopes union across capabilities for the same window: allows union, denies union, any deny wins. An unscoped grant in one capability does not widen a scoped allow in another. | `q3_allow_entries_from_two_capabilities_union`, `q3_deny_entries_from_two_capabilities_union`, `q3_a_deny_in_one_capability_beats_an_allow_in_another`, `q3_an_unscoped_grant_in_another_capability_is_narrowed_by_a_scoped_allow` |
| 4 | Yes. Each window sees only the scope of the capabilities that cover it. A window no capability covers never reaches the handler. | `q4_each_window_sees_only_its_own_capability` |
| 5 | Empty allow and empty deny, everything allowed. Distinguishable from an allow list (allow is non-empty) and from a deny-only scope (deny is non-empty). Not distinguishable from an explicit `allow: []`. | `q5_no_entries_means_empty_allow_and_empty_deny_and_everything_allowed`, `q5_an_explicit_empty_allow_array_is_indistinguishable_from_no_entries` |
| 6 | Yes, but not with `mock_context`: its ACL is empty and it rejects every plugin command. A context from `generate_context!` over tauri-build output carries the real ACL, and the mock runtime then resolves it. The PYLAIR_SELF_TEST_WEBVIEW self-test was therefore not extended. | `q6_the_stock_mock_context_has_an_empty_acl_and_rejects_every_plugin_command`, `q6_the_real_authority_is_consulted_for_origin_and_window`, `q4` (uncovered window) |
| 7 | Yes. After denied `python_call_stream` and `python_call`: task watermark unchanged, window not subscribed, zero messages on any channel, and the next allowed call gets exactly the next task id. | `q7_denied_calls_take_no_slot_subscribe_nothing_and_send_nothing` |

Behaviour pins: no entries allows everything (`q5`), deny beats a matching
allow (`q1_deny_beats_an_allow_entry_that_also_matches`, unit
`deny_beats_an_allow_that_also_matches`), allow present with no match denies
(`q1_allow_list_present_and_no_match_is_denied`).

Startup warning: prototyped as a lazy check, one warning per window and
pattern on the first call after both the scope and the handshake manifest are
known (`a_pattern_matching_no_exposed_function_warns_once`). A true startup
check is not possible with Tauri's public API: `CommandScope` only resolves
inside an invoke from a webview, and the authority's scope manager is
`pub(crate)`.

## Capability JSON that worked

Simplest form, one entry on the default set (`capabilities/q2c.json`). Both
call commands get the scope, and the other default permissions keep working:

```json
{
  "identifier": "restricted-window",
  "windows": ["restricted"],
  "permissions": [
    { "identifier": "pylair:default", "deny": [{ "name": "admin_*" }] }
  ]
}
```

Allow list form (`capabilities/q1-allow.json`), both commands listed:

```json
{
  "identifier": "reports-window",
  "windows": ["reports"],
  "permissions": [
    { "identifier": "pylair:allow-python-call", "allow": [{ "name": "report_*" }] },
    { "identifier": "pylair:allow-python-call-stream", "allow": [{ "name": "report_*" }] },
    "pylair:allow-python-state",
    "pylair:allow-python-cancel",
    "pylair:allow-python-grant-credit"
  ]
}
```

## Surprises

1. A scoped allow narrows an unscoped grant, in the same capability or in
   another one. Tauri's usual "capabilities add up" model does not hold,
   because an unscoped grant contributes no entries.
2. `allow: []` means allow everything. It reads like "allow nothing".
3. A scope on only one of the two commands leaves the other open. The TS
   client sends every call through `python_call_stream`, but a page can still
   invoke `plugin:pylair|python_call` directly (`q2d`).
4. Scope entries are parsed lazily on the first invoke, not at build time. A
   malformed entry compiles, then every call from that window is refused with
   `error deserializing scope: unknown field ...` (`q10`, `q10b`). That fails
   closed, which is the right direction.
5. The debug-build ACL rejection message lists every capability and window in
   the app. Release builds say only that the command is not allowed.
6. `python_call_stream` now has 9 parameters and trips clippy
   `too_many_arguments`. The spike allows it.
7. By source reading, not tested here: Tauri's `commands.deny` (the
   `pylair:deny-python-call` permission) ignores the window, so it cannot
   express a per-window restriction. Scope entries can. And `pylair://exposes`
   is still broadcast to every subscribed window, so a restricted window that
   has made any allowed call learns the full function list.

## Not proven

A real webview. The mock runtime goes through the same `Webview::on_message`
authority path, but the JS argument serialization, the platform IPC protocol
handler and real window labels are not exercised. Platform results here are
Linux only for the harness, and Windows for the matcher tests, the plugin
suite and the spike crate build.

## Recommendation

Viable as written for the core mechanism. Change these before building it:

1. Ship one idiomatic form and validate it. Document the set-level entry
   (`pylair:default` with `deny`) as the way to restrict a window, so both
   call commands always carry the same scope. Add a startup or `pylair doctor`
   check that flags a capability scoping only one of the two commands.
2. Decide the narrowing semantics on purpose. Either document that any allow
   list narrows every grant for that window, or reject allow lists and offer
   deny entries only. Deny-only composes cleanly (denies union, nothing
   narrows).
3. Treat `allow: []` as a configuration error. The handler cannot tell it from
   no entry, so a build-time check on the capability files is the only place
   to catch it.
4. Do the unmatched-pattern warning at build or `doctor` time by reading the
   capability files against the exposes manifest, since the runtime cannot
   enumerate scopes at startup. The lazy per-call prototype is a fallback.

Also needed: a `PermissionDeniedError` in `@pylair/client` (today a denial
arrives as a plain `PythonError` with code -32050 through `asPythonError`, by
reading, not run in TS), the error row in `docs/protocol-v1.md` made
permanent, and README and `docs/security.md` updated to say invocation scoping
is now available per function.
