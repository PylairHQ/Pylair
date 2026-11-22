# reference-app

The Pylair validation app (docs/maintainers/PROJECT_BRIEF.md §8) and the end-to-end
suite's target. It demonstrates, through the real bridge:

- **token-by-token streaming** — `chat()` streams deltas from a local
  Ollama model (`pylair-ollama`); `demo_stream()` runs the identical
  streaming+cancel path with no model server so every control works on
  clean machines;
- **safe cancellation** — the cancel button aborts the JS side, the
  supervisor cancels the Python task, and `pylair_ollama`'s httpx
  lifecycle releases the connection;
- **crash recovery** — `crash()` kills the Python process on purpose;
  the state badge shows `ready → restarting → ready` as the supervisor
  restarts it;
- **clean-machine packaging** — `pylair build` bundles the interpreter
  and deps; CI installs the result on all three OSes with no system
  Python and drives `add`/`count` through the packaged webview
  (`PYLAIR_SELF_TEST_WEBVIEW`).

## Run

```sh
pnpm install
uv sync
pnpm exec pylair dev
```

For real chat: `ollama serve` and `ollama pull qwen2.5:0.5b` (or set
`REFERENCE_MODEL`). Without Ollama the UI still works — use
**demo stream**.

Deps are repo-relative paths on purpose: this example lives inside the
pylair checkout. Apps from `create-pylair` use published versions.
