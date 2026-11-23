# pylair

The Python side of [Pylair](https://github.com/PylairHQ/Pylair), a desktop
framework for Python and local-AI applications. This package is the
runtime that lives inside a Pylair desktop app: it hosts your exposed
functions, runs the length-prefixed JSON-RPC protocol over stdio, and
reports health and cancellation state to the Rust supervisor.

You don't run it directly — `pylair` (the CLI) spawns it. In your app:

```python
from pylair import expose

@expose
def add(a: int, b: int) -> int:
    return a + b

@expose
async def chat(prompt: str):
    async for token in my_model.stream(prompt):
        yield token
```

`@expose`d functions become typed calls and streams on the TypeScript
side (`@pylair/client`). Streaming is a `yield`-producing function;
cancellation is cooperative.

`pylair.Blob` returns large binary payloads (generated media, model
output, file contents) as a host-served `pylair-blob` URL instead of
base64 in the frame — `Blob(data, mime=…)` for bytes,
`Blob.from_path(path, mime=…)` for a zero-copy file. The TypeScript side
receives a `BlobHandle` with `.url`, ranged `stream()` and `release()`.
See `docs/blobs.md` in the Pylair repo.

- Scaffolding: `npx create-pylair my-app`
- Dev: `pylair dev` — supervised runtime, hot reload
- Build: `pylair build` — bundles CPython + your deps into a native
  installer; end users need no Python install

Requires Python 3.12+.

Pylair builds on [Tauri](https://tauri.app) but is not affiliated with or
endorsed by the Tauri project.

License: MIT OR Apache-2.0.
