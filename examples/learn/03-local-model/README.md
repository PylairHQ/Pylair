# Inkwell after guide 3

This is the complete state of the Inkwell example app at the end of
[guide 3](../../../docs/learn/03-local-model.md). Compared with step 2 it adds
a pinned Hugging Face snapshot, readiness gating and local semantic search in
a warm worker.

The model is `minishlab/potion-base-2M` at revision
`389b9f64be5aa4ae7a6bc6fe95ef20ce485ae5da`. It is an 8 MB static
embedding model, small enough for a tutorial and fast enough for CPU search.

Run it from a fresh checkout with:

```sh
npm install
uv sync
npx pylair dev
```
