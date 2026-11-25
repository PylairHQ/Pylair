import { useRef, useState } from "react";
import { PythonError } from "@pylair/client";
import { count, hello } from "./api.generated";

export default function App() {
  const [greeting, setGreeting] = useState("");
  const [nums, setNums] = useState<number[]>([]);
  const [note, setNote] = useState("");
  const [streaming, setStreaming] = useState(false);
  const cancel = useRef<AbortController | null>(null);

  const runCount = async () => {
    const ctrl = new AbortController();
    cancel.current = ctrl;
    setNums([]);
    setNote("");
    setStreaming(true);
    try {
      for await (const n of count(5, { signal: ctrl.signal })) {
        setNums((xs) => [...xs, n]);
      }
      setNote("stream finished");
    } catch (e) {
      // -32010 is the bridge cancellation outcome - anything else is real.
      if (e instanceof PythonError && e.code === -32010) setNote("stream cancelled");
      else setNote(`stream error: ${e instanceof Error ? e.message : String(e)}`);
    } finally {
      setStreaming(false);
      cancel.current = null;
    }
  };

  return (
    <main style={{ fontFamily: "system-ui", padding: 32 }}>
      <h1>Pylair app</h1>
      <button onClick={async () => setGreeting(await hello("pylair"))}>
        Call python hello()
      </button>{" "}
      <button onClick={runCount} disabled={streaming}>
        Stream count(5)
      </button>{" "}
      <button onClick={() => cancel.current?.abort()} disabled={!streaming}>
        Cancel
      </button>
      <p>{greeting}</p>
      <p>
        {nums.join(" ")} {note && <small>({note})</small>}
      </p>
      <p>
        <small>
          <code>hello</code> and <code>count</code> are typed stubs in{" "}
          <code>src/api.generated.ts</code> - autocomplete and cancellation
          included - regenerated from <code>backend/app.py</code> whenever{" "}
          <code>pylair dev</code> is running.
        </small>
      </p>
    </main>
  );
}
