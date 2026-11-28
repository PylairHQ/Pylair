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

If anything is missing, run `npx pylair doctor`. A healthy run ends with:

```text
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

## Make the scaffold Inkwell

The starter template is a hello-world bridge. To build the same app this guide
explains, replace three scaffold files with the complete Inkwell versions
below. They are collapsed because they are complete files rather than focused
excerpts; copy each file as a whole.

First replace `backend/app.py`:

<details>
<summary>Complete <code>backend/app.py</code>: the note storage API</summary>

<!-- snippet: 01-first-app/backend/app.py -->
```python
"""Inkwell backend: local markdown notes exposed to the TypeScript UI."""

import os
import re
from dataclasses import dataclass
from datetime import UTC, datetime
from pathlib import Path

from pylair import expose

_WORDS = re.compile(r"[\w']+")
_NAME = re.compile(r"^[a-z0-9][a-z0-9-]{0,63}$")

SAMPLES = {
    "welcome": """# Welcome to Inkwell

Inkwell keeps plain markdown notes on this machine. Python owns the files;
the TypeScript UI asks for them through typed Pylair calls.

Try editing this note, saving it, and then changing `backend/app.py` while
`pylair dev` is running.
""",
    "ideas": """# Ideas

- Index every note and show progress while it happens.
- Rank notes with a small local embedding model.
- Ship the whole thing as one desktop app.
""",
}


@dataclass
class NoteSummary:
    name: str
    title: str
    words: int
    updated: str


@dataclass
class Note:
    name: str
    text: str
    words: int


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


def note_path(name: str) -> Path:
    if not _NAME.fullmatch(name):
        raise ValueError(f"invalid note name {name!r}")
    return notes_dir() / f"{name}.md"


def word_count(text: str) -> int:
    return len(_WORDS.findall(text))


def title_of(name: str, text: str) -> str:
    for line in text.splitlines():
        if line.startswith("# "):
            return line[2:].strip() or name
    return name.replace("-", " ").title()


def summarize(path: Path) -> NoteSummary:
    text = path.read_text(encoding="utf-8")
    updated = datetime.fromtimestamp(path.stat().st_mtime, tz=UTC).isoformat()
    return NoteSummary(
        name=path.stem,
        title=title_of(path.stem, text),
        words=word_count(text),
        updated=updated,
    )


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

</details>

Then replace `src/App.tsx`:

<details>
<summary>Complete <code>src/App.tsx</code>: the note editor UI</summary>

<!-- snippet: 01-first-app/src/App.tsx -->
```tsx
import { useEffect, useState } from "react";
import { listNotes, readNote, saveNote } from "./api.generated";

type Summary = Awaited<ReturnType<typeof listNotes>>[number];

export default function App() {
  const [notes, setNotes] = useState<Summary[]>([]);
  const [selected, setSelected] = useState("welcome");
  const [text, setText] = useState("");
  const [status, setStatus] = useState("loading notes...");

  const refresh = async () => {
    const summaries = await listNotes();
    setNotes(summaries);
    if (!summaries.some((note) => note.name === selected)) {
      setSelected(summaries[0]?.name ?? "welcome");
    }
  };

  useEffect(() => {
    refresh()
      .then(() => setStatus("ready"))
      .catch((error: unknown) =>
        setStatus(error instanceof Error ? error.message : String(error)),
      );
  }, []);

  useEffect(() => {
    readNote(selected)
      .then((note) => setText(note.text))
      .catch(() => setText(""));
  }, [selected]);

  const save = async () => {
    const summary = await saveNote(selected, text);
    setStatus(`saved ${summary.name} (${summary.words} words)`);
    await refresh();
  };

  return (
    <main className="app">
      <aside>
        <h1>Inkwell</h1>
        {notes.map((note) => (
          <button
            className={note.name === selected ? "note selected" : "note"}
            key={note.name}
            onClick={() => setSelected(note.name)}
          >
            <strong>{note.title}</strong>
            <small>{note.words} words</small>
          </button>
        ))}
      </aside>
      <section>
        <div className="toolbar">
          <span>{selected}.md</span>
          <button onClick={save}>Save</button>
        </div>
        <textarea value={text} onChange={(event) => setText(event.target.value)} />
        <p className="status">{status}</p>
      </section>
    </main>
  );
}
```

</details>

Finally replace `src/app.css`:

<details>
<summary>Complete <code>src/app.css</code>: the editor layout</summary>

<!-- snippet: 01-first-app/src/app.css -->
```css
body {
  margin: 0;
  color: #1f2933;
  background: #f8fafc;
  font-family: Inter, ui-sans-serif, system-ui, sans-serif;
}

button {
  font: inherit;
}

.app {
  display: grid;
  grid-template-columns: 260px 1fr;
  height: 100vh;
}

aside {
  box-sizing: border-box;
  padding: 24px;
  background: #e8eef5;
  overflow: auto;
}

h1 {
  margin: 0 0 20px;
  font-size: 24px;
}

.note {
  display: flex;
  width: 100%;
  flex-direction: column;
  align-items: flex-start;
  gap: 4px;
  margin-bottom: 8px;
  padding: 10px;
  border: 1px solid transparent;
  border-radius: 8px;
  background: transparent;
  cursor: pointer;
  text-align: left;
}

.note.selected,
.note:hover {
  border-color: #8aa4c2;
  background: white;
}

.note small {
  color: #52606d;
}

section {
  display: flex;
  min-width: 0;
  flex-direction: column;
  padding: 24px;
  box-sizing: border-box;
}

.toolbar {
  display: flex;
  align-items: center;
  justify-content: space-between;
  margin-bottom: 12px;
}

.toolbar button {
  padding: 7px 18px;
  border: 0;
  border-radius: 6px;
  color: white;
  background: #2563eb;
  cursor: pointer;
}

textarea {
  flex: 1;
  box-sizing: border-box;
  padding: 16px;
  resize: none;
  border: 1px solid #bcccdc;
  border-radius: 8px;
  font: 15px/1.5 ui-monospace, SFMono-Regular, Consolas, monospace;
}

.status {
  min-height: 20px;
  color: #52606d;
}
```

</details>

The rest of the scaffold stays unchanged. The sections below explain what these
files do.


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
