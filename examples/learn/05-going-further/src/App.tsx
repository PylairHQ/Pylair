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
