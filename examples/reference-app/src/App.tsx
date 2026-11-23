import { useEffect, useRef, useState } from "react";
import {
  call,
  stream,
  state,
  onState,
  PythonError,
  type BlobHandle,
  type RuntimeState,
  type StateChange,
} from "@pylair/client";

interface OllamaStatus {
  installed: boolean;
  running: boolean;
  version: string | null;
  models: string[];
  default_model: string;
  error: string | null;
}

const STATE_COLOR: Record<RuntimeState, string> = {
  starting: "#b58900",
  ready: "#2aa198",
  unresponsive: "#cb4b16",
  restarting: "#b58900",
  failed: "#dc322f",
  stopped: "#657b83",
};

export default function App() {
  const [rt, setRt] = useState<RuntimeState>("starting");
  const [transitions, setTransitions] = useState<string[]>([]);
  const [ollama, setOllama] = useState<OllamaStatus | null>(null);
  const [prompt, setPrompt] = useState("");
  const [output, setOutput] = useState("");
  const [busy, setBusy] = useState<"chat" | "demo" | null>(null);
  const [note, setNote] = useState("");
  const [blobUrl, setBlobUrl] = useState<string | null>(null);
  const [blobStatus, setBlobStatus] = useState("fetching");
  const blobHandle = useRef<BlobHandle | null>(null);
  const abort = useRef<AbortController | null>(null);

  const refreshStatus = () =>
    call<OllamaStatus>("status")
      .then(setOllama)
      .catch((e) => setNote(`status failed: ${String(e)}`));

  useEffect(() => {
    void state().then(setRt);
    let unlisten: (() => void) | undefined;
    void onState((s: StateChange) => {
      setRt(s.to);
      setTransitions((t) =>
        [...t.slice(-7), `${s.from} → ${s.to} — ${s.detail}`],
      );
    }).then((u) => (unlisten = u));
    // Under the webview self-test every task id must belong to the
    // self-test itself — a UI-driven call here would race it for ids.
    if (!(globalThis as { __PYLAIR_SELF_TEST__?: boolean }).__PYLAIR_SELF_TEST__) {
      refreshStatus();
      // Decision-0010 demo: make_blob returns a BlobHandle; <img> loads
      // through the pylair-blob scheme, then release() frees the host file.
      // The load outcome lands in data-blob-status on #blob-img so the
      // webview self-test can assert on it. Skipped under the self-test flag
      // like refreshStatus — every task id there belongs to the self-test.
      void (async () => {
        try {
          const h = await call<BlobHandle>("make_blob");
          blobHandle.current = h;
          setBlobStatus("loading");
          setBlobUrl(h.url);
        } catch (e) {
          setBlobStatus(`error: ${e instanceof Error ? e.message : String(e)}`);
        }
      })();
    }
    return () => unlisten?.();
  }, []);

  const runStream = async (fn: "chat" | "demo_stream", input: Record<string, unknown>, tag: "chat" | "demo") => {
    abort.current = new AbortController();
    setBusy(tag);
    setOutput("");
    setNote("");
    try {
      for await (const delta of stream<string>(fn, input, { signal: abort.current.signal })) {
        setOutput((o) => o + delta);
      }
      setNote(`${tag} finished`);
    } catch (e) {
      if (e instanceof PythonError && e.code === -32010) setNote("cancelled");
      else setNote(`${tag} error: ${e instanceof Error ? e.message : String(e)}`);
    } finally {
      setBusy(null);
      abort.current = null;
    }
  };

  const canChat = !busy && rt === "ready" && !!ollama?.running;
  const canAct = !busy && rt === "ready";

  return (
    <main style={{ fontFamily: "system-ui", padding: 24, maxWidth: 720, margin: "0 auto" }}>
      <h1 style={{ marginBottom: 4 }}>Pylair reference app</h1>
      <p style={{ marginTop: 0, color: "#666" }}>
        Local-AI chat over the bridge: streaming, cancellation, crash recovery.
      </p>

      <section style={{ display: "flex", gap: 16, alignItems: "center", marginBottom: 12 }}>
        <span
          title="runtime state"
          style={{
            padding: "2px 10px",
            borderRadius: 10,
            background: STATE_COLOR[rt],
            color: "white",
            fontSize: 13,
          }}
        >
          python: {rt}
        </span>
        <span style={{ fontSize: 13 }}>
          ollama:{" "}
          {ollama === null
            ? "…"
            : ollama.running
              ? `${ollama.version ?? "running"} — ${ollama.models.length} model(s)`
              : `not running${ollama.error ? ` (${ollama.error})` : ""}`}
        </span>
        <button onClick={refreshStatus}>refresh</button>
      </section>

      <section style={{ display: "flex", gap: 8, marginBottom: 8 }}>
        <input
          style={{ flex: 1, padding: 6 }}
          placeholder="ask the local model…"
          value={prompt}
          onChange={(e) => setPrompt(e.target.value)}
        />
        <button
          disabled={!canChat || !prompt.trim()}
          onClick={() => void runStream("chat", { prompt }, "chat")}
        >
          chat
        </button>
        <button
          disabled={!canAct}
          onClick={() => void runStream("demo_stream", { tokens: 80, delay_ms: 60 }, "demo")}
        >
          demo stream
        </button>
        <button disabled={!busy} onClick={() => abort.current?.abort()}>
          cancel
        </button>
      </section>

      <pre
        style={{
          minHeight: 120,
          padding: 12,
          background: "#f5f5f5",
          whiteSpace: "pre-wrap",
          borderRadius: 6,
        }}
      >
        {output || "(streamed output appears here)"}
      </pre>
      <p style={{ minHeight: 20, color: "#555" }}>{note}</p>

      <section style={{ display: "flex", gap: 8, alignItems: "center" }}>
        <button
          disabled={!canAct}
          onClick={() =>
            void call("crash").catch(() => {
              /* the process dies before it can reply */
            })
          }
        >
          crash python
        </button>
        <small>kills the runtime — watch the state badge recover</small>
      </section>

      <section style={{ display: "flex", gap: 8, alignItems: "center", marginTop: 12 }}>
        <small style={{ color: "#777" }}>backend blob:</small>
        <img
          id="blob-img"
          data-blob-status={blobStatus}
          src={blobUrl ?? undefined}
          alt=""
          width={24}
          height={24}
          style={{ visibility: blobStatus === "loaded" ? "visible" : "hidden" }}
          onLoad={() => {
            setBlobStatus("loaded");
            void blobHandle.current?.release();
          }}
          onError={() => setBlobStatus("error: image failed to load")}
        />
        <small style={{ color: "#777" }}>{blobStatus}</small>
      </section>

      <section style={{ marginTop: 12 }}>
        <small style={{ color: "#777" }}>recent transitions</small>
        <ul style={{ margin: "4px 0", paddingLeft: 18, fontSize: 12, color: "#777" }}>
          {transitions.map((t, i) => (
            <li key={i}>{t}</li>
          ))}
        </ul>
      </section>
    </main>
  );
}
