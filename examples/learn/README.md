# Inkwell learning-path examples

Inkwell is the running example for the [learning path](../../docs/learn/README.md).
It is a small local markdown note tool: Python owns the notes directory, the
React UI edits notes, indexing streams progress, and a tiny local embedding
model ranks notes offline.

This shape was chosen because it motivates each guide without invented demo
machinery. Local files make the first bridge calls useful, indexing makes
progress and cancellation visible, semantic search makes a small model useful
on CPU, and the finished tool is the kind of app someone would realistically
install.

Each folder is the complete state at the end of one guide:

- [`01-first-app/`](01-first-app/): exposed note functions and a working editor
- [`02-real-work/`](02-real-work/): streaming progress, cancellation, structured
  errors and a warm worker
- [`03-local-model/`](03-local-model/): a pinned Hugging Face snapshot and local
  semantic search

To compare steps, diff consecutive folders, for example:

```sh
diff -ru 01-first-app 02-real-work
```

Every step is a standalone app. From inside one folder:

```sh
npm install
uv sync
npx pylair dev
```

The generated TypeScript client, virtual environments, model data and build
outputs are intentionally not committed.
