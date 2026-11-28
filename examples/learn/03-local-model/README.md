# Inkwell after guide 3

This is the complete state of the Inkwell example app at the end of
[guide 3](../../../docs/learn/03-local-model.md). Compared with step 2 it adds
a pinned Hugging Face snapshot, readiness gating and local semantic search in
a warm worker.

The model is `blobbybob/potion-mxbai-micro` at revision
`4b7425b7053e9976684c939bb1715f718d75b175`. It is a 700 KB quantized static
embedding model, small enough for a tutorial and fast enough for CPU search.

Run it from a fresh checkout with:

```sh
npm install
uv sync
npx pylair dev
```
