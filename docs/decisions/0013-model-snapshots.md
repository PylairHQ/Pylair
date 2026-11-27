# 0013: Multi-file model snapshots (`models.fetch_snapshot`)

Status: proposed
(extends [0009](0009-model-manager.md), which is accepted and
implemented; readiness reporting is [0007](0007-app-readiness.md);
spike evidence gathered against the live Hugging Face API, quoted in
§2 and the evidence index)

## Question

Decision 0009 ships `pylair.models` for exactly one verified file: one
URL, one sha256, one `<name>` file under `<data>/models/`. Real local-AI
models are rarely one file. A Hugging Face repository snapshot is a
directory: `config.json`, tokenizer files (`tokenizer.json`,
`tokenizer_config.json`, `vocab.json`, `merges.txt`,
`special_tokens_map.json`), one or more sharded `*.safetensors` plus a
`model.safetensors.index.json` mapping tensors to shards. GGUF-in-HF
repos, whisper.cpp model dirs, onnx exports and sentence-transformer
repos share the same shape: a directory whose files must agree with
each other, not a lone artifact.

Extend `pylair.models` to directory-shaped models with a snapshot API:

```python
from pylair import models

models.fetch_snapshot(
    name,
    repo="org/model",            # Hugging Face repo id
    revision="<full commit sha>",
    allow=None,                  # glob list; default: everything
    ignore=None,                 # glob list, applied after allow
    headers=None,                # e.g. {"Authorization": "Bearer hf_…"}
)
models.path(name)                # -> Path of the snapshot DIRECTORY
models.path(name, revision=…)    # -> a specific revision, if present
```

Progress reports through app readiness (decision 0007) exactly as the
single-file form does: one component per snapshot, `loading` with exact
byte progress, `ready` on a fully verified snapshot, `failed` naming
the file that failed. `requires=(name,)` gates calls on it unchanged.

## 1. API: revision is required to be a full commit sha

**Decision: `revision` must be a 40-character lowercase hex commit
sha. Branch names and tags are rejected at the argument check.**

The argument:

- **Reproducibility is the entire point of the API.** A snapshot is a
  content set pinned by hash. `revision="main"` names a moving target:
  two machines installing the same app version a week apart can fetch
  different bytes, so a bug report cannot be reproduced and a support
  answer of "which model do you have" has no meaning.
- **Verification without a pinned revision is a race, not a check.**
  The integrity design in §2 pins every file's hash from the tree API
  *at that revision*. If the revision is a branch, the tree query and
  the file downloads are two separate reads of a moving ref: an
  attacker or an upstream force-push between them produces a manifest
  that verifies perfectly against content the author never meant to
  ship together. A commit sha makes the tree response immutable, so
  the manifest is a closed system: every file either matches the
  pinned hashes or the fetch fails.
- **HF commit shas are immutable and content-addressed by git itself.**
  A branch can be force-pushed; a sha cannot point anywhere else.

**Getting a sha must be easy, or the rule loses.** Authoring time
resolution is one lookup: `GET /api/models/<repo>` returns `sha`, the
current default-branch commit. The implementation ships a CLI helper
so the lookup is one command, not a curl incantation:

```
$ pylair models resolve org/model@main
71034c5d8bde858ff824298bdedc65515b97d2b9
```

(`@main` optional; any branch or tag resolves. The helper is an
authoring tool in the dev CLI - it never ships into the runtime, and
the resolved sha is what the app pins in code.)

Rejected alternatives:

- **Branch names allowed with a warning**: the warning trains users to
  ignore it, and the race above remains. Rejected.
- **Branch resolved to a sha at fetch time, then pinned**: this is
  strictly the race above with extra steps. The resolution read is
  still a moving-target read. Rejected. (Resolving at *authoring* time
  and pinning the result in code is the supported flow - the resolve
  helper above.)
- **Tags**: mutable on Hugging Face (they are git refs, not immutable
  release objects). Same problem, rejected as fetch input; fine as
  resolve-helper input.
- **Optional revision defaulting to the default branch**: the worst
  option, since it makes the easy path the unreproducible one.
  Rejected; `revision` is required, not defaulted.

`models.path(name)` resolves the `current` pointer and returns that
revision's snapshot **directory** when it verifies ready (§3), else
`ModelNotReady` exactly as in 0009. `models.path(name, revision=…)`
returns a specific revision directory when present and verified,
independent of the pointer - the escape hatch for pinning a process to
one revision across an upgrade. In a worker process both forms are
resolve-and-check only, same rule as 0009.

## 2. Integrity without per-file sha256 from the author

Single-file `models.fetch` required the app author to supply sha256.
A 40-file snapshot cannot ask the author for 40 hashes. The answer:
the host's own metadata at the pinned revision is already a complete,
immutable manifest.

**What the tree API returns** (`GET
https://huggingface.co/api/models/<repo>/tree/<revision>?recursive=true`,
live response against `hf-internal-testing/tiny-random-gpt2` at sha
`71034c5d…`, trimmed):

```json
[
  {"type": "file", "oid": "4ff64fe5192d88c0b5dbfc578c775c0ce05dd7d0",
   "size": 807, "path": "config.json"},
  {"type": "file", "oid": "cdebb9016e0099550c661ad5d7b4b0db174d2da7",
   "size": 453864,
   "lfs": {"oid": "8111d5afb0715dbf5a31396d31432cb56370ba23f6650a035ea0fc8a20b4e500",
           "size": 453864, "pointerSize": 131},
   "xetHash": "f8accece953fd366d4ce30597b97acc1ccedc3c785187a5ef6ecb4a8e1755122",
   "path": "model.safetensors"}
]
```

Two file kinds, two verification hashes:

| Kind | Detected by | Hash algorithm | Verified against |
|---|---|---|---|
| git blob (small files: config, tokenizer, index) | no `lfs` key; `oid` is 40 hex chars | **git blob sha1**: `sha1("blob " + size + "\0" + content)`, not raw sha1 | the tree entry's `oid` |
| LFS (weights, large binaries) | `lfs` key present; `lfs.oid` is 64 hex chars | **raw sha256** of the content | the tree entry's `lfs.oid` |

The git-blob construction matters: `oid` is the git object id, which
is sha1 over the `blob <size>\0<payload>` framing, so the verifier
must prepend the framing before hashing. Verified locally during the
spike (construction proof in the evidence index). sha1 is weak as a
cryptographic primitive, but here it is not defending anything alone:
it is pinned inside an immutable commit, itself named by sha1, fetched
over TLS from the host that serves the content. An attacker who can
collide the blob sha1 still has to collide it against a specific
framed length inside a specific commit. The marginal risk is accepted
for small text files; the weights, which are the bytes that matter,
are sha256.

`xetHash` appears on xet-backed LFS files. v1 ignores it and verifies
`lfs.oid` in all cases: the classic resolve endpoint serves the whole
file content regardless of storage backend, and `lfs.oid` is the
content sha256 either way.

The tree response is paginated for large repos (`?recursive=true`
plus cursor pagination through the `Link` header). The fetcher walks
all pages before writing the manifest.

**Download URLs**: `https://<endpoint>/<repo>/resolve/<revision>/<path>`
per file, same host family the single-file form already handles.
Redirects to the CDN carry the 302-sensitive-header stripping of 0009
§3 unchanged; this is exactly the gated-repo leak that rule exists
for.

**Non-HF sources**: for hosts without tree metadata, the app supplies
the manifest itself:

```python
models.fetch_snapshot(
    name,
    base_url="https://files.example.com/models/whisper-v3/",
    manifest=[
        {"path": "config.json", "sha256": "…", "size": 1161},
        {"path": "model.bin",   "sha256": "…", "size": 3100000000},
    ],
)
```

Every entry uses raw sha256 (the strong hash; there is no git framing
to mirror), and every file is required to appear in the manifest -
a file present in `allow` but missing from the manifest is a loud
error at argument-check time, since unverifiable bytes must never
enter the snapshot. `base_url` and `manifest` are mutually exclusive
with `repo`/`revision`. Manifest-sourced snapshots use a synthetic
revision key (sha256 of the canonical manifest JSON, truncated to 40
hex) so the revision-addressed storage of §3 works uniformly - a
changed manifest is a changed snapshot, which is exactly the upgrade
semantic of §7.

## 3. Storage layout, atomic readiness, locking

```
<app data dir>/models/<name>/
    current                          # small file: the active revision sha
    <revision>/                      # one verified, immutable snapshot
        <repo files at their repo-relative paths>
        .manifest.json               # the pinned tree for this revision
        .verify.json                 # per-file verification record
        .ready                       # ready marker: {revision, file_count, total_bytes}
        .use                         # reader lock file: shared-locked by every
                                     # process holding path() here (§7)
    <revision>.partial/              # staging: same layout, renamed when complete
        .parts/                      # in-flight: .parts/<path>.part per file
    .lock                            # cross-process fetch lock, as in 0009
```

**Revisions are immutable and content-addressed by their sha.** A
snapshot lives at `<name>/<revision>/` for its entire life; nothing
inside it ever changes once `.ready` lands. Staging happens in the
sibling `<name>/<revision>.partial/` and the completion step is one
rename of a directory **nobody has open yet** - safe on Windows, where
renaming or deleting files a process holds open fails with a sharing
violation (safetensors mmaps weights, so an in-use revision is
immovable). The `current` file is the only mutable thing in the
layout, switched with `os.replace` on a small file - atomic on every
OS including Windows. No directory rename ever touches a revision a
reader might hold.

The rules:

- **A file only enters the staging snapshot verified.** Downloads
  write to `.partial/.parts/<path>.part`; on completion the file's
  hash is checked against the manifest, and only then is it moved into
  the staging tree with `os.replace` (same filesystem). A
  half-downloaded or unverified file never exists at its final path.
- **`.ready` is written last** inside staging, after every manifest
  file is in place and verified; then `<revision>.partial/` is renamed
  to `<revision>/` in one step. A crash at any earlier moment leaves
  only a `.partial` dir, which `path()` never looks at.
- **`path()` cheap-check**: resolve `current`, shared-lock `.use`,
  recheck `.ready` under the held lock (the full protocol is §7 -
  the recheck is what keeps a reader out of a directory GC has
  started deleting), then in the revision dir: `.manifest.json`
  present, every manifest entry exists with the recorded size, and no
  recorded mtime changed.
  One stat per file (typically tens of files, tens of microseconds
  each) with zero hashing; any mtime mismatch re-hashes just that file
  before deciding, mirroring 0009 §1. A failed check raises
  `ModelNotReady`, never a wrong `Path`.
- **Fetch lock**: `.lock` byte-range locked exactly as in 0009 §5
  (kernel lock, released on holder death, no stale cleanup). A second
  caller or process spins, reads progress from `.partial` sizes plus
  verified file sizes against the manifest totals, and reports
  `downloading (shared)`.
- **`.verify.json`** records size + mtime_ns per verified file at the
  moment it lands, so the cheap check above has a trustworthy baseline
  even for files whose content never changes across revisions.

## 4. Readiness, progress and parallelism (decision 0007)

One component per snapshot, the name as passed:

- `loading(name, done_bytes/total_bytes, "x of y GB")` where
  **total_bytes is exact before the first byte downloads** - the tree
  response carries every file's size, so the disk preflight (§6) and
  the progress bar share one precise number. `done_bytes` counts
  verified files in full plus the current length of each `.part`.
- **Parallelism: 4 concurrent file downloads.** Reasoning: snapshot
  repos are latency-bound on small files (a dozen tokenizer configs at
  a few KB each cost one round trip apiece; serially that is seconds of
  pure latency) while the large shards are bandwidth-bound, so a small
  fixed fan-out removes the latency tail without competing with the
  bandwidth-bound shards. 4 matches what browsers and the HF hub
  client use per host; more buys nothing once the bandwidth is
  saturated and multiplies the retry surface. The value is a named
  constant, not a v1 knob.
- Large shards still stream in 64 KiB chunks through the 0009
  machinery per file: same retry policy, same Range/If-Range resume,
  same idle-stall timeout, same redirect header stripping.
- **Resume across restarts**: `.manifest.json` persists the plan,
  verified files persist in place, `.parts/` persists partials. A
  restart re-reads the manifest, re-hashes each `.part` once (the 0009
  re-hash cost, now trivially small relative to a shard), and resumes
  only what is missing. Dev restart mid-snapshot loses nothing but the
  in-flight chunks.
- **Precise failure**: `failed(name, "<path>: <reason>")` -
  `model-00002-of-00004.safetensors: sha256 mismatch: expected … got
  …` or `tokenizer.json: connection reset after 3 retries`. The file
  is always named; a snapshot-level reason (disk space, tree fetch
  failure) names the stage instead: `tree: 404 for org/model@<sha>`.
- Cancellation, thread placement and the declarative
  `models.model_snapshot(name, ...)` module-level form mirror 0009 §5
  exactly, including the auto-start-after-hello rule and the sweep
  hint. Workers stay leaf executors: `path()` resolves, never fetches.
- **Upgrades never flicker**: because revisions are immutable and the
  `current` switch is a file `os.replace` (§7), `path()` always
  resolves to a complete revision - the old one until the instant the
  pointer moves, the new one after. There is no not-ready window
  during an upgrade, so a `requires=(name,)`-gated app never flickers.

## 5. huggingface_hub: neither depend nor interoperate

Evaluated three options against the exact artifact we ship:

1. **Depend on `huggingface_hub`.** The package pulls `requests`
   (`urllib3`, `certifi`, `idna`, `charset-normalizer`), `filelock`,
   `fsspec`, `tqdm`, `pyyaml`, `packaging`, `typing-extensions` and,
   on current versions, `hf-xet` (a compiled Rust extension). That is
   roughly 15 to 25 MB inside every installer for machinery we already
   own: 0009 implements verified download, resume, retries, redirect
   header stripping and readiness reporting, and the hub client's own
   cache layout would bypass our readiness model entirely. The hub
   also re-downloads through its own cache rather than into the app
   data dir, breaking the uninstall and per-app trust story of 0009
   §1. Rejected.
2. **Interoperate with its cache** (`~/.cache/huggingface`): read or
   hard-link files out of the hub cache when present. The cache layout
   (`blobs/`, `snapshots/<revision>/`, refs indirection) is the hub's
   private implementation detail, has already changed shape once (xet
   migration), and sharing files across apps reintroduces the
   cross-app trust problem the per-app data dir deliberately avoids.
   Rejected for v1; a `docs/ideas.md` entry may note a future
   explicit `dir=` import path.
3. **Neither (chosen).** `fetch_snapshot` is stdlib-only like the rest
   of `pylair.models`: `urllib` over the proven-TLS bundle (0009 §2),
   hashes through `hashlib`. The dependency-free runtime stays
   dependency-free.

**Offline behaviour**: identical to 0009. A verified snapshot resolves
in milliseconds with zero network. A missing or partial snapshot fails
with a precise `failed` detail naming the stage (`tree: …` versus the
file). Air-gapped deployment remains possible by copying the revision
directory plus its `.manifest.json`/`.verify.json` into place out of
band and writing `current`; `path()` accepts it because verification
is local.

**HF_TOKEN**: when the caller passes no `headers` and `HF_TOKEN` is
set in the runtime environment, the fetcher sends `Authorization:
Bearer <token>` as a convenience, matching hub-tooling behaviour.
Explicit `headers` always win. The token is in the sensitive set in
every case: stripped on any redirect origin change, never present in
logs, `failed` details, readiness reports or the manifest (0009 §6
rules apply unchanged).

## 6. Gated repos, mirrors, disk preflight

- **Gated/private repos**: exactly the `headers` surface of 0009. The
  redirect chain of a gated file (`huggingface.co` 302 to a CDN host)
  is the leak case 0009 §3 already defends: sensitive headers survive
  same-origin hops only. A 401/403 on the tree or a file is
  non-retryable and reports `failed(name, "tree: 401 unauthorized -
  check authorization for org/model")` (status and host, never the
  header).
- **Mirrors**: `HF_ENDPOINT` is honored when set (the hf-mirror.com
  pattern: tree and resolve URLs are built on the endpoint host
  instead of `huggingface.co`). Explicit `endpoint=` argument wins
  over the env; neither defaults anywhere but huggingface.co. The
  redirect rules still apply, so a mirror that redirects back to the
  real CDN cannot receive the token.
- **Disk preflight**: total bytes are the sum of manifest sizes,
  known exactly before anything downloads. `shutil.disk_usage` on the
  models dir, minus what is already verified or partial for this
  revision (resume must not double-count), then
  `failed(name, "not enough disk space: need X, have Y")` without
  touching the network, as in 0009 §3. Note the upgrade case: during
  a revision switch the disk holds the old revision plus the staging
  tree at once, so the preflight for an upgrade must account for the
  full new revision on top of the existing one - peak usage is
  old + new until the GC of §7 completes.

## 7. Updates and garbage collection

A new `revision` for the same `name` is an upgrade, and the design
answers two failure modes of the naive approach:

1. **Windows sharing violations**: safetensors mmaps weights, so a
   warm worker holding revision A open makes every rename or delete
   inside A fail. Any scheme that renames or deletes an in-use
   revision is broken on Windows by construction.
2. **Mixed-revision loads**: if `path()` returned a stable `<name>/`
   whose contents were swapped underneath, an app holding that `Path`
   would lazily read some files (tokenizer, a later shard, config)
   from the new revision while its in-memory weights came from the
   old one - a mixed-revision load inside one process, the same
   incoherence the full-sha rule of §1 exists to prevent.

The revision-addressed layout of §3 avoids both by construction:

- **Fetch**: the new revision downloads into
  `<name>/<revision>.partial/` while `<name>/current` still points at
  the old revision. Readers are undisturbed; the old revision keeps
  serving `path()` for the entire upgrade.
- **Complete**: `.ready` lands in staging, `.partial` is renamed to
  `<name>/<revision>/` - a directory nobody has open yet, so the
  rename cannot hit a sharing violation. Only then is `current`
  rewritten to the new sha with `os.replace` (small file, atomic on
  every OS). After the switch, new `path()` calls resolve the new
  revision; **a `Path` an app already holds keeps pointing at the old
  immutable revision for the life of the process** - no mixed loads,
  ever, because revision directories never change content.
- **Reader locks, and the lock-then-recheck protocol**: `path()`
  takes a shared byte-range lock on the revision's `.use` file and
  holds it for the life of the process (shared, not exclusive - any
  number of readers; kernel-released on death exactly like the 0009
  fetch lock). The ordering is load-bearing, because the sequence
  read-`current`-then-lock races with a switching upgrade plus a GC
  from another process: the reader reads A, the switch moves to B, GC
  wins A's lock before the reader takes it, and the reader would lock
  a revision already being deleted. So `path()` is a bounded retry
  loop:

  1. read `current` (or take the explicit `revision=` argument);
  2. open and shared-lock `<revision>/.use` - a failed open is a
     retry, not an error;
  3. **recheck**: `<revision>/.ready` must still exist, now that the
     lock is held. GC deletes `.ready` first (below), so a present
     `.ready` under a held shared lock proves the directory is not
     being deleted;
  4. `.ready` missing: release, re-read `current`, retry - bounded at
     3 attempts, then `ModelNotReady` with a detail naming the race
     (`revision switched during lookup; current is <sha>`).

  A reader that wins the shared lock with `.ready` present cannot end
  up in a half-deleted directory: the deletion either has not started
  (GC could not win the exclusive lock while any shared lock is held)
  or was never going to touch this revision. This is what lets GC
  distinguish "deletable" from "in use" on every OS, not just
  Windows.
- **GC**: at the end of a successful fetch, and again at each fetch
  start and runtime startup, the runtime scans `<name>/` for
  non-current revisions and tries to delete each one whose `.use`
  lock can be taken exclusively without blocking. A lock that cannot
  be taken means a live reader - skip silently. When the exclusive
  lock is won, deletion is ordered and the lock is held throughout:
  **`.ready` first, then the rest of the directory**. The `.ready`-
  first order is the other half of the reader protocol above: a
  reader that shared-locks `.use` after `.ready` is gone fails the
  recheck and retries onto `current`, and a revision with no `.ready`
  is never handed out - so even a reader that slips between the
  exclusive lock's release-to-scan windows lands on the new revision,
  never in a partial directory. On Windows a delete can also fail
  outright with a sharing violation even when the lock was won (a
  reader that mmap'd but never locked, or a race with a dying
  process): that failure is **"still in use", never an error** - the
  revision is retried at the next opportunity. `.partial` residue
  from crashed fetches is collected the same way (no reader can hold
  it, and `.lock` proves no live writer).
- **The newborn-revision window (implementation note, added with PR
  2)**: between the staging rename and the `current` write, a brand
  new revision is momentarily neither current nor reader-locked, and
  a concurrent GC could probe and delete it. The obvious fix -
  shared-locking `.use` inside staging before the rename so the lock
  travels with the directory - is impossible on Windows, where a
  directory cannot be renamed while any file inside it is open at any
  share mode. The implementation therefore closes the window from
  the other side: the fetch writes `current` through a completion
  hook while still holding `<name>/.lock` (the same lock the whole
  fetch is under), and GC skips a name entirely when `<name>/.lock`
  is contended - a strictly stronger form of the `.partial` rule
  above, since a live fetch always re-runs GC at its own start and
  end. This also means a fast-path repeat carries its `current`
  write into the locked flow instead of writing unlocked.
- **Disk usage honesty**: readers hold `.use` for the process
  lifetime, so a long-running app that has moved on to the new
  revision still pins the old one on disk until restart. Peak usage
  during an upgrade is therefore old + new (§6), and the old revision
  only leaves once every process holding it exits. That is the price
  of never corrupting a live load, and it is documented in
  `docs/models.md` so users can read their disk usage correctly.
- A repeated fetch of the revision `current` already names costs one
  stat pass and returns immediately, as in 0009.

## 8. Out of scope for v1

Explicitly not in v1, each a plausible `docs/ideas.md` entry:

- **xet protocol / chunk-level dedup** (verify by `lfs.oid` sha256
  regardless of backend; the xet chunk API is a bandwidth optimization,
  not an integrity requirement)
- **any `huggingface_hub` dependency or cache interop** (§5)
- **branch or tag revisions as fetch input** (§1; the resolve helper
  covers the authoring flow)
- **dataset repos** (`datasets/` API shape differs; model repos only)
- **cross-revision file reuse** (unchanged shards are re-downloaded in
  v1; correctness first, bandwidth optimization later - the
  content-addressed file layout makes this a safe later addition)
- **per-file parallel range download** (parallelism is across files,
  not within one file)
- **shared-machine caches** (per-app data dir only, as in 0009)
- **bandwidth caps, mirror fallback lists, auto-update-on-new-commit**
- **loading the snapshot** into torch/transformers/llama.cpp -
  `models` ends at a verified directory path

## 9. Test plan

- **Unit**: tree-response fixture with both file kinds; git-blob sha1
  framing verified against a real repo file whose `oid` is known;
  mixed snapshot download against a loopback server (one LFS-kind file,
  several small files, nested paths); allow/ignore filtering; manifest
  mode for `base_url` sources including the unverifiable-file error.
- **Atomicity**: crash injection between file moves proves no final
  path ever holds unverified bytes; `path()` never resolves a
  `.partial` dir; `.ready`-last ordering proven by deleting it and
  asserting `ModelNotReady`.
- **Revision immutability**: fetch revision A, hold the `Path` from
  `path()`, upgrade to revision B, assert the held `Path` still reads
  revision A's bytes for every file (tokenizer and config included)
  and only fresh `path()` calls see B. On Windows: same test with a
  memory-mapped shard held open across the switch - the switch must
  succeed because nothing renames A.
- **GC**: after the switch, A is deleted once no reader holds it;
  while a reader holds A's `.use` lock, GC skips it on every OS;
  a simulated Windows sharing-violation delete failure is treated as
  "still in use" and retried, never raised. A revision whose `.ready`
  GC deleted first is never handed out even if the rest of the
  directory still exists.
- **The path()/GC race**: interleave a reader between reading
  `current` and taking the shared lock with a switch plus an
  aggressive GC (another process). Across repeated interleavings the
  reader either lands on the old revision with `.ready` verified
  under its held lock - and the directory is then provably not being
  deleted - or fails the recheck, retries, and lands on the new
  revision. It never returns a `Path` into a partial directory, and
  the bounded retry surfaces `ModelNotReady` with the switch detail
  rather than hanging.
- **Resume**: kill mid-shard, restart, assert only missing bytes are
  re-fetched (server logs Range offsets); mtime-tamper a verified file
  and assert the single-file re-hash catches it.
- **Auth**: gated-fixture 302 to a second loopback origin asserts
  `Authorization` never crosses; `HF_TOKEN` adoption and explicit
  `headers` precedence; token absent from every error string.
- **Resolve helper**: `pylair models resolve org/model@main` against
  the loopback fixture prints the sha and rejects non-hex input to
  fetch with the exact argument error.
- **e2e**: `python -m pylair` plus fixture server, `requires` gating
  on the snapshot component, `path()` inside a worker raising
  `ModelNotReady` before readiness with zero network hits; an upgrade
  mid-run never flickers the gated component.

## 10. Implementation plan

1. **pylair PR 1 - fetch core**: tree/manifest resolution, the
   two-kind verifier, per-file download through the existing 0009
   machinery with fan-out 4, the revision-addressed layout
   (`.partial` staging, `.parts`, `.manifest.json`, `.verify.json`,
   `.ready`-last, completion rename), fetch locking, disk preflight
   including the old+new peak rule. Unit suite of §9.
2. **pylair PR 2 - lifecycle**: `path()` with `current` resolution
   and the `revision=` form, the `.use` reader locks, the atomic
   pointer switch, GC with in-use skipping, revision immutability
   guarantees, readiness wiring (component lifecycle, exact-byte
   progress, precise `failed`, no-flicker upgrades),
   `models.model_snapshot` declarative form, `HF_TOKEN`/`HF_ENDPOINT`.
   e2e and worker tests, including the Windows mmap-during-switch
   test.
3. **CLI/docs/selftest PR**: the `pylair models resolve` authoring
   helper in the dev CLI, `docs/models.md` snapshot section, README
   bullet, client selftest check mirroring the 0009 one, ideas.md
   entries for the §8 deferrals.

No wire change, no crate change, no client runtime change: the API
lives in `pylair` and reports through the existing readiness channel.
The npm trio moves only for the resolve helper and the selftest check;
the PyPI package carries the fetch itself.

## Alternatives considered

- **Stable `<name>/` path with a two-rename switch** (the original
  §7): renaming the current revision aside and the staged one into
  its place. Broken twice on Windows - safetensors mmaps make an
  in-use revision unrenamable and undeletable - and broken everywhere
  semantically: a held `Path` would straddle the swap, mixing
  old-revision weights in memory with new-revision lazy file reads in
  one process. Rejected in favor of revision-addressed storage where
  no rename ever touches an openable revision and a held `Path` is
  immutable by construction (§7).
- **Require authors to pin per-file hashes in app code**: exactly the
  burden §2 removes, and it cannot work for sharded repos whose file
  set changes across revisions. Rejected.
- **Branch revisions with resolve-then-pin at fetch time**: a
  moving-target read followed by a pin is still a moving target; the
  race is described in §1. Rejected. (Resolve at authoring time, pin
  in code - the supported flow.)
- **`huggingface_hub` as a dependency**: 15 to 25 MB of transitive
  dependencies including a compiled extension, duplicating machinery
  0009 already owns, with a cache layout that bypasses the readiness
  model. Rejected (§5).
- **Hub-cache interoperability**: private, version-unstable layout and
  cross-app trust. Rejected for v1 (§5).
- **Keep-one-old-revision for fast rollback**: the reader-lock GC
  already keeps an old revision alive exactly as long as it is in use;
  retaining it beyond that costs multi-GB duplication for a rollback
  that is a pinned re-fetch anyway. Rejected (§7).
- **Per-file range parallelism**: added complexity in resume and
  verification ordering for bandwidth already saturated by cross-file
  fan-out. Rejected for v1 (§8).
- **Symlink for `current`**: symlink creation needs elevated
  privileges on Windows; a small file plus `os.replace` is atomic
  everywhere with no privilege requirement. Rejected.

## Evidence index

- Live HF tree API response quoted in §2:
  `GET https://huggingface.co/api/models/hf-internal-testing/tiny-random-gpt2/tree/main`
  and the same query pinned at sha
  `71034c5d8bde858ff824298bdedc65515b97d2b9` with `recursive=true`
  (10 entries; LFS entries carry `lfs.oid` at 64 hex chars, plain
  entries `oid` at 40, both with exact `size`).
- Repo metadata endpoint:
  `GET https://huggingface.co/api/models/hf-internal-testing/tiny-random-gpt2`
  returns `sha` - the current default-branch commit, the lookup the
  resolve helper automates.
- git blob sha1 framing construction
  (`sha1("blob " + len + NUL + content)`) verified locally in the
  spike; the 40-char output matches the plain-file `oid` shape.
- 0009 §2: TLS from the bundled runtime proven on all three OSes, the
  reason no TLS or CA discussion is repeated here.
- 0009 §3/§5: download semantics, resume mechanics, kernel lock proof -
  inherited unchanged per file, and the same lock mechanism reused
  for the `.use` reader locks of §7.
- Windows file locking semantics: `MoveFile`/`DeleteFile` fail with
  `ERROR_SHARING_VIOLATION`/`ERROR_ACCESS_DENIED` on files open
  without delete sharing, and memory-mapped files (how safetensors
  readers load weights) are the canonical case - the motivation for
  revision-addressed storage in §7.
