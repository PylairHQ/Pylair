# Learn Pylair

A hands-on path for building a Python powered desktop app, one working step at
a time.

## Who this is for

You know some Python and a little TypeScript or React. You do not need to know
Pylair, Tauri or Rust. Each guide ends with something running and links to the
reference pages for depth instead of repeating them.

## Prerequisites

Install the tools listed in the [root README](../../README.md#prerequisites):
Node 22 or newer, `uv`, Rust and the native toolchain for your OS. Then check
the machine once with:

```sh
npx pylair doctor
```

## The example app

The path builds **Inkwell**, a small local markdown note tool. A document tool
is a useful shape for learning because each step has a natural reason to exist:

1. Python owns local files while the React UI edits them.
2. Indexing notes is real work with progress and cancellation.
3. Semantic search motivates a small local model.
4. Notes are the kind of app people want to install and keep offline.
5. Search and document conversion motivate bundled command line tools later.

The complete state after each guide lives in
[`examples/learn/`](../../examples/learn/). You can start at the beginning, jump
into any step, or diff two step folders to see exactly what changed.

## The path

| Guide | You build | Time |
|---|---|---:|
| [1. Your first app](01-first-app.md) | A note editor running in a desktop window | 20 min |
| [2. Real work](02-real-work.md) | Progress, cancellation, errors and warm workers | 25 min |
| [3. A local model](03-local-model.md) | Offline semantic search over notes | 25 min |

This first installment covers the development loop through local models.
Packaging, auto-update and bundled binaries are the next installment.

**If you only read one thing:** read
[1. Your first app](01-first-app.md). It shows the shape everything else uses.
