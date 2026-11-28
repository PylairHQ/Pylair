"""Learning-path example tests.

These drive the real example backends through the same frame-level runtime
harness as the protocol suite. The TypeScript side is checked in the JS lane;
here each step's exposed Python functions run in the example's own uv
environment. Step 3's Hugging Face traffic is loopback-only: ModelServer
serves a tiny generated static-embedding snapshot at the same pinned revision
the guide uses.
"""

from __future__ import annotations

import json
import os
import struct
import subprocess
import sys
import time
from pathlib import Path

import pytest

from conftest import Runtime
from models_server import ModelServer, Repo
from pylair import _protocol as proto

HERE = Path(__file__).parent
REPO_ROOT = (HERE / "../../..").resolve()
LEARN = REPO_ROOT / "examples" / "learn"
MODEL_REV = "389b9f64be5aa4ae7a6bc6fe95ef20ce485ae5da"


def _call(rid: int, fn: str, task: int, input: dict | None = None, stream=None) -> dict:
    params = {"fn": fn, "task": task, "input": input or {}}
    if stream is not None:
        params["stream"] = stream
    return proto.request(rid, "call", params)


def _python_for(step: str) -> str:
    suffix = "Scripts/python.exe" if sys.platform == "win32" else "bin/python"
    path = LEARN / step / ".venv" / suffix
    if path.exists():
        return str(path)
    message = f"{step} has no .venv; run `uv sync` in examples/learn/{step} first"
    if os.environ.get("PYLAIR_LEARN_REQUIRED") == "1":
        pytest.fail(message)
    pytest.skip(message)


def _runtime(step: str, module: str, tmp_path: Path, extra_env: dict | None = None):
    env = {"PYLAIR_APP_DATA_DIR": str(tmp_path)}
    env.update(extra_env or {})
    return Runtime(module=module, env=env, python=_python_for(step))


def _stop(rt: Runtime) -> None:
    rt.kill()
    rt.proc.wait(timeout=10)


def test_learn01_note_crud_through_runtime(tmp_path):
    rt = _runtime("01-first-app", "testapp_learn_01", tmp_path)
    try:
        hello = rt.recv()
        names = {e["name"] for e in hello["params"]["exposes"]}
        assert {"list_notes", "read_note", "save_note"} <= names

        rt.send(_call(1, "list_notes", 1))
        listed = rt.recv_until(lambda m: m.get("id") == 1)["result"]
        assert {n["name"] for n in listed} >= {"welcome", "ideas"}

        rt.send(
            _call(
                2,
                "save_note",
                2,
                {"name": "runtime-test", "text": "# Runtime\n\nhello typed bridge"},
            )
        )
        saved = rt.recv_until(lambda m: m.get("id") == 2)["result"]
        assert saved["name"] == "runtime-test"
        assert saved["words"] == 4

        rt.send(_call(3, "read_note", 3, {"name": "runtime-test"}))
        read = rt.recv_until(lambda m: m.get("id") == 3)["result"]
        assert read["text"] == "# Runtime\n\nhello typed bridge"
    finally:
        _stop(rt)


def test_learn02_stream_and_warm_analysis_through_runtime(tmp_path):
    rt = _runtime("02-real-work", "testapp_learn_02", tmp_path)
    try:
        rt.recv()
        rt.send(_call(1, "scan_notes", 1, stream=True))
        rt.send(proto.notification("stream.credit", {"task": 1, "n": 10}))
        chunks = []
        deadline = time.monotonic() + 20
        while len(chunks) < 3 and time.monotonic() < deadline:
            msg = rt.recv_until(
                lambda m: m.get("method") == "stream.chunk" or m.get("id") == 1,
                timeout=max(1, deadline - time.monotonic()),
            )
            if msg.get("method") == "stream.chunk":
                chunks.append(msg["params"]["value"])
            else:
                break
        assert len(chunks) == 3
        assert [c["done"] for c in chunks] == [1, 2, 3]
        assert all(c["total"] == 3 for c in chunks)
        terminal = rt.recv_until(lambda m: m.get("id") == 1)
        assert terminal["result"] == {"done": True}

        rt.send(_call(2, "analyze_note", 2, {"name": "projects"}))
        analysis = rt.recv_until(lambda m: m.get("id") == 2, timeout=30)["result"]
        assert analysis["name"] == "projects"
        assert analysis["longest_line"] > 0
        assert "notes" in analysis["keywords"] or "desktop" in analysis["keywords"]
    finally:
        _stop(rt)


def _safetensors_f32(rows: list[list[float]]) -> bytes:
    """A minimal safetensors file for one F32 tensor named `embeddings`."""
    flat = [x for row in rows for x in row]
    payload = struct.pack("<" + "f" * len(flat), *flat)
    header = json.dumps(
        {
            "embeddings": {
                "dtype": "F32",
                "shape": [len(rows), len(rows[0])],
                "data_offsets": [0, len(payload)],
            }
        }
    ).encode()
    header += b" " * ((8 - (len(header) % 8)) % 8)
    return struct.pack("<Q", len(header)) + header + payload


def _tiny_static_model_repo() -> Repo:
    vocab = {
        "[UNK]": 0,
        "welcome": 1,
        "inkwell": 2,
        "notes": 3,
        "desktop": 4,
        "offline": 5,
        "projects": 6,
        "bread": 7,
        "model": 8,
        "search": 9,
    }
    tokenizer = {
        "version": "1.0",
        "truncation": None,
        "padding": None,
        "added_tokens": [],
        "normalizer": {"type": "Lowercase"},
        "pre_tokenizer": {"type": "Whitespace"},
        "post_processor": None,
        "decoder": None,
        "model": {"type": "WordLevel", "vocab": vocab, "unk_token": "[UNK]"},
    }
    rows = [
        [0.0, 0.0],
        [1.0, 0.0],
        [1.0, 0.0],
        [0.0, 1.0],
        [0.0, 1.0],
        [0.0, 1.0],
        [1.0, 1.0],
        [-1.0, 0.0],
        [1.0, 0.0],
        [1.0, 0.0],
    ]
    return Repo(
        MODEL_REV,
        {
            "config.json": b'{"normalize": true}',
            "tokenizer.json": json.dumps(tokenizer, separators=(",", ":")).encode(),
            "model.safetensors": _safetensors_f32(rows),
        },
        lfs={"model.safetensors"},
    )


@pytest.fixture
def model_server():
    for _ in range(6):
        srv = ModelServer()
        if srv.probe():
            break
        srv.close()
    else:
        pytest.fail("loopback server never accepted a connection")
    yield srv
    srv.close()


def test_learn03_model_gating_and_search_through_runtime(model_server, tmp_path):
    model_server.add_repo("minishlab/potion-base-2M", _tiny_static_model_repo())
    rt = _runtime(
        "03-local-model",
        "testapp_learn_03",
        tmp_path,
        {
            "HF_ENDPOINT": model_server.base,
            "PYLAIR_MODELS_ALLOW_INSECURE": "1",
        },
    )
    try:
        rt.recv()
        # Sent before the download can possibly finish: requires="embedder"
        # must gate the call, then the warm worker resolves path() read-only.
        rt.send(_call(1, "search_notes", 1, {"query": "offline desktop notes"}))
        result = rt.recv_until(lambda m: m.get("id") == 1, timeout=30)
        assert "result" in result, result
        hits = result["result"]
        assert len(hits) == 3
        assert {h["name"] for h in hits} == {"welcome", "ideas", "projects"}
        assert all(isinstance(h["score"], (int, float)) for h in hits)

        tree_hits = sum(
            1
            for r in model_server.requests
            if r["path"].startswith("/api/models/minishlab/potion-base-2M/tree/")
        )
        assert tree_hits == 1
    finally:
        _stop(rt)


def test_learn04_shippable_state_through_runtime(model_server, tmp_path):
    model_server.add_repo("minishlab/potion-base-2M", _tiny_static_model_repo())
    rt = _runtime(
        "04-shipping",
        "testapp_learn_04",
        tmp_path,
        {
            "HF_ENDPOINT": model_server.base,
            "PYLAIR_MODELS_ALLOW_INSECURE": "1",
        },
    )
    try:
        hello = rt.recv()
        names = {e["name"] for e in hello["params"]["exposes"]}
        assert {
            "list_notes",
            "read_note",
            "save_note",
            "scan_notes",
            "analyze_note",
            "search_notes",
        } <= names

        rt.send(_call(1, "search_notes", 1, {"query": "offline desktop notes"}))
        result = rt.recv_until(lambda m: m.get("id") == 1, timeout=30)
        assert "result" in result, result
        assert {h["name"] for h in result["result"]} == {"welcome", "ideas", "projects"}
    finally:
        _stop(rt)


def _compile_test_helper(tmp_path: Path) -> Path:
    suffix = ".exe" if sys.platform == "win32" else ""
    out = tmp_path / f"pylair-test-helper{suffix}"
    subprocess.run(
        [
            "rustc",
            "-O",
            str(REPO_ROOT / "fixtures" / "test-helper" / "main.rs"),
            "-o",
            str(out),
        ],
        check=True,
    )
    return out


def test_learn05_binary_export_returns_a_blob(model_server, tmp_path):
    model_server.add_repo("minishlab/potion-base-2M", _tiny_static_model_repo())
    helper = _compile_test_helper(tmp_path)
    blob_dir = tmp_path / "blobs"
    blob_dir.mkdir()
    rt = _runtime(
        "05-going-further",
        "testapp_learn_05",
        tmp_path,
        {
            "HF_ENDPOINT": model_server.base,
            "PYLAIR_MODELS_ALLOW_INSECURE": "1",
            "PYLAIR_BINARIES": json.dumps({"tectonic": str(helper)}),
            "PYLAIR_BLOB_DIR": str(blob_dir),
        },
    )
    try:
        rt.recv()
        rt.send(_call(1, "export_note_pdf", 1, {"name": "welcome"}))
        register = rt.recv_until(
            lambda m: m.get("method") == "blob.register" or m.get("id") == 1,
            timeout=20,
        )
        assert register.get("method") == "blob.register", register
        assert register["params"]["task_id"] == 1
        assert register["params"]["mime"] == "application/pdf"
        payload = Path(register["params"]["path"]).read_bytes()
        assert payload.startswith(b"%PDF-")

        response = rt.recv_until(lambda m: m.get("id") == 1, timeout=20)
        tag = response["result"]["$blob"]
        assert tag["id"] == register["params"]["id"]
        assert tag["mime"] == "application/pdf"
        assert tag["size"] == len(payload)
    finally:
        _stop(rt)


def test_learn05_tectonic_download_failure_maps_to_user_message(model_server, tmp_path):
    model_server.add_repo("minishlab/potion-base-2M", _tiny_static_model_repo())
    helper = _compile_test_helper(tmp_path)
    rt = _runtime(
        "05-going-further",
        "testapp_learn_05",
        tmp_path,
        {
            "HF_ENDPOINT": model_server.base,
            "PYLAIR_MODELS_ALLOW_INSECURE": "1",
            "PYLAIR_BINARIES": json.dumps({"tectonic": str(helper)}),
            "PYLAIR_BLOB_DIR": str(tmp_path / "blobs"),
            "PYLAIR_TEST_HELPER_NETWORK_FAILURE": "1",
        },
    )
    try:
        rt.recv()
        rt.send(_call(1, "export_note_pdf", 1, {"name": "welcome"}))
        response = rt.recv_until(lambda m: m.get("id") == 1, timeout=20)
        error = response["error"]
        assert error["message"] == (
            "First PDF export needs an internet connection to download typesetting files"
        )
        assert error["data"]["type"] == "RuntimeError"
    finally:
        _stop(rt)
