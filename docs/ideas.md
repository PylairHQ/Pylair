# Ideas (not in v0.1 scope)

Parking lot for ideas that come up during development. Do not implement
from this file — promote to a milestone or a decision doc first.

- PyInstaller as an opt-in alternative packaging backend
  (evaluate after M0 findings; docs/maintainers/PROJECT_BRIEF.md section 4).
- Bundled inference runtime (llama.cpp or similar) instead of requiring
  external Ollama — owner question, section 12.
- Unix socket / Windows named pipe transport behind the transport trait.
- Svelte template (`packages/create-pylair/templates/svelte-ts`).
- Temp-file-handle transport for bulk binary payloads (images, audio)
  to replace base64-in-JSON (+33% wire cost) — protocol note §2.
- `{"$bigint": "<decimal>"}` tagging for Python ints beyond ±2^53 —
  emitted as `bigint` in generated TS. Until then the codec rejects
  out-of-range ints with a clear error (docs/codegen.md): annotate
  `str` for exact big integers.
