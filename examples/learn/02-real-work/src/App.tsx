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
