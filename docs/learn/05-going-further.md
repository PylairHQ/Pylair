# 5. Export notes with a bundled tool, and other directions

**What you'll build:** Inkwell gains an **Export PDF** button backed by a
pinned Tectonic binary. Python runs the tool with cancellation wired in,
returns the PDF as a Blob, and the UI previews or saves it without sending
the file through a base64 JSON frame.

**Time:** about 30 minutes.

Start from your guide 4 app, or fetch
[`examples/learn/05-going-further/`](../../examples/learn/05-going-further/)
with the [sparse-checkout command](README.md#jumping-in-without-the-repo)
and run `npm install` and `uv sync` inside it.

## Update the app

This step changes six files. If you are continuing from guide 4, replace or
create them with the complete versions below. If you fetched this step
directly, they are already in this state; use the blocks as the explanation
reference.

First create `licenses/tectonic.txt`. Pylair requires a license file for every
binary the app ships:

<details>
<summary>Complete <code>licenses/tectonic.txt</code>: the upstream MIT license</summary>

<!-- snippet: 05-going-further/licenses/tectonic.txt -->
```text
(Tectonic is licensed under the MIT License. Elements of the system from which
it is derived are licensed under an extremely wide variety of open-source
licenses.)

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the “Software”), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

</details>

Then replace `pyproject.toml`:

<details>
<summary>Complete <code>pyproject.toml</code>: dependencies and the pinned Tectonic declaration</summary>

<!-- snippet: 05-going-further/pyproject.toml -->
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

# Tectonic is a single-file, MIT-licensed typesetting binary. The musl
# Linux build is static, so it also runs on the gnu target Pylair builds.
[tool.pylair.binaries.tectonic]
license = "MIT"
license_file = "licenses/tectonic.txt"
version = "0.17.0"

[tool.pylair.binaries.tectonic.x86_64-pc-windows-msvc]
url = "https://github.com/tectonic-typesetting/tectonic/releases/download/tectonic%400.17.0/tectonic-0.17.0-x86_64-pc-windows-msvc.zip"
sha256 = "f61ce51f0b0ade1015b7de7ef368541c5424e9756ecbd0d7af97d6d48030845f"
member = "tectonic.exe"

[tool.pylair.binaries.tectonic.x86_64-apple-darwin]
url = "https://github.com/tectonic-typesetting/tectonic/releases/download/tectonic%400.17.0/tectonic-0.17.0-x86_64-apple-darwin.tar.gz"
sha256 = "7c90ef5b6ddb1eb1937e4337add5237b79338e4b9676459fa91187d24d6cdf80"
member = "tectonic"

[tool.pylair.binaries.tectonic.aarch64-apple-darwin]
url = "https://github.com/tectonic-typesetting/tectonic/releases/download/tectonic%400.17.0/tectonic-0.17.0-aarch64-apple-darwin.tar.gz"
sha256 = "a3f1cac7c5678f01661a92212f58480ae3b0634115d880dbc59e2953ded45667"
member = "tectonic"

[tool.pylair.binaries.tectonic.x86_64-unknown-linux-gnu]
url = "https://github.com/tectonic-typesetting/tectonic/releases/download/tectonic%400.17.0/tectonic-0.17.0-x86_64-unknown-linux-musl.tar.gz"
sha256 = "8533d07f9ccbd7a65824b9e0459041bca34af1eb33daba48f59215593753a3b7"
member = "tectonic"
[tool.pylair.binaries.tectonic.aarch64-unknown-linux-gnu]
url = "https://github.com/tectonic-typesetting/tectonic/releases/download/tectonic%400.17.0/tectonic-0.17.0-aarch64-unknown-linux-musl.tar.gz"
sha256 = "b10954a95404f3ab2328d2fa59a5ebab8e657f893fab096f98be8db7c0c979b8"
member = "tectonic"
```

</details>



Replace `src-tauri/tauri.conf.json` as well. The only application change is
`frame-src`: the iframe preview is a nested browsing context, so the CSP must
explicitly permit `blob:` frames rather than relying on `media-src`:

<details>
<summary>Complete <code>src-tauri/tauri.conf.json</code>: CSP with blob frames enabled</summary>

<!-- snippet: 05-going-further/src-tauri/tauri.conf.json -->
```json
{
  "$schema": "https://schema.tauri.app/config/2",
  "productName": "inkwell",
  "version": "0.1.0",
  "identifier": "dev.pylair.inkwell",
  "build": {
    "frontendDist": "../dist",
    "devUrl": "http://localhost:1420",
    "beforeDevCommand": "npm run dev",
    "beforeBuildCommand": "npm run build"
  },
  "app": {
    "windows": [
      {
        "label": "main",
        "title": "inkwell",
        "width": 960,
        "height": 640
      }
    ],
    "security": {
      "csp": "default-src 'self'; script-src 'self'; connect-src 'self' blob: ipc: http://ipc.localhost pylair-blob: http://pylair-blob.localhost; img-src 'self' data: blob: pylair-blob: http://pylair-blob.localhost; frame-src 'self' blob: pylair-blob: http://pylair-blob.localhost; media-src 'self' blob: pylair-blob: http://pylair-blob.localhost; style-src 'self' 'unsafe-inline'",
      "devCsp": "default-src 'self'; script-src 'self' 'unsafe-inline'; connect-src 'self' blob: ws: ipc: http://ipc.localhost pylair-blob: http://pylair-blob.localhost; img-src 'self' data: blob: pylair-blob: http://pylair-blob.localhost; frame-src 'self' blob: pylair-blob: http://pylair-blob.localhost; media-src 'self' blob: pylair-blob: http://pylair-blob.localhost; style-src 'self' 'unsafe-inline'"
    }
  },
  "bundle": {
    "active": true,
    "targets": "all",
    "icon": [
      "icons/32x32.png",
      "icons/128x128.png",
      "icons/128x128@2x.png",
      "icons/icon.icns",
      "icons/icon.ico"
    ],
    "resources": {
      "resources/python": "python/",
      "resources/site-packages": "site-packages/",
      "resources/backend": "backend/",
      "resources/THIRD-PARTY-NOTICES": "THIRD-PARTY-NOTICES/"
    },
    "macOS": {
      "minimumSystemVersion": "12.0"
    },
    "windows": {
      "nsis": {
        "installMode": "currentUser"
      }
    }
  }
}
```

</details>

Replace `backend/app.py`:

<details>
<summary>Complete <code>backend/app.py</code>: notes, search and PDF export</summary>

<!-- snippet: 05-going-further/backend/app.py -->
```python
"""Inkwell backend: local markdown notes exposed to the TypeScript UI."""

import asyncio
import os
import re
import tempfile
from collections import Counter
from collections.abc import AsyncIterator
from dataclasses import dataclass
from datetime import UTC, datetime
from pathlib import Path

from pylair import Blob, binary, expose, models, proc, warm

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


FIRST_EXPORT_MESSAGE = (
    "First PDF export needs an internet connection to download typesetting files"
)


def tectonic_needs_download(detail: str) -> bool:
    """Recognize Tectonic's first-run support-file download failure."""
    text = detail.lower()
    if "please check your network connection" in text:
        return True
    download = any(word in text for word in ("download", "network", "offline", "connect"))
    bundle = any(word in text for word in ("bundle", "support file", "latex support"))
    return download and bundle

_LATEX_SPECIALS = {
    "\\": r"\textbackslash{}",
    "&": r"\&",
    "%": r"\%",
    "$": r"\$",
    "#": r"\#",
    "_": r"\_",
    "{": r"\{",
    "}": r"\}",
    "~": r"\textasciitilde{}",
    "^": r"\textasciicircum{}",
}


def latex_escape(text: str) -> str:
    return "".join(_LATEX_SPECIALS.get(ch, ch) for ch in text)


def note_as_tex(title: str, markdown: str) -> str:
    """Render Inkwell's simple markdown subset as a small LaTeX document."""
    parts = [
        r"\documentclass{article}",
        r"\begin{document}",
        rf"\section*{{{latex_escape(title)}}}",
    ]
    for line in markdown.splitlines():
        stripped = line.strip()
        if not stripped:
            parts.append("")
        elif stripped.startswith("## "):
            parts.append(rf"\subsection*{{{latex_escape(stripped[3:])}}}")
        elif stripped.startswith("# "):
            parts.append(rf"\subsection*{{{latex_escape(stripped[2:])}}}")
        elif stripped.startswith("- "):
            parts.append(rf"\noindent\textbullet\ {latex_escape(stripped[2:])}\par")
        else:
            parts.append(rf"\noindent {latex_escape(stripped)}\par")
    parts.append(r"\end{document}")
    return "\n".join(parts) + "\n"


@expose
async def export_note_pdf(name: str) -> Blob:
    """Typeset one note with the bundled Tectonic binary and return a PDF."""
    text = read_text_note(name)
    title = title_of(name, text)
    with tempfile.TemporaryDirectory(prefix="inkwell-export-") as tmp:
        work = Path(tmp)
        source = work / "note.tex"
        source.write_text(note_as_tex(title, text), encoding="utf-8")
        process = await proc.spawn(
            binary("tectonic"),
            "--outdir",
            str(work),
            str(source),
            stdout=proc.DEVNULL,
            stderr=proc.PIPE,
        )
        lines: list[str] = []
        async for line in process.stderr:
            lines.append(line)
        code = await process.wait()
        output = work / "note.pdf"
        if code != 0 or not output.exists():
            detail = "".join(lines[-4:]).strip() or f"tectonic exited with {code}"
            if tectonic_needs_download(detail):
                raise RuntimeError(FIRST_EXPORT_MESSAGE)
            raise RuntimeError(f"PDF export failed: {detail}")
        return Blob(output.read_bytes(), mime="application/pdf")
```

</details>

Replace `src/App.tsx`:

<details>
<summary>Complete <code>src/App.tsx</code>: the note editor, search and export UI</summary>

<!-- snippet: 05-going-further/src/App.tsx -->
```tsx
import { useEffect, useRef, useState } from "react";
import {
  health,
  isCancelled,
  onApp,
  PythonError,
  type BlobHandle,
  type HealthAppComponent,
} from "@pylair/client";
import {
  analyzeNote,
  exportNotePdf,
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
  const exportCancel = useRef<AbortController | null>(null);
  const pdfRef = useRef<BlobHandle | null>(null);
  const [pdfUrl, setPdfUrl] = useState<string | null>(null);
  const [pdfName, setPdfName] = useState("");

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
    return () => {
      void pdfRef.current?.release();
      pdfRef.current = null;
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

  const exportPdf = async () => {
    const ctrl = new AbortController();
    exportCancel.current = ctrl;
    let handle: BlobHandle | null = null;
    try {
      setStatus("exporting PDF...");
      const noteName = selected;
      handle = (await exportNotePdf(noteName, { signal: ctrl.signal })) as BlobHandle;
      const objectUrl = await handle.mediaUrl();
      const previous = pdfRef.current;
      pdfRef.current = handle;
      setPdfUrl(objectUrl);
      setPdfName(noteName);
      if (previous) void previous.release();
      setStatus("PDF ready");
    } catch (error) {
      if (handle && pdfRef.current !== handle) void handle.release();
      if (isCancelled(error)) setStatus("export cancelled");
      else setStatus(describeError(error));
    } finally {
      exportCancel.current = null;
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
            <button onClick={exportPdf}>Export PDF</button>
            <button onClick={() => exportCancel.current?.abort()}>Cancel export</button>
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
        {pdfUrl && (
          <div className="pdfpreview">
            <div>
              <strong>{pdfName}.pdf</strong>
              <a href={pdfUrl} download={`${pdfName}.pdf`}>Save PDF</a>
            </div>
            <iframe title={`${pdfName} PDF preview`} src={pdfUrl} />
          </div>
        )}
        <p className="status">{status}</p>
        <p className="status">{analysis}</p>
      </section>
    </main>
  );
}
```

</details>

Finally replace `src/app.css`:

<details>
<summary>Complete <code>src/app.css</code>: the editor and PDF preview layout</summary>

<!-- snippet: 05-going-further/src/app.css -->
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

.pdfpreview {
  margin-bottom: 10px;
}

.pdfpreview div {
  display: flex;
  justify-content: space-between;
  margin-bottom: 5px;
}

.pdfpreview iframe {
  width: 100%;
  height: 240px;
  border: 1px solid #bcccdc;
  border-radius: 6px;
  background: white;
}
```

</details>

Run `uv sync` after the edit. The Python dependency set did not change, but
the command confirms the environment and lockfile are still in sync.

## Pick the tool

Inkwell uses [Tectonic](https://github.com/tectonic-typesetting/tectonic), a
single-file typesetting engine, to turn a note into a PDF. It fits this guide
because:

- it is MIT-licensed rather than GPL-licensed;
- upstream publishes one executable per desktop platform;
- the Windows asset is a zip and the macOS and Linux assets are tar.gz files,
  the two archive shapes Pylair's extractor supports;
- the compressed assets are about 10 to 21 MB depending on platform, large
  enough to be worth pinning but small enough for a tutorial;
- it is a real document tool rather than a fixture invented for the example.

Tectonic is self-contained as an executable, but its typesetting engine
fetches LaTeX support files from Tectonic's servers the first time a document
needs them. The first PDF export therefore needs an internet connection and
can take noticeably longer; later exports work offline from Tectonic's cache.
That cache is separate from Pylair's binary cache. The binary and those
support files are different artifacts: Pylair verifies the pinned binary
archive, while Tectonic verifies its own support bundle. Inkwell still keeps
its notes, search index inputs and embedding model local, but this export is
not offline until that first typesetting download has happened.

## Declare and pin the binary

The declaration names the license, the license file and one hash-pinned
source per target triple:

<!-- snippet: 05-going-further/pyproject.toml -->
```toml
[tool.pylair.binaries.tectonic]
license = "MIT"
license_file = "licenses/tectonic.txt"
version = "0.17.0"

[tool.pylair.binaries.tectonic.x86_64-pc-windows-msvc]
url = "https://github.com/tectonic-typesetting/tectonic/releases/download/tectonic%400.17.0/tectonic-0.17.0-x86_64-pc-windows-msvc.zip"
sha256 = "f61ce51f0b0ade1015b7de7ef368541c5424e9756ecbd0d7af97d6d48030845f"
member = "tectonic.exe"
```

`member` extracts exactly one file from the archive. The Linux targets use
Tectonic's static musl builds because they run on the GNU Linux triples Pylair
builds for without depending on the host's glibc version. Apple Silicon and
aarch64 Linux have their own entries; Intel macOS, Windows x64 and Linux x64
are covered too. Upstream does not publish a suitable aarch64 Windows asset in
this release, and Pylair does not currently build a standalone-CPython Windows
arm64 shell, so there is no Windows arm64 target to declare.

During `pylair dev` or `pylair build`, Pylair downloads each archive once
into `.pylair/cache/binaries/<sha256>`, verifies the hash before staging, and
never falls back to a `tectonic` that happens to be on `PATH`. The license
file travels into the app's third-party notices. Missing license files fail
the build; shipping an unaccounted binary is not a gray area.
[docs/binaries.md](../binaries.md) covers the extraction and staging rules.

## Run it with cancellation

The backend asks the host for the resolved path, then spawns the tool through
`pylair.proc`:

<!-- snippet: 05-going-further/backend/app.py -->
```python
        process = await proc.spawn(
            binary("tectonic"),
            "--outdir",
            str(work),
            str(source),
            stdout=proc.DEVNULL,
            stderr=proc.PIPE,
        )
```

`binary("tectonic")` is the host-resolved absolute path of the pinned binary.
`pylair.proc.spawn` matters because of ownership: when the bridge task is
cancelled, its child process tree is killed before the cancellation is
reported. `stdout=proc.DEVNULL` is deliberate because Tectonic's normal
output is not needed. `stderr=proc.PIPE` is drained by the `async for` loop,
so the pipe cannot fill and block the child.

The UI's existing cancellation pattern is the other half:

<!-- snippet: 05-going-further/src/App.tsx -->
```tsx
      handle = (await exportNotePdf(noteName, { signal: ctrl.signal })) as BlobHandle;
```

When **Cancel export** aborts the controller, the client cancellation reaches
the Python task and Pylair kills the child process before surfacing the
cancelled error. No orphaned conversion keeps running in the background.

The first-run network failure is also mapped to a user-actionable error.
When Tectonic reports that it cannot download needed typesetting files, the
backend raises this exact message instead of leaking raw compiler output:

<!-- snippet: 05-going-further/backend/app.py -->
```python
            if tectonic_needs_download(detail):
                raise RuntimeError(FIRST_EXPORT_MESSAGE)
```

The bridge carries it as a structured `PythonError`, so Inkwell shows "First
PDF export needs an internet connection to download typesetting files" in the
status line. Other Tectonic failures keep their trimmed stderr detail.

## Return bytes as a Blob

The export returns `Blob(output.read_bytes(), mime="application/pdf")`:

<!-- snippet: 05-going-further/backend/app.py -->
```python
        return Blob(output.read_bytes(), mime="application/pdf")
```

A Blob crosses the bridge as a small tag, while the host keeps the bytes in
its per-generation blob store. The UI receives a handle with `.url`,
`.stream()`, `.mediaUrl()` and `.release()` rather than a giant base64 value
inside a JSON frame. Inkwell uses `mediaUrl()` because the PDF is small and a
`blob:` URL works in an iframe on every WebView:

<!-- snippet: 05-going-further/src/App.tsx -->
```tsx
      const objectUrl = await handle.mediaUrl();
      const previous = pdfRef.current;
      pdfRef.current = handle;
      setPdfUrl(objectUrl);
      setPdfName(noteName);
      if (previous) void previous.release();
```

Release is still important, but timing matters: releasing a handle also
revokes the object URL that `mediaUrl()` made. Inkwell therefore keeps the
current handle while its preview is visible, releases the previous handle when
a replacement arrives, and releases the current one when the UI unmounts.
Larger files should use `.url` or `.stream()` instead of copying everything
into the webview. The lifetime, range and ownership rules are in
[docs/blobs.md](../blobs.md).

## Try it

Run:

```sh
npx pylair dev
```

The first run resolves the pinned Tectonic archive for your platform into
`.pylair/cache/binaries/`. Choose a note, press **Export PDF**, and the PDF
preview appears under the editor with a **Save PDF** link. The first export
needs a network connection to fetch Tectonic's support files; after that it
works offline. Press **Cancel export** during a run to exercise the
cancellation path.

## Other direction: directory units

Tectonic is the simple shape: one executable. Some tools are a tree instead.
`llama-server`, for example, ships with shared libraries beside the server
executable. Pylair calls that a directory unit and declares it with
`executable` rather than `member`. This block is illustrative, not a
copy-paste-ready pin:

```toml
# Illustrative shape only - verify the current release artifact and hash
# before using a real llama-server declaration.
[tool.pylair.binaries.llama-server]
license = "MIT"
license_file = "licenses/llama.cpp.txt"

[tool.pylair.binaries.llama-server.x86_64-unknown-linux-gnu]
url = "https://github.com/ggml-org/llama.cpp/releases/download/b7040/llama-b7040-bin-linux-x64.zip"
sha256 = "<sha256 of the archive>"
executable = "llama-server"
```

The whole tree is staged and hash or manifest verified. On macOS and Linux,
Pylair rewrites library loading paths so the executable finds its sibling
libraries inside the unit; on Windows the loader already searches the
executable's directory first. `pylair.binary("llama-server")` still resolves
to the entry point.

Size is the trade-off. A directory unit is inside the installer, every update
download, and the rollback cache. Large GPU builds can be hundreds of
megabytes, so a lazy downloaded runtime may be a better product decision than
bundling every variant. `pylair doctor` reports unit size and warns above
200 MB. The unit format and loader rewriting details are in
[docs/binaries.md](../binaries.md#directory-units).

## Other direction: wrap an existing Python web app

Inkwell is a bridge app: React calls typed Python functions. If you already
have a Gradio or Streamlit app, wrap mode takes the opposite path. Pylair
packages the existing Python web server in a desktop shell, waits for its
readiness endpoint, and shows it in a native window without giving the remote
page bridge IPC.

The shape is:

```sh
npx pylair wrap --framework gradio --entry app.py
npx pylair dev
npx pylair build
```

Wrap mode is for putting an existing app in a desktop shell, not for typed
Python-to-TypeScript calls. Its configuration, readi
ness behavior and
per-framework checks are in [docs/wrap-mode-brief.md](../wrap-mode-brief.md).

## What you learned

> - `[tool.pylair.binaries]` declares license-aware, per-platform,
>   hash-pinned executables.
> - `member` extracts a single file from a zip or tar.gz archive; there is no
>   `PATH` fallback.
> - `pylair.binary()` resolves the staged path and `pylair.proc.spawn` ties
>   child-process lifetime to bridge cancellation.
> - Pipes must be drained or disabled; Inkwell uses `DEVNULL` for stdout and
>   drains stderr.
> - `Blob` keeps generated files out of base64 JSON frames and gives the UI a
>   controlled URL or stream.
> - Directory units cover tools with companion libraries, while wrap mode
>   packages an existing Python web app.
>
> **Where next:** the complete Inkwell state is in
> [`examples/learn/05-going-further/`](../../examples/learn/05-going-further/).
> The reference pages in [docs/README.md](../README.md) are the map for
> deeper topics.
