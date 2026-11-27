# Ideas (not in v0.1 scope)

Parking lot for ideas that come up during development. Do not implement
from this file — promote to a milestone or a decision doc first.

- PyInstaller as an opt-in alternative packaging backend
  (evaluate after M0 findings; docs/maintainers/PROJECT_BRIEF.md section 4).
- Bundled inference runtime (llama.cpp or similar) instead of requiring
  external Ollama — owner question, section 12.
- Unix socket / Windows named pipe transport behind the transport trait.
- Svelte template (`packages/create-pylair/templates/svelte-ts`).
- Snapshot xet protocol / chunk-level dedup for Hugging Face transfers
  (decision 0013 §8: verification by `lfs.oid` sha256 is backend-independent;
  the xet chunk API is bandwidth optimization).
- Snapshot cross-revision file reuse: unchanged shards are re-downloaded in
  v1; the content-addressed layout makes reuse a safe later addition
  (decision 0013 §8).
- Per-file parallel range download within one snapshot file (parallelism is
  across files today).
- Shared-machine model caches across apps (per-app data dir only, as in 0009).
- Dataset repos in `pylair.models` snapshots (the `datasets/` API shape
  differs; model repos only in v1).
- Loading snapshots into inference frameworks (torch/transformers/llama.cpp)
  behind a helper API — `pylair.models` ends at a verified directory path.
- Windows sidecar tree-kill via Job Object: `taskkill /T` walks live parent
  links only, so a grandchild whose own parent already exited survives a
  cancel (documented in docs/binaries.md). Assigning sidecar children to a
  Job Object with KILL_ON_JOB_CLOSE closes the gap; POSIX killpg does not
  have it (decision 0014 section 4).
- Harden the NSIS update-acceptance scenario c probe: it still waits on the
  fixture's lossy boot-line count, the flake class scenario e already fixed
  by switching to product evidence (boots/*.json / update.state). Scenario c
  needs the same treatment for the restored-v2 boot (observed as a 300s
  timeout flake in PR #212's CI).
