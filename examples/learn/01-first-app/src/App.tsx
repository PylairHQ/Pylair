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
