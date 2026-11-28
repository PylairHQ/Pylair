# 3. A local model: offline semantic search

**What you'll build:** Inkwell downloads a pinned embedding model with
verified resumable files, shows download progress in the UI, waits until the
model is ready, and ranks notes offline.

**Time:** about 25 minutes.

Start from your guide 2 app, or fetch
[`examples/learn/03-local-model/`](../../examples/learn/03-local-model/) with the
[sparse-checkout command](README.md#jumping-in-without-the-repo) and run
`npm install` and `uv sync` inside it.

## Update the app

This step changes the Python dependencies as well as the backend and UI. If
you are continuing from guide 2, replace the five files below with their
complete guide 3 versions. If you fetched this step directly, they are already
in this state; use the blocks as the explanation reference.

Replace `pyproject.toml`:

<details>
<summary>Complete <code>pyproject.toml</code>: adds model2vec for development</summary>

<!-- snippet: 03-local-model/pyproject.toml -->
```toml
[project]
name = "inkwell"
version = "0.1.0"
description = "A Pylair desktop app"
requires-python = ">=3.12"
dependencies = ["pylair==0.1.0rc22", "model2vec==0.10.0"]

[tool.pylair]
app-module = "backend.app"
source = "backend"
requirements = "requirements.txt"
python-version = "3.12"
# prune = ["extra/deny-glob"]   # extend the default bundle deny-list
```

</details>

Replace `requirements.txt`:

<details>
<summary>Complete <code>requirements.txt</code>: bundles model2vec into installers</summary>

<!-- snippet: 03-local-model/requirements.txt -->
```text
# Locked dependencies bundled into the installer by `pylair build`.
pylair==0.1.0rc22
model2vec==0.10.0
```

</details>

Replace `backend/app.py`:

<details>
<summary>Complete <code>backend/app.py</code>: the model declaration and semantic search</summary>

<!-- snippet: 03-local-model/backend/app.py -->
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

from pylair import expose, models, warm

_WORDS = re.compile(r"[\w']+")
_NAME = re.compile(r"^[a-z0-9][a-z0-9-]{0,63}$")
MODEL_REPO = "minishlab/potion-base-2M"
MODEL_REVISION = "389b9f64be5aa4ae7a6bc6fe95ef20ce485ae5da"

# 8 MB of static embeddings: small enough for a tutorial,
# fast enough for CPU search in a desktop app.
models.model_snapshot(
    "embedder",
    repo=MODEL_REPO,
    revision=MODEL_REVISION,
    allow=["config.json", "model.safetensors", "tokenizer.json"],
)

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


@dataclass
class SearchHit:
    name: str
    title: str
    score: float


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


_embedding_model = None


def load_embedding_model():
    """Load the pinned snapshot once inside this warm worker."""
    global _embedding_model
    if _embedding_model is None:
        from model2vec import StaticModel

        _embedding_model = StaticModel.from_pretrained(str(models.path("embedder")))
    return _embedding_model


@expose(worker=warm(pool=1), requires="embedder")
def search_notes(query: str) -> list[SearchHit]:
    """Rank notes by cosine similarity to the query embedding."""
    if not query.strip():
        raise ValueError("query must not be empty")
    model = load_embedding_model()
    query_vector = model.encode([query], normalize=True)[0]
    hits: list[SearchHit] = []
    for path in sorted(notes_dir().glob("*.md")):
        text = path.read_text(encoding="utf-8")
        note_vector = model.encode([text], normalize=True)[0]
        score = float(query_vector @ note_vector)
        hits.append(
            SearchHit(
                name=path.stem,
                title=title_of(path.stem, text),
                score=round(score, 4),
            )
        )
    return sorted(hits, key=lambda hit: hit.score, reverse=True)


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
<summary>Complete <code>src/App.tsx</code>: readiness and search UI</summary>

<!-- snippet: 03-local-model/src/App.tsx -->
```tsx
import { useEffect, useRef, useState } from "react";
import {
  health,
  isCancelled,
  onApp,
  PythonError,
  type HealthAppComponent,
} from "@pylair/client";
import {
  analyzeNote,
  listNotes,
  readNote,
  saveNote,
  scanNotes,
  searchNotes,
} from "./api.generated";

type Summary = Awaited<ReturnType<typeof listNotes>>[number];
type Hit = Awaited<ReturnType<typeof searchNotes>>[number];
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

function modelText(component: HealthAppComponent | null): string {
  if (!component) return "embedder: starting";
  if (component.phase === "ready") return "embedder: ready";
  if (component.phase === "failed") return `embedder failed: ${component.detail ?? "unknown error"}`;
  const percent = component.progress == null ? "" : ` ${Math.round(component.progress * 100)}%`;
  return `embedder: downloading${percent}${component.detail ? ` (${component.detail})` : ""}`;
}

export default function App() {
  const [notes, setNotes] = useState<Summary[]>([]);
  const [selected, setSelected] = useState("welcome");
  const [text, setText] = useState("");
  const [status, setStatus] = useState("loading notes...");
  const [scan, setScan] = useState<ScanProgress | null>(null);
  const [analysis, setAnalysis] = useState("");
  const [embedder, setEmbedder] = useState<HealthAppComponent | null>(null);
  const [query, setQuery] = useState("offline desktop notes");
  const [hits, setHits] = useState<Hit[]>([]);
  const scanCancel = useRef<AbortController | null>(null);

  const refresh = async () => {
    const summaries = await listNotes();
    setNotes(summaries);
    if (!summaries.some((note) => note.name === selected)) {
      setSelected(summaries[0]?.name ?? "welcome");
    }
  };

  const refreshEmbedder = async () => {
    const snapshot = await health();
    setEmbedder(snapshot.app?.components.embedder ?? null);
  };

  useEffect(() => {
    void refreshEmbedder();
    const timer = window.setInterval(() => void refreshEmbedder(), 700);
    let unlisten: (() => void) | undefined;
    void onApp((change) => {
      if (change.name === "embedder") void refreshEmbedder();
    }).then((off) => {
      unlisten = off;
    });
    return () => {
      window.clearInterval(timer);
      unlisten?.();
    };
  }, []);

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

  const search = async () => {
    try {
      setHits(await searchNotes(query));
      setStatus("search finished");
    } catch (error) {
      setStatus(describeError(error));
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

  const modelReady = embedder?.phase === "ready";

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
        <div className="modelbar">
          <span>{modelText(embedder)}</span>
          <input value={query} onChange={(event) => setQuery(event.target.value)} />
          <button onClick={search} disabled={!modelReady}>
            Search
          </button>
        </div>
        {hits.length > 0 && (
          <div className="hits">
            {hits.map((hit) => (
              <button key={hit.name} onClick={() => setSelected(hit.name)}>
                {hit.title} <small>{hit.score.toFixed(3)}</small>
              </button>
            ))}
          </div>
        )}
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
<summary>Complete <code>src/app.css</code>: the model status and search result styles</summary>

<!-- snippet: 03-local-model/src/app.css -->
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

.modelbar {
  display: flex;
  align-items: center;
  gap: 8px;
  margin-bottom: 10px;
  color: #52606d;
}

.modelbar input {
  flex: 1;
  padding: 7px 9px;
  border: 1px solid #bcccdc;
  border-radius: 6px;
}

.hits {
  display: flex;
  gap: 6px;
  margin-bottom: 10px;
}

.hits button {
  padding: 6px 9px;
  border: 1px solid #bcccdc;
  border-radius: 6px;
  background: white;
  cursor: pointer;
}
```

</details>

After replacing `pyproject.toml`, run `uv sync` so the lockfile and virtual
environment pick up `model2vec`.


## Pick and pin the model

Inkwell uses
[`minishlab/potion-base-2M`](https://huggingface.co/minishlab/potion-base-2M),
a real static embedding model published by the model2vec maintainers under
the MIT license. Its weights are about 7.6 MB, and the files Inkwell downloads
total roughly 8.2 MB. That is small enough for a tutorial and fast enough for
CPU search in a desktop app. It is not a transformer-quality general model,
but it is genuinely useful for small local collections and keeps the feedback
loop quick.

Never pin a moving branch in an app. Resolve the branch to a full commit sha:

```sh
npx pylair models resolve minishlab/potion-base-2M@main
```

This is the output from the pin used here:

```text
389b9f64be5aa4ae7a6bc6fe95ef20ce485ae5da
revision="389b9f64be5aa4ae7a6bc6fe95ef20ce485ae5da"  # minishlab/potion-base-2M@main, resolved 2026-11-28
```

## Declare the snapshot

Add `model2vec==0.10.0` to both dependency lists:

- `pyproject.toml`, so `uv sync` installs it for development.
- `requirements.txt`, so `pylair build` bundles the same library.

Then declare the model at module import time:

<!-- snippet: 03-local-model/backend/app.py -->
```python
MODEL_REPO = "minishlab/potion-base-2M"
MODEL_REVISION = "389b9f64be5aa4ae7a6bc6fe95ef20ce485ae5da"

# 8 MB of static embeddings: small enough for a tutorial,
# fast enough for CPU search in a desktop app.
models.model_snapshot(
    "embedder",
    repo=MODEL_REPO,
    revision=MODEL_REVISION,
    allow=["config.json", "model.safetensors", "tokenizer.json"],
)
```

`model_snapshot` is declarative. Importing the module registers the component
but does not download during a codegen scan. Once the runtime starts, Pylair
fetches the pinned Hugging Face snapshot in the background. `allow` limits the
snapshot to the three files this app needs.

Use `models.fetch_snapshot()` when you need an explicit one-off fetch. For app
startup state, prefer the declarative form here because it wires readiness for
you. The full API and storage layout are in [docs/models.md](../models.md).

## Gate search until the model is ready

The search function has two important declarations:

- `worker=warm(pool=1)` keeps the process and loaded model resident.
- `requires="embedder"` makes the runtime wait for the model component before
  dispatching the call.

<!-- snippet: 03-local-model/backend/app.py -->
```python
_embedding_model = None


def load_embedding_model():
    """Load the pinned snapshot once inside this warm worker."""
    global _embedding_model
    if _embedding_model is None:
        from model2vec import StaticModel

        _embedding_model = StaticModel.from_pretrained(str(models.path("embedder")))
    return _embedding_model


@expose(worker=warm(pool=1), requires="embedder")
def search_notes(query: str) -> list[SearchHit]:
    """Rank notes by cosine similarity to the query embedding."""
    if not query.strip():
        raise ValueError("query must not be empty")
    model = load_embedding_model()
    query_vector = model.encode([query], normalize=True)[0]
    hits: list[SearchHit] = []
    for path in sorted(notes_dir().glob("*.md")):
        text = path.read_text(encoding="utf-8")
        note_vector = model.encode([text], normalize=True)[0]
        score = float(query_vector @ note_vector)
        hits.append(
            SearchHit(
                name=path.stem,
                title=title_of(path.stem, text),
                score=round(score, 4),
            )
        )
    return sorted(hits, key=lambda hit: hit.score, reverse=True)
```

`models.path("embedder")` is read-only resolution. Workers are allowed to
resolve a verified model, but never to fetch one; the main runtime owns
downloads. That rule keeps a warm worker from starting a second download or
racing the first one.

Inkwell passes `normalize=True`, so each vector has unit length and cosine
similarity is just a dot product. It encodes the notes on every search because
the corpus is tiny. A larger app would cache vectors in the warm worker and
invalidate them when files change.

## Show readiness in the UI

`requires=` is the server-side gate: a call cannot run too early. The UI
still needs to explain what is happening, so Inkwell also reads
`health().app.components.embedder`.

`health()` is a snapshot. `onApp` fires only when a component changes phase,
not on every progress tick, so progress bars still poll `health()` on a small
cadence:

<!-- snippet: 03-local-model/src/App.tsx -->
```tsx
  const refreshEmbedder = async () => {
    const snapshot = await health();
    setEmbedder(snapshot.app?.components.embedder ?? null);
  };

  useEffect(() => {
    void refreshEmbedder();
    const timer = window.setInterval(() => void refreshEmbedder(), 700);
    let unlisten: (() => void) | undefined;
    void onApp((change) => {
      if (change.name === "embedder") void refreshEmbedder();
    }).then((off) => {
      unlisten = off;
    });
    return () => {
      window.clearInterval(timer);
      unlisten?.();
    };
  }, []);
```

The status text uses `phase`, `progress` and `detail`:

<!-- snippet: 03-local-model/src/App.tsx -->
```tsx
function modelText(component: HealthAppComponent | null): string {
  if (!component) return "embedder: starting";
  if (component.phase === "ready") return "embedder: ready";
  if (component.phase === "failed") return `embedder failed: ${component.detail ?? "unknown error"}`;
  const percent = component.progress == null ? "" : ` ${Math.round(component.progress * 100)}%`;
  return `embedder: downloading${percent}${component.detail ? ` (${component.detail})` : ""}`;
}
```

The button is disabled until the component is ready. This is a UX gate in
addition to the runtime's `requires=` gate, not a replacement for it:

<!-- snippet: 03-local-model/src/App.tsx -->
```tsx
          <button onClick={search} disabled={!modelReady}>
            Search
          </button>
```

The search call itself is small because the worker does the model work:

<!-- snippet: 03-local-model/src/App.tsx -->
```tsx
  const search = async () => {
    try {
      setHits(await searchNotes(query));
      setStatus("search finished");
    } catch (error) {
      setStatus(describeError(error));
    }
  };
```

## Run it

```sh
uv sync
npx pylair dev
```

The first run downloads about 8.2 MB into the app data directory. The model
status moves from downloading to ready, then **Search** becomes enabled. On a
warm checkout, a direct harness run of the same Python code produced:

```text
hits [SearchHit(name='ideas', title='Ideas', score=0.596), SearchHit(name='projects', title='Projects', score=0.513), SearchHit(name='welcome', title='Welcome to Inkwell', score=0.2876)]
```

Your ordering can change as you edit the notes. That is the point: the app is
searching your local files, not a hard-coded result.

## Disk, upgrades and private models

Snapshots live under the app data directory in `models/embedder/<revision>`,
with a `current` pointer naming the verified revision to serve. An upgrade
downloads beside the old verified revision and flips `current` atomically, so
peak disk use is old plus new and readers never see a half-written tree.
`HF_TOKEN` is adopted as a bearer token for private Hugging Face repos when
you do not pass an explicit Authorization header, and `HF_ENDPOINT` selects a
mirror. The exact behavior, including resume and verification failure cases,
is in [docs/models.md](../models.md).

## What you learned

> - `pylair models resolve` turns a moving branch into a full revision sha.
> - `models.model_snapshot` declares a pinned multi-file model and starts its
>   verified download at runtime startup.
> - `requires=` closes the first-call race server-side.
> - `health().app` and `onApp` let the UI explain readiness without guessing.
> - `models.path()` in a warm worker resolves the verified snapshot read-only,
>   and the loaded model can stay resident between calls.
>
> **Where next:** [guide 4](04-shipping.md) turns this app into an installed
> desktop application. The complete state for this guide is in
> [`examples/learn/03-local-model/`](../../examples/learn/03-local-model/).
