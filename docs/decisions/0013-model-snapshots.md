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
- The cost is one lookup at authoring time (`GET /api/models/<repo>`
  returns `sha`, the current default-branch commit). Release tooling
  can automate it; the docs will show the one-liner.

Rejected alternatives:

- **Branch names allowed with a warning**: the warning trains users to
  ignore it, and the race above remains. Rejected.
- **Branch resolved to a sha at fetch time, then pinned**: this is
  strictly the race above with extra steps. The resolution read is
  still a moving-target read. Rejected.
- **Tags**: mutable on Hugging Face (they are git refs, not immutable
  release objects). Same problem, rejected.
- **Optional revision defaulting to the default branch**: the worst
  option, since it makes the easy path the unreproducible one.
  Rejected; `revision` is required, not defaulted.

`models.path(name)` returns the snapshot **directory** `Path` when the
snapshot verifies ready (§3), else `ModelNotReady` exactly as in 0009.
In a worker process it is resolve-and-check only, same rule.

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
to mirror), and every file is required to appear in the manifest  - 
a file present in `allow` but missing from the manifest is a loud
error at argument-check time, since unverifiable bytes must never
enter the snapshot. `base_url` and `manifest` are mutually exclusive
with `repo`/`revision`.

## 3. Storage layout, atomic readiness, locking

```
<app data dir>/models/<name>/
    <repo files at their repo-relative paths>   # the snapshot itself
    .parts/                 # in-flight: .parts/<path>.part per file
    .manifest.json          # the pinned tree, written before downloads start
    .verify.json            # per-file verification record (size, mtime_ns)
    .lock                   # cross-process lock, byte-range locked as in 0009
    .ready                  # ready marker: {revision, file_count, total_bytes}
```

The rules:

- **A file only enters the snapshot verified.** Downloads write to
  `.parts/<path>.part`; on completion the file's hash is checked
  against the manifest, and only then is it moved into place with
  `os.replace` (same filesystem, since `.parts/` sits inside the
  snapshot dir). A half-downloaded or unverified file never exists at
  its final path.
- **`.ready` is written last**, after every manifest file is in place
  and verified, and records the revision, file count and total bytes.
  It is the atomic commit point of the whole snapshot: a crash at any
  earlier moment leaves no `.ready`, so `path()` never returns a
  half-finished snapshot.
- **`path()` cheap-check**: `.ready` present, `.manifest.json` present,
  every manifest entry exists with the recorded size, and no recorded
  mtime changed. One stat per file (typically tens of files, tens of
  microseconds each) with zero hashing; any mtime mismatch re-hashes
  just that file before deciding, mirroring 0009 §1. A failed check
  falls through to resume, never to a wrong `Path`.
- **Lock**: `.lock` byte-range locked exactly as in 0009 §5 (kernel
  lock, released on holder death, no stale cleanup). A second caller
  or process spins, reads progress from `.parts` sizes plus verified
  file sizes against the manifest totals, and reports
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
- **Precise failure**: `failed(name, "<path>: <reason>")`  - 
  `model-00002-of-00004.safetensors: sha256 mismatch: expected … got
  …` or `tokenizer.json: connection reset after 3 retries`. The file
  is always named; a snapshot-level reason (disk space, tree fetch
  failure) names the stage instead: `tree: 404 for org/model@<sha>`.
- Cancellation, thread placement and the declarative
  `models.model_snapshot(name, ...)` module-level form mirror 0009 §5
  exactly, including the auto-start-after-hello rule and the sweep
  hint. Workers stay leaf executors: `path()` resolves, never fetches.

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
file). Air-gapped deployment remains possible by copying the snapshot
directory plus `.manifest.json`/`.verify.json` into place out of band;
`path()` accepts it because verification is local.

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
  non-retryable and reports `failed(name, "tree: 401 unauthorized  - 
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
  snapshot (resume must not double-count), then
  `failed(name, "not enough disk space: need X, have Y")` without
  touching the network, as in 0009 §3.

## 7. Updates and garbage collection

A new `revision` for the same `name` is an upgrade, and upgrades are
atomic per snapshot:

- `<data>/models/<name>/` always holds **the current revision only**,
  keyed inside `.ready`. A fetch for a different revision downloads
  into a sibling staging dir `<name>.next-<revision>/` with the same
  internal layout. The current snapshot keeps serving `path()` the
  whole time; nothing about the in-progress upgrade is visible to app
  code.
- When every staged file verifies, `.ready` is written in staging and
  the switch is two renames: current aside to `<name>.old-<revision>`,
  staging into `<name>`. Both renames are same-directory and atomic;
  the only inconsistent instant is between them, and `path()` treats a
  missing `.ready` as not-ready, so the worst case is a transient
  `ModelNotReady`, never corrupt bytes. (A single `os.replace` cannot
  swap a non-empty dir atomically on Windows; the two-rename dance is
  the honest approximation and its failure mode is safe.)
- **GC**: after a successful switch the old revision directory is
  deleted on the spot. Rollback-by-revision is not retained locally:
  revisions are immutable upstream, so a rollback is a re-fetch of a
  pinned sha, and holding multi-GB copies of every previous revision
  is the worst trade on a user machine. `.part` residue and staging
  dirs from crashed upgrades are collected at the start of the next
  fetch for that name (they are detectable by naming, and `.lock`
  proves no live writer).
- A repeated fetch of the same revision the snapshot already verifies
  costs one stat pass and returns immediately, as in 0009.

## 8. Out of scope for v1

Explicitly not in v1, each a plausible `docs/ideas.md` entry:

- **xet protocol / chunk-level dedup** (verify by `lfs.oid` sha256
  regardless of backend; the xet chunk API is a bandwidth optimization,
  not an integrity requirement)
- **any `huggingface_hub` dependency or cache interop** (§5)
- **branch or tag revisions** (§1)
- **dataset repos** (`datasets/` API shape differs; model repos only)
- **delta sync between revisions** (unchanged files are not re-used
  across revisions in v1; the GC story deliberately keeps one revision)
- **per-file parallel range download** (parallelism is across files,
  not within one file)
- **shared-machine caches** (per-app data dir only, as in 0009)
- **bandwidth caps, mirror fallback lists, auto-update-on-new-commit**
- **loading the snapshot** into torch/transformers/llama.cpp  - 
  `models` ends at a verified directory path

## 9. Test plan

- **Unit**: tree-response fixture with both file kinds; git-blob sha1
  framing verified against a real repo file whose `oid` is known;
  mixed snapshot download against a loopback server (one LFS-kind file,
  several small files, nested paths); allow/ignore filtering; manifest
  mode for `base_url` sources including the unverifiable-file error.
- **Atomicity**: crash injection between file moves proves no final
  path ever holds unverified bytes; missing `.ready` means `path()`
  raises; the two-rename switch never serves half of either revision.
- **Resume**: kill mid-shard, restart, assert only missing bytes are
  re-fetched (server logs Range offsets); mtime-tamper a verified file
  and assert the single-file re-hash catches it.
- **Auth**: gated-fixture 302 to a second loopback origin asserts
  `Authorization` never crosses; `HF_TOKEN` adoption and explicit
  `headers` precedence; token absent from every error string.
- **GC**: staged upgrade switch, old revision gone, crash mid-switch
  recovered at next fetch.
- **e2e**: `python -m pylair` plus fixture server, `requires` gating
  on the snapshot component, `path()` inside a worker raising
  `ModelNotReady` before readiness with zero network hits.

## 10. Implementation plan

1. **pylair PR 1 - fetch core**: tree/manifest resolution, the
   two-kind verifier, per-file download through the existing 0009
   machinery with fan-out 4, `.parts` layout, locking, disk preflight,
   the storage layout of §3. Unit suite of §9.
2. **pylair PR 2 - snapshot lifecycle**: `path()` cheap-check,
   `.ready` atomicity, revision switch + GC, readiness wiring
   (component lifecycle, exact-byte progress, precise `failed`),
   `models.model_snapshot` declarative form, `HF_TOKEN`/`HF_ENDPOINT`.
   e2e and worker tests.
3. **Docs/selftest PR**: `docs/models.md` snapshot section, README
   bullet, client selftest check mirroring the 0009 one, ideas.md
   entries for the §8 deferrals.

No wire change, no crate change, no client runtime change: the API
lives in `pylair` and reports through the existing readiness channel,
so only the PyPI package moves, plus the npm trio for the selftest
check as in 0009 §9.

## Alternatives considered

- **Require authors to pin per-file hashes in app code**: exactly the
  burden §2 removes, and it cannot work for sharded repos whose file
  set changes across revisions. Rejected.
- **Branch revisions with resolve-then-pin**: a moving-target read
  followed by a pin is still a moving target; the race is described in
  §1. Rejected.
- **`huggingface_hub` as a dependency**: 15 to 25 MB of transitive
  dependencies including a compiled extension, duplicating machinery
  0009 already owns, with a cache layout that bypasses the readiness
  model. Rejected (§5).
- **Hub-cache interoperability**: private, version-unstable layout and
  cross-app trust. Rejected for v1 (§5).
- **Keep-one-old-revision rollback retention**: multi-GB duplication
  on user machines for a rollback that is a pinned re-fetch anyway.
  Rejected; GC deletes at switch (§7).
- **Per-file range parallelism**: added complexity in resume and
  verification ordering for bandwidth already saturated by cross-file
  fan-out. Rejected for v1 (§8).
- **Directory `os.replace` atomic switch**: not atomic for non-empty
  directories on Windows; the two-rename dance with the safe failure
  mode is the approximation chosen instead (§7).

## Evidence index

- Live HF tree API response quoted in §2:
  `GET https://huggingface.co/api/models/hf-internal-testing/tiny-random-gpt2/tree/main`
  and the same query pinned at sha
  `71034c5d8bde858ff824298bdedc65515b97d2b9` with `recursive=true`
  (10 entries; LFS entries carry `lfs.oid` at 64 hex chars, plain
  entries `oid` at 40, both with exact `size`).
- Repo metadata endpoint:
  `GET https://huggingface.co/api/models/hf-internal-testing/tiny-random-gpt2`
  returns `sha` - the current default-branch commit, the authoring-time
  lookup for a pinnable revision.
- git blob sha1 framing construction
  (`sha1("blob " + len + NUL + content)`) verified locally in the
  spike; the 40-char output matches the plain-file `oid` shape.
- 0009 §2: TLS from the bundled runtime proven on all three OSes, the
  reason no TLS or CA discussion is repeated here.
- 0009 §3/§5: download semantics, resume mechanics, kernel lock proof  - 
  inherited unchanged per file.
