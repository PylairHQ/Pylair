// Acceptance driver (decision 0011, PR 4). The webview does what a user
// would — poll runtime health, check for an update, download + install,
// relaunch — and records every observable step into the app-data accept
// log through the Python backend, so the CI harness asserts from real
// artifacts (state-dir files + the JSONL log), never from console text.
import { useEffect, useState } from "react";
import { call, health, update } from "@pylair/client";

async function rec(tag: string, data: unknown): Promise<void> {
  await call("accept_record", { tag, data: JSON.stringify(data ?? null) });
}

async function recQuiet(tag: string, data: unknown): Promise<void> {
  await rec(tag, data).catch(() => {});
}

async function updateSurface(): Promise<unknown> {
  const h = await health();
  return h.update ?? null;
}

async function drive(): Promise<void> {
  // Wait for the Python runtime to reach "ready" — the record calls need
  // it. Every poll also lands a health().update sample in the log.
  for (let i = 0; i < 240; i++) {
    try {
      const h = await health();
      await recQuiet("health", h.update ?? null);
      if (h.state === "ready") break;
    } catch {
      /* runtime not up yet */
    }
    await new Promise((r) => setTimeout(r, 500));
  }
  await recQuiet("booted", null);

  const offer = await update.check().catch((e) => ({ error: String(e) }));
  await recQuiet("check", offer);

  if (offer && typeof offer === "object" && "version" in offer && offer.version) {
    await recQuiet("install", offer);
    try {
      await update.downloadAndInstall({
        onEvent: (ev) => {
          void recQuiet("dl", ev);
        },
      });
      // POSIX: install() returned and the process lives on — relaunch
      // into the new version. On Windows this line is never reached:
      // install() exits the process into the NSIS installer.
      await recQuiet("installed", null);
      await update.relaunch();
    } catch (e) {
      await recQuiet("install_err", String(e));
    }
  }
}

export default function App() {
  const [line, setLine] = useState("update acceptance driver");
  useEffect(() => {
    void drive().catch((e) => setLine(`driver failed: ${e}`));
    // Keep health().update flowing into the accept log while the app
    // idles — the post-relaunch app's samples prove the final state.
    const t = setInterval(() => void updateSurface().then((u) => recQuiet("health", u)), 2000);
    return () => clearInterval(t);
  }, []);
  return (
    <main style={{ fontFamily: "system-ui", padding: 32 }}>
      <h1>{line}</h1>
    </main>
  );
}
