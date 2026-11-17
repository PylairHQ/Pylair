"""W0 command-mode sample: FastAPI on the port we are given.

Exercises the compat matrix: page load, text input, file upload +
download, an SSE stream, and an external link.
"""

import json
import os
import tempfile

from fastapi import FastAPI, UploadFile
from fastapi.responses import FileResponse, HTMLResponse, StreamingResponse

app = FastAPI()
MARKER = "W0-WRAP-MARKER"
IPC_FILE = os.path.join(tempfile.gettempdir(), "w0-ipc.json")
DOWNLOAD_FILE = os.path.join(tempfile.gettempdir(), "w0-download.txt")


@app.get("/", response_class=HTMLResponse)
def index():
    return f"""<!doctype html><html><body>
<h1>{MARKER}</h1>
<form action="/upload" method="post" enctype="multipart/form-data">
  <input type="file" name="f"/><button>upload</button>
</form>
<a href="/download">download file</a>
<a href="https://tauri.app" target="_blank">external link</a>
<div id="log"></div>
<script>
const es = new EventSource("/stream");
es.onmessage = (e) => document.getElementById("log").textContent += e.data + " ";
// IPC-leak probe: the question isn't whether __TAURI_INTERNALS__ exists on
// a remote origin — it's whether invoke() actually works. Try a core
// command, two plugin commands (incl. shutdown — the dangerous one), and
// a bogus baseline. Report every result.
(async () => {{
  const inv = window.__TAURI_INTERNALS__?.invoke?.bind(window.__TAURI_INTERNALS__);
  const report = {{
    tauri: typeof window.__TAURI__,
    internals: typeof window.__TAURI_INTERNALS__,
    invoke: typeof inv,
    ua: navigator.userAgent,
    attempts: {{}},
  }};
  const tries = {{
    "core:event|emit": ["plugin:event|emit", {{event: "w0-probe", payload: 1}}],
    "plugin:pylair|python_call": ["plugin:pylair|python_call", {{function: "x", args: []}}],
    "plugin:pylair|python_shutdown": ["plugin:pylair|python_shutdown", {{}}],
    "plugin:pylair|does_not_exist": ["plugin:pylair|does_not_exist", {{}}],
  }};
  for (const [name, [cmd, args]] of Object.entries(tries)) {{
    if (!inv) {{ report.attempts[name] = "no-invoke"; continue; }}
    try {{ await inv(cmd, args); report.attempts[name] = "RESOLVED"; }}
    catch (e) {{ report.attempts[name] = "REJECTED: " + String(e && e.message || e).slice(0, 200); }}
  }}
  fetch("/ipc-probe", {{method: "POST", headers: {{"Content-Type": "application/json"}},
    body: JSON.stringify(report)}}).catch(() => {{}});
}})();
</script>
</body></html>"""


@app.post("/ipc-probe")
def ipc_probe(p: dict):
    # Lands in stderr → forwarded by the supervisor → visible in app logs.
    print("IPC-PROBE " + json.dumps(p), flush=True)
    with open(IPC_FILE, "w") as fh:
        json.dump(p, fh, indent=2)
    return {"ok": True}


@app.post("/upload")
async def upload(f: UploadFile):
    data = await f.read()
    return {"name": f.filename, "size": len(data)}


@app.get("/download")
def download():
    with open(DOWNLOAD_FILE, "w") as fh:
        fh.write(MARKER)
    return FileResponse(DOWNLOAD_FILE, filename="w0-download.txt")


@app.get("/stream")
def stream():
    async def gen():
        for i in range(5):
            yield f"data: tick{i}\n\n"

    return StreamingResponse(gen(), media_type="text/event-stream")


if __name__ == "__main__":
    import uvicorn

    uvicorn.run(app, host="127.0.0.1", port=int(os.environ["PYLAIR_PORT"]))
