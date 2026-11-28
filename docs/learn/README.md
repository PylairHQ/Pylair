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

## Jumping in without the repo

Each step folder is a complete standalone app. To fetch one without cloning the
whole repository, use git sparse checkout. This example fetches guide 1; change
the folder name for another step:

```sh
git clone --depth 1 --filter=blob:none --sparse https://github.com/PylairHQ/Pylair.git pylair-learn
cd pylair-learn
git sparse-checkout set examples/learn/01-first-app
cd examples/learn/01-first-app
npm install
uv sync
```

The step is then a normal app directory. You can run `npx pylair dev` there or
copy the folder somewhere else first.

## The path

| Guide | You build | Time |
|---|---|---:|
| [1. Your first app](01-first-app.md) | A note editor running in a desktop window | 20 min |
| [2. Real work](02-real-work.md) | Progress, cancellation, errors and warm workers | 25 min |
| [3. A local model](03-local-model.md) | Offline semantic search over notes | 25 min |
| [4. Shipping](04-shipping.md) | A native installer, signing choices and optional updates | 25 min |
| [5. Going further](05-going-further.md) | PDF export through a pinned binary, Blobs and next directions | 30 min |

The path runs from the first dev loop through packaging, updates, bundled
binaries and Blob results.

**If you only read one thing:** read
[1. Your first app](01-first-app.md). It shows the shape everything else uses.
