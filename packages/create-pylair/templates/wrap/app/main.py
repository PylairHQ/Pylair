"""Sample web app for wrap mode: FastAPI served by uvicorn.

Replace this with your own app, and `cmd` in pyproject.toml with the command
that serves it. Pylair starts the command with `{port}` filled in, waits for
it to answer on 127.0.0.1, then shows it in the window.
"""

from fastapi import FastAPI
from fastapi.responses import HTMLResponse, PlainTextResponse, Response

app = FastAPI()

PAGE = """<!doctype html>
<meta charset="utf-8">
<title>__APP_NAME__</title>
<body style="font-family: system-ui, sans-serif; max-width: 40rem; margin: 3rem auto">
  <h1>__APP_NAME__</h1>
  <p id="marker">PYLAIR-WRAP-OK</p>
  <p>This page is served by FastAPI on 127.0.0.1 and shown in the app window.</p>
  <ul>
    <li><a href="/download">Download a file</a> (asks where to save it)</li>
    <li><a href="https://example.com/" target="_blank">An external link</a>
        (opens in your browser)</li>
  </ul>
</body>
"""


@app.get("/", response_class=HTMLResponse)
def index() -> str:
    return PAGE


@app.get("/download")
def download() -> Response:
    return Response(
        "hello from a wrapped app\n",
        media_type="text/plain",
        headers={"Content-Disposition": 'attachment; filename="hello.txt"'},
    )


@app.get("/health", response_class=PlainTextResponse)
def health() -> str:
    return "ok"
