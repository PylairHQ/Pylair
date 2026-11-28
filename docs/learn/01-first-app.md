# 1. Your first app

**What you'll build:** Inkwell, a small markdown note editor running in a
desktop window. Python owns the notes directory, and the React UI calls Python
through typed functions.

**Time:** about 20 minutes, plus a few minutes for the first Rust compile.

## Create the app

Pick a fresh directory and scaffold the app:

```sh
npm create pylair inkwell
cd inkwell
npm install
uv sync
```

When the scaffolder asks, choose **a new UI that calls Python** and then **no**
for auto-update. Auto-update is a later topic; this first app does not need a
signing key. The scaffold prints the next commands:

```text
Created inkwell/ (bridge)

Next:
  cd inkwell
  npm install         # or pnpm install
  uv sync             # python env (https://docs.astral.sh/uv/)
  npx pylair dev      # supervised dev: python hot reload + vite
                      # (pnpm users: pnpm pylair dev / pnpm pylair build)
  npx pylair build    # native installer with bundled python
  npx pylair doctor   # diagnose environment any time
  Start here: https://github.com/PylairHQ/Pylair/blob/main/docs/learn/01-first-app.md
```

If anything is missing, run `npx pylair doctor`. On the machine used for this
guide it ended with:

```text
 ok  versions: compatibility        a compatible set (npm-v0.1.0-rc.36: crate 0.1.0-rc.28, pylair 0.1.0rc21) - the ecosystems version independently, so differing numbers are expected, not skew
 ok  ollama (optional)              5 model(s) available

all checks passed
```

## The layout

The scaffold is small on purpose:

| Path | What it is for |
|---|---|
| `backend/` | Your Python package. `backend/app.py` starts with the exposed API. |
| `src/` | The React front end. `src/api.generated.ts` will appear here. |
| `src-tauri/` | The native shell: Tauri configuration, Rust glue, icons and capabilities. |
| `pyproject.toml` | Python metadata plus `[tool.pylair]`, the app module and source directory. |
| `requirements.txt` | Locked Python dependencies bundled into installers. |
| `package.json` | Scripts and npm dependencies, including `@pylair/client` and `@pylair/cli`. |
| `index.html`, `vite.config.ts`, `tsconfig.json` | The ordinary Vite front-end setup. |

You do not need to read the Rust code for these guides. Treat it as the shell
that starts and supervises Python.

## Expose Python functions

Inkwell stores notes as markdown files. It creates two sample notes on first
use, in the app data directory when the app runs under Pylair and in
`~/.inkwell` when you experiment outside it.

<!-- snippet: 01-first-app/backend/app.py -->
```python
def notes_dir() -> Path:
    """Return the notes directory, creating sample notes on first use."""
    root = Path(os.environ.get("PYLAIR_APP_DATA_DIR", Path.home() / ".inkwell"))
    directory = root / "notes"
    directory.mkdir(parents=True, exist_ok=True)
    for name, text in SAMPLES.items():
        path = directory / f"{name}.md"
        if not path.exists():
            path.write_text(text, encoding="utf-8")
    return directory
```

Names are deliberately restricted to lowercase letters, numbers and hyphens.
That keeps a note name a safe file stem rather than a path.

The public API is three functions. `@expose` is the whole bridge declaration:
no schema file and no handwritten JSON conversion.

<!-- snippet: 01-first-app/backend/app.py -->
```python
@expose
def list_notes() -> list[NoteSummary]:
    """List markdown notes, newest first."""
    return sorted(
        (summarize(path) for path in notes_dir().glob("*.md")),
        key=lambda note: note.updated,
        reverse=True,
    )


@expose
def read_note(name: str) -> Note:
    """Read one note by its file stem."""
    path = note_path(name)
    if not path.exists():
        raise FileNotFoundError(f"unknown note {name!r}")
    text = path.read_text(encoding="utf-8")
    return Note(name=name, text=text, words=word_count(text))


@expose
def save_note(name: str, text: str) -> NoteSummary:
    """Save one note and return its fresh summary."""
    path = note_path(name)
    path.write_text(text, encoding="utf-8")
    return summarize(path)
```

The dataclasses in the annotations matter. `NoteSummary` and `Note` become
TypeScript interfaces, and their fields are checked on the wire. The details
are in [the codegen reference](../codegen.md).

## Call Python from the UI

`pylair dev` generates `src/api.generated.ts` and keeps it current. You can
also run the same step yourself:

```sh
npx pylair codegen --module backend.app --out src/api.generated.ts
```

```text
pylair codegen: wrote src/api.generated.ts (3 functions)
```

The generated file is ephemeral and gitignored. Import from it like any other
TypeScript module:

<!-- snippet: 01-first-app/src/App.tsx -->
```tsx
import { useEffect, useState } from "react";
import { listNotes, readNote, saveNote } from "./api.generated";

type Summary = Awaited<ReturnType<typeof listNotes>>[number];
```

The editor state is ordinary React state. The Pylair part is that `listNotes`,
`readNote` and `saveNote` are Python calls with TypeScript types:

<!-- snippet: 01-first-app/src/App.tsx -->
```tsx
  const refresh = async () => {
    const summaries = await listNotes();
    setNotes(summaries);
    if (!summaries.some((note) => note.name === selected)) {
      setSelected(summaries[0]?.name ?? "welcome");
    }
  };
```

<!-- snippet: 01-first-app/src/App.tsx -->
```tsx
  const save = async () => {
    const summary = await saveNote(selected, text);
    setStatus(`saved ${summary.name} (${summary.words} words)`);
    await refresh();
  };
```

Notice that Python's `list_notes` became `listNotes`. Codegen uses camel case
in TypeScript while preserving the Python wire name inside the stub.

## Run it

Start the supervised dev loop:

```sh
npx pylair dev
```

The first run compiles the Rust/Tauri side, so it can take several minutes.
Later runs reuse the cache. The useful lines look like this:

```text
pylair: watching python source; starting tauri dev
pylair: close the window or press Ctrl+C to stop
  VITE v7.3.8  ready in 296 ms
  ➜  Local:   http://localhost:1420/
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.64s
     Running `target\debug\inkwell.exe`
```

A desktop window opens with two sample notes. Select **Ideas**, edit the text
and press **Save**. The status line reports the fresh word count returned by
Python.

## Edit Python and watch the reload

Leave `pylair dev` running, then edit `backend/app.py`. For example, change
one sentence in the welcome note. When the file is saved, the terminal prints
lines like these:

```text
reload: app.py changed, restarting Python
reload: ready in 571ms (generation 1)
```

The first line means the watcher saw the Python change and restarted the
runtime. The second means the new generation was ready to serve calls. The
window stays open; only Python restarted.

If Python has a syntax error, the reload line tells you startup failed and the
next save retries. `npx pylair dev --inspect` gives you a live timeline for
these events; guide 2 uses it when calls become more interesting.

## What you learned

> - `@expose` makes a Python function callable from TypeScript.
> - Type annotations and dataclasses become the generated client contract.
> - `pylair dev` runs uv sync, codegen, Vite, the native shell and Python hot
>   reload together.
> - `src/api.generated.ts` is generated, gitignored and safe to delete.
>
> **Next:** [guide 2](02-real-work.md) adds progress, cancellation,
> structured errors and warm workers. The complete state for this guide is in
> [`examples/learn/01-first-app/`](../../examples/learn/01-first-app/).
