# 2. Real work: progress, cancellation and errors

**What you'll build:** Inkwell gains a scan button that streams progress, a
cancel button that really stops the scan, useful error display, and keyword
analysis inside a warm worker process.

**Time:** about 25 minutes.

Start from your guide 1 app, or fetch
[`examples/learn/02-real-work/`](../../examples/learn/02-real-work/) with the
[sparse-checkout command](README.md#jumping-in-without-the-repo) and run
`npm install` and `uv sync` inside it.

## Update the app

If you are continuing from guide 1, replace `backend/app.py`, `src/App.tsx`
and `src/app.css` with these complete guide 2 versions. If you fetched this
step directly, the files are already in this state. Copy each file as a whole;
the excerpts later in the guide are for explanation.

Replace `backend/app.py`:

<details>
<summary>Complete <code>backend/app.py</code>: streams, errors and a warm worker</summary>

<!-- snippet: 02-real-work/backend/app.py -->
```python
"""Inkwell backend: local markdown notes exposed to the TypeScript UI."""

import asyncio
import os
import re
from collections import Counter
from collections.abc import AsyncIterator
from dataclasses import dataclass
from datetime import UTC, datetime
from pathlib import Path

from pylair import expose, warm

_WORDS = re.compile(r"[\w']+")
_NAME = re.compile(r"^[a-z0-9][a-z0-9-]{0,63}$")
_STOPWORDS = {
    "about", "after", "again", "because", "before", "being", "could",
    "every", "from", "have", "into", "just", "like", "more", "over",
    "some", "than", "that", "their", "them", "then", "there", "these",
    "they", "this", "through", "when", "while", "with", "would", "your",
}

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
    "projects": """# Projects

## Desktop notes

A local-first note app should open quickly, index in the background and
keep working when the machine is offline.

## Weekend bread

Flour, water, salt and time. The dough improves with patience.
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


@dataclass
class ScanProgress:
    done: int
    total: int
    note: str
    words: int
    headings: int
    links: int


@dataclass
class NoteAnalysis:
    name: str
    keywords: list[str]
    longest_line: int


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


def read_text_note(name: str) -> str:
    path = note_path(name)
    if not path.exists():
        raise FileNotFoundError(f"unknown note {name!r}")
    return path.read_text(encoding="utf-8")


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
    text = read_text_note(name)
    return Note(name=name, text=text, words=word_count(text))


@expose
def save_note(name: str, text: str) -> NoteSummary:
    """Save one note and return its fresh summary."""
    path = note_path(name)
    path.write_text(text, encoding="utf-8")
    return summarize(path)


@expose
async def scan_notes() -> AsyncIterator[ScanProgress]:
    """Scan every note, yielding one progress item per file."""
    paths = sorted(notes_dir().glob("*.md"))
    total = len(paths)
    for index, path in enumerate(paths, start=1):
        text = path.read_text(encoding="utf-8")
        yield ScanProgress(
            done=index,
            total=total,
            note=path.stem,
            words=word_count(text),
            headings=sum(1 for line in text.splitlines() if line.startswith("#")),
            links=text.count("](") + text.count("https://"),
        )
        # The pause keeps progress visible for the tiny sample corpus. Real
        # indexing work usually yields because parsing itself takes time.
        await asyncio.sleep(0.25)


@expose(worker=warm(pool=1))
def analyze_note(name: str) -> NoteAnalysis:
    """Extract simple keywords in a warm worker process."""
    text = read_text_note(name)
    words = [word.lower() for word in _WORDS.findall(text)]
    useful = [word for word in words if len(word) > 3 and word not in _STOPWORDS]
    keywords = [word for word, _count in Counter(useful).most_common(5)]
    longest_line = max((len(line) for line in text.splitlines()), default=0)
    return NoteAnalysis(name=name, keywords=keywords, longest_line=longest_line)
```

</details>

Replace `src/App.tsx`:

<details>
<summary>Complete <code>src/App.tsx</code>: scan, cancel, analyze and error display</summary>

<!-- snippet: 02-real-work/src/App.tsx -->
```tsx
import { useEffect, useRef, useState } from "react";
import { isCancelled, PythonError } from "@pylair/client";
import {
  analyzeNote,
  listNotes,
  readNote,
  saveNote,
  scanNotes,
} from "./api.generated";

type Summary = Awaited<ReturnType<typeof listNotes>>[number];
type ScanProgress = Awaited<ReturnType<typeof scanNotes>> extends AsyncIterable<infer T>
  ? T
  : never;

function describeError(error: unknown): string {
  if (error instanceof PythonError) {
    const parts = [error.message];
    for (const detail of error.details) {
      parts.push(`${detail.path}: expected ${detail.expected}, got ${detail.received}`);
    }
    if (error.suggestions.length > 0) {
      parts.push(`did you mean ${error.suggestions.join(", ")}?`);
    }
    if (error.traceback) {
      parts.push("Python traceback printed in the console");
      console.error(error.traceback);
    }
    return parts.join(" · ");
  }
  return error instanceof Error ? error.message : String(error);
}

export default function App() {
  const [notes, setNotes] = useState<Summary[]>([]);
  const [selected, setSelected] = useState("welcome");
  const [text, setText] = useState("");
  const [status, setStatus] = useState("loading notes...");
  const [scan, setScan] = useState<ScanProgress | null>(null);
  const [analysis, setAnalysis] = useState("");
  const scanCancel = useRef<AbortController | null>(null);

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
      .catch((error: unknown) => setStatus(describeError(error)));
  }, []);

  useEffect(() => {
    readNote(selected)
      .then((note) => setText(note.text))
      .catch((error: unknown) => setStatus(describeError(error)));
  }, [selected]);

  const save = async () => {
    const summary = await saveNote(selected, text);
    setStatus(`saved ${summary.name} (${summary.words} words)`);
    await refresh();
  };

  const startScan = async () => {
    const ctrl = new AbortController();
    scanCancel.current = ctrl;
    setScan(null);
    try {
      for await (const progress of scanNotes({ signal: ctrl.signal })) {
        setScan(progress);
      }
      setStatus("scan finished");
    } catch (error) {
      if (isCancelled(error)) setStatus("scan cancelled");
      else setStatus(describeError(error));
    } finally {
      scanCancel.current = null;
    }
  };

  const analyze = async () => {
    try {
      const result = await analyzeNote(selected);
      setAnalysis(
        result.keywords.length > 0
          ? `keywords: ${result.keywords.join(", ")} · longest line ${result.longest_line}`
          : "no keywords yet",
      );
    } catch (error) {
      setAnalysis(describeError(error));
    }
  };

  const showWireError = async () => {
    try {
      // Intentionally lie to TypeScript for one call so the runtime's wire
      // validation has something to reject.
      await readNote(123 as unknown as string);
    } catch (error) {
      setStatus(describeError(error));
    }
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
          <div>
            <button onClick={startScan}>Scan notes</button>
            <button onClick={() => scanCancel.current?.abort()}>Cancel scan</button>
            <button onClick={analyze}>Analyze</button>
            <button onClick={showWireError}>Try an error</button>
            <button onClick={save}>Save</button>
          </div>
        </div>
        {scan && (
          <p className="scan">
            scanned {scan.done}/{scan.total}: {scan.note} ({scan.words} words,{" "}
            {scan.headings} headings)
          </p>
        )}
        <textarea value={text} onChange={(event) => setText(event.target.value)} />
        <p className="status">{status}</p>
        <p className="status">{analysis}</p>
      </section>
    </main>
  );
}
```

</details>

Replace `src/app.css`:

<details>
<summary>Complete <code>src/app.css</code>: the wider workspace layout</summary>

<!-- snippet: 02-real-work/src/app.css -->
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
  margin-left: 6px;
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

.scan {
  margin: 0 0 10px;
  color: #52606d;
}
```

</details>


## Stream progress from Python

A scan is a natural streaming API: every finished file is one progress item.
In Python, an exposed async generator becomes an `AsyncIterable` in
TypeScript.

<!-- snippet: 02-real-work/backend/app.py -->
```python
@expose
async def scan_notes() -> AsyncIterator[ScanProgress]:
    """Scan every note, yielding one progress item per file."""
    paths = sorted(notes_dir().glob("*.md"))
    total = len(paths)
    for index, path in enumerate(paths, start=1):
        text = path.read_text(encoding="utf-8")
        yield ScanProgress(
            done=index,
            total=total,
            note=path.stem,
            words=word_count(text),
            headings=sum(1 for line in text.splitlines() if line.startswith("#")),
            links=text.count("](") + text.count("https://"),
        )
        # The pause keeps progress visible for the tiny sample corpus. Real
        # indexing work usually yields because parsing itself takes time.
        await asyncio.sleep(0.25)
```

The pause is there so progress is visible with only three sample notes. In a
real indexer you would not need it.

`ScanProgress` is another dataclass, so the generated TypeScript stream item
has typed `done`, `total`, `note`, `words`, `headings` and `links` fields.
Pylair also applies stream credit behind the scenes: a fast Python producer
cannot outrun the front end.

## Consume and cancel the stream

The generated `scanNotes` function accepts the same call options as a plain
call. Passing an `AbortSignal` is the cancellation path:

<!-- snippet: 02-real-work/src/App.tsx -->
```tsx
  const startScan = async () => {
    const ctrl = new AbortController();
    scanCancel.current = ctrl;
    setScan(null);
    try {
      for await (const progress of scanNotes({ signal: ctrl.signal })) {
        setScan(progress);
      }
      setStatus("scan finished");
    } catch (error) {
      if (isCancelled(error)) setStatus("scan cancelled");
      else setStatus(describeError(error));
    } finally {
      scanCancel.current = null;
    }
  };
```

The cancel button only aborts the controller:

<!-- snippet: 02-real-work/src/App.tsx -->
```tsx
            <button onClick={() => scanCancel.current?.abort()}>Cancel scan</button>
```

For async Python, cancellation arrives at the next `await` point. Python's
`finally` blocks and async cleanup run. Chunks already delivered stay
delivered; the stream then ends with `TASK_CANCELLED`, which the client
exposes as `isCancelled(error)`. The exact contract is in
[docs/cancellation.md](../cancellation.md).

## Show useful errors

Errors from Python are `PythonError` instances. Three optional fields are
especially useful while developing:

- `error.details`: structured input validation failures, each with `path`,
  `expected` and `received`.
- `error.suggestions`: close exposed-function names for a wrong function
  name.
- `error.traceback`: the trimmed Python traceback in dev builds. Release
  builds never include it.

Inkwell formats all three in one helper:

<!-- snippet: 02-real-work/src/App.tsx -->
```tsx
function describeError(error: unknown): string {
  if (error instanceof PythonError) {
    const parts = [error.message];
    for (const detail of error.details) {
      parts.push(`${detail.path}: expected ${detail.expected}, got ${detail.received}`);
    }
    if (error.suggestions.length > 0) {
      parts.push(`did you mean ${error.suggestions.join(", ")}?`);
    }
    if (error.traceback) {
      parts.push("Python traceback printed in the console");
      console.error(error.traceback);
    }
    return parts.join(" · ");
  }
  return error instanceof Error ? error.message : String(error);
}
```

The app has a **Try an error** button that deliberately sends a number where
`readNote` expects a string:

<!-- snippet: 02-real-work/src/App.tsx -->
```tsx
  const showWireError = async () => {
    try {
      // Intentionally lie to TypeScript for one call so the runtime's wire
      // validation has something to reject.
      await readNote(123 as unknown as string);
    } catch (error) {
      setStatus(describeError(error));
    }
  };
```

TypeScript normally prevents this mistake. The cast exists only as a small
experiment: it proves the runtime validates the wire input even when a caller
bypasses the types. In a real app, remove the experiment and keep the error
helper.

## CPU work: warm workers and isolated workers

`scan_notes` is async because it can pause between files. Keyword extraction
is different: it is synchronous CPU work. Blocking the runtime event loop
would make every other call stutter, so it runs isolated.

There are two isolated shapes:

- `worker=isolated()` starts a fresh worker process for each call. Use it
  for one-off, crash-prone or strongly stateless work.
- `worker=warm(pool=1)` keeps a worker process alive for that function.
  Imports and module state survive between calls. Use it when startup cost
  matters, especially models.

Inkwell uses a warm worker even though keyword extraction is small. Guide 3
will put a model in the same pattern:

<!-- snippet: 02-real-work/backend/app.py -->
```python
@expose(worker=warm(pool=1))
def analyze_note(name: str) -> NoteAnalysis:
    """Extract simple keywords in a warm worker process."""
    text = read_text_note(name)
    words = [word.lower() for word in _WORDS.findall(text)]
    useful = [word for word in words if len(word) > 3 and word not in _STOPWORDS]
    keywords = [word for word, _count in Counter(useful).most_common(5)]
    longest_line = max((len(line) for line in text.splitlines()), default=0)
    return NoteAnalysis(name=name, keywords=keywords, longest_line=longest_line)
```

Calling it is ordinary typed TypeScript:

<!-- snippet: 02-real-work/src/App.tsx -->
```tsx
  const analyze = async () => {
    try {
      const result = await analyzeNote(selected);
      setAnalysis(
        result.keywords.length > 0
          ? `keywords: ${result.keywords.join(", ")} · longest line ${result.longest_line}`
          : "no keywords yet",
      );
    } catch (error) {
      setAnalysis(describeError(error));
    }
  };
```

Cancellation is stronger here than for inline sync Python. A per-call worker
is terminated, then killed after grace. On POSIX a warm worker first gets an
interrupt while user code runs; if it stays stuck, it is killed and the pool
respawns. Windows warm-worker cancellation is kill-only. Those details matter
for native libraries, so read [docs/cancellation.md](../cancellation.md)
before putting long native calls behind a button.

## Inspect what happened

Restart the dev loop with the inspector:

```sh
npx pylair dev --inspect
```

The terminal announces it before the windows open:

```text
pylair: inspector window will open (PYLAIR_INSPECT=1)
```

The inspector window opens beside the app. Scan, cancel a scan, analyze a
note and press **Try an error**. In the timeline you can correlate the call,
its outcome, its latency, stderr lines and dev reloads. The Calls view keeps
per-function counts and p50/p95 latency; failed rows expand to the same
error fields you saw in the UI.

Payload previews are off by default. If you need them while debugging, use
`--inspect-payloads` and treat them as sensitive local debugging data. The
full reference is [docs/inspector.md](../inspector.md).

## What you learned

> - Async generators become typed TypeScript streams.
> - `AbortController` is the front-end cancellation mechanism, and
>   `isCancelled` distinguishes a user cancel from a failure.
> - `PythonError.details`, `suggestions` and `traceback` expose structured
>   wire errors without parsing the message.
> - CPU-heavy synchronous work belongs in an isolated worker. Warm workers
>   keep expensive state loaded; per-call workers start fresh every time.
> - `pylair dev --inspect` is the fastest way to see what the bridge did.
>
> **Next:** [guide 3](03-local-model.md) uses the warm-worker pattern for a
> real local embedding model. The complete state for this guide is in
> [`examples/learn/02-real-work/`](../../examples/learn/02-real-work/).
