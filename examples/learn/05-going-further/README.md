# Inkwell after guide 5

This is the complete state of the Inkwell example app at the end of
[guide 5](../../../docs/learn/05-going-further.md). Compared with step 4 it
adds PDF export through a declared, hash-verified Tectonic binary, with
cancellation and a Blob result.

Run it from a fresh checkout with:

```sh
npm install
uv sync
npx pylair dev
```

The first dev run or build downloads the pinned Tectonic archive for the host
platform into `.pylair/cache/binaries/`.
