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
            raise RuntimeError(f"PDF export failed: {detail}")
        return Blob(output.read_bytes(), mime="application/pdf")

