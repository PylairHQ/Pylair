"""pylair._snapshots — the multi-file fetch core (decision 0013 PR 1).

Loopback-only: the fixture server emulates the tree API (including
Link-header pagination), resolve URLs that 302 to a second-origin
"CDN", LFS and git-blob file kinds. Covered: both verifiers and a
wrong-oid failure naming the file, allow/ignore, crash injection via
a killed subprocess proving no <revision>/ without .ready, resume
skipping verified files and ranging .parts, concurrent fetch
wait-then-noop, disk preflight, the non-HF manifest path with its
synthetic revision, bad revisions, fan-out cancellation, auth headers
never crossing origins. No sleeps for timing — every wait polls state
or releases a server-side gate.
"""

from __future__ import annotations

import hashlib
import json
import os
import subprocess
import sys
import threading
import time
from pathlib import Path

import pytest

from models_server import File, ModelServer, Repo
from pylair import _snapshots, models
from pylair.models import ModelError

REV = "a" * 40
REV2 = "b" * 40


@pytest.fixture(autouse=True)
def _state(monkeypatch, tmp_path):
    monkeypatch.setenv("PYLAIR_APP_DATA_DIR", str(tmp_path))
    monkeypatch.setenv("PYLAIR_MODELS_ALLOW_INSECURE", "1")
    models._reset()
    _snapshots._reset()
    yield
    models._reset()
    _snapshots._reset()


@pytest.fixture
def server():
    for _ in range(6):
        srv = ModelServer()
        if srv.probe():
            break
        srv.close()
    else:
        pytest.fail("loopback server never accepted a connection")
    yield srv
    srv.close()


def _wait(pred, timeout: float = 15.0, what: str = "condition") -> None:
    deadline = time.monotonic() + timeout
    while not pred():
        if time.monotonic() > deadline:
            raise AssertionError(f"{what} never met")
        time.sleep(0.005)


def _sha(body: bytes) -> str:
    return hashlib.sha256(body).hexdigest()


def _std_repo() -> Repo:
    """Small files exercising blob-sha1 plus one LFS weight file,
    nested paths included."""
    return Repo(
        REV,
        {
            "config.json": b'{"model_type": "tiny"}',
            "tokenizer.json": b'{"version": 1}' * 40,
            "nested/vocab.json": b'{"a": 1}' * 30,
            "model.safetensors": os.urandom(120_000),
        },
        lfs={"model.safetensors"},
    )


def _fetch(name, server, **kw):
    kw.setdefault("endpoint", server.base)
    return _snapshots.fetch_snapshot(name, **kw)


# ---- verifiers + layout ----------------------------------------------------


def test_empty_blob_sha1_known_vector():
    """git's empty blob is the well-known e69de29b… — the framing
    construction is proven against git itself, not just self-consistency."""
    assert _snapshots._git_blob_oid(b"") == "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"


def test_fetch_mixed_snapshot(server, tmp_path):
    r = _std_repo()
    server.add_repo("org/model", r)
    out = _fetch("m1", server, repo="org/model", revision=REV)
    rev_dir = tmp_path / "models" / "m1" / REV
    assert out == rev_dir
    for path, body in r.files.items():
        assert (rev_dir / path).read_bytes() == body, path
    # layout bookkeeping
    ready = json.loads((rev_dir / ".ready").read_text())
    assert ready["revision"] == REV
    assert ready["file_count"] == len(r.files)
    assert ready["total_bytes"] == sum(len(b) for b in r.files.values())
    manifest = json.loads((rev_dir / ".manifest.json").read_text())
    kinds = {e["path"]: e["kind"] for e in manifest["entries"]}
    assert kinds["model.safetensors"] == "sha256"
    assert kinds["config.json"] == "blob-sha1"
    verify = json.loads((rev_dir / ".verify.json").read_text())
    assert set(verify["files"]) == set(r.files)
    # staging is gone, no residue
    assert not (tmp_path / "models" / "m1" / f"{REV}.partial").exists()
    # directory entries in the tree were skipped, not fetched
    assert not (rev_dir / "nested").is_file()


def test_wrong_oid_fails_naming_file(server, tmp_path):
    r = _std_repo()
    server.add_repo("org/model", r)
    # corrupt the LFS entry's advertised oid after registration
    repo = server.repos[("org/model", REV)]
    orig = repo.entries

    def bad_entries():
        ents = orig()
        for e in ents:
            if e["path"] == "model.safetensors":
                e["lfs"]["oid"] = "0" * 64
        return ents

    repo.entries = bad_entries
    with pytest.raises(ModelError, match=r"model\.safetensors"):
        _fetch("m1", server, repo="org/model", revision=REV)
    # every small file verified fine; the snapshot never completed
    assert not (tmp_path / "models" / "m1" / REV).exists()


def test_blob_kind_wrong_oid_fails(server, tmp_path):
    r = _std_repo()
    server.add_repo("org/model", r)
    repo = server.repos[("org/model", REV)]
    orig = repo.entries

    def bad_entries():
        ents = orig()
        for e in ents:
            if e["path"] == "config.json":
                e["oid"] = "0" * 40
        return ents

    repo.entries = bad_entries
    with pytest.raises(ModelError, match=r"config\.json.*blob sha1"):
        _fetch("m1", server, repo="org/model", revision=REV)


# ---- globs -------------------------------------------------------------------


def test_allow_ignore(server, tmp_path):
    r = _std_repo()
    server.add_repo("org/model", r)
    out = _fetch(
        "m1",
        server,
        repo="org/model",
        revision=REV,
        allow=["*.json", "*.safetensors"],
        ignore=["nested/*"],
    )
    names = sorted(
        p for p in out.rglob("*") if p.is_file() and not p.name.startswith(".")
    )
    assert [str(p.relative_to(out)).replace(os.sep, "/") for p in names] == [
        "config.json",
        "model.safetensors",
        "tokenizer.json",
    ]
    # fnmatch * crosses slashes — documented semantics
    hits = [h for h in server.requests if "/resolve/" in h["path"]]
    assert not any("vocab" in h["path"] for h in hits)


def test_allow_excluding_everything_is_an_error(server):
    r = _std_repo()
    server.add_repo("org/model", r)
    with pytest.raises(ModelError, match="nothing to fetch"):
        _fetch("m1", server, repo="org/model", revision=REV, allow=["*.gguf"])


# ---- revisions + args --------------------------------------------------------


@pytest.mark.parametrize(
    "bad", ["main", "v1.0", "a" * 39, "a" * 41, "A" * 40, "g" * 40, "", None]
)
def test_bad_revision_rejected(bad):
    with pytest.raises(ValueError, match="full 40-character"):
        _snapshots.fetch_snapshot("m1", repo="org/model", revision=bad)


def test_mutually_exclusive_sources(server):
    with pytest.raises(ValueError, match="mutually exclusive"):
        _snapshots.fetch_snapshot(
            "m1",
            repo="org/model",
            revision=REV,
            base_url="https://x/",
            manifest=[{"path": "a", "sha256": "0" * 64, "size": 1}],
        )
    with pytest.raises(ValueError, match="required"):
        _snapshots.fetch_snapshot("m1")


def test_repo_paths_validated():
    for bad in [
        "../x",
        "a/../b",
        "/abs",
        "a\\b",
        "",
        "a//b",
        ".ready",
        ".parts/x",
        "a\x01b",
    ]:
        with pytest.raises(ValueError):
            _snapshots._check_repo_path(bad)
    for ok in ["config.json", "nested/deep/file.bin", ".gitattributes", "x.y"]:
        assert _snapshots._check_repo_path(ok) == ok


def test_repo_paths_device_names_rejected():
    """Windows reserved device names alias the device with or without
    an extension, in any segment — a remote tree must never write one."""
    for bad in ["aux.json", "nul/x", "CON", "com1", "lpt9.txt", "sub/PRN.bin"]:
        with pytest.raises(ValueError, match="device"):
            _snapshots._check_repo_path(bad)
    for ok in ["auxiliary", "com10", "nulll.json", "console.bin"]:
        assert _snapshots._check_repo_path(ok) == ok


def test_repo_paths_trailing_dot_space_rejected():
    """Windows strips trailing dots and spaces, so "a." aliases "a" —
    another silent overwrite."""
    for bad in ["a./f", "sub/b ", "x.txt.", "y/z "]:
        with pytest.raises(ValueError, match="dot or space"):
            _snapshots._check_repo_path(bad)
    for ok in ["a.b/f", "sub/dir with spaces/f.txt"]:
        assert _snapshots._check_repo_path(ok) == ok


def test_casefold_collision_rejects_plan():
    entries = [
        _snapshots._Entry("README.md", 3, "0" * 40, "blob-sha1"),
        _snapshots._Entry("readme.md", 3, "1" * 40, "blob-sha1"),
    ]
    with pytest.raises(ValueError, match="README.md.*readme.md"):
        _snapshots._check_plan_collisions(entries)
    fine = [
        _snapshots._Entry("README.md", 3, "0" * 40, "blob-sha1"),
        _snapshots._Entry("readmex.md", 3, "1" * 40, "blob-sha1"),
    ]
    _snapshots._check_plan_collisions(fine)


def test_casefold_collision_in_tree_fails_before_download(server, tmp_path):
    r = Repo(REV, {"README.md": b"one", "readme.md": b"two"})
    server.add_repo("org/model", r)
    with pytest.raises(ValueError, match="collide"):
        _fetch("m1", server, repo="org/model", revision=REV)
    assert not any("/resolve/" in h["path"] for h in server.requests)
    assert not (tmp_path / "models" / "m1" / REV).exists()


def test_device_name_in_tree_fails_before_download(server, tmp_path):
    r = Repo(REV, {"aux.json": b"{}", "config.json": b"{}"})
    server.add_repo("org/model", r)
    with pytest.raises(ValueError, match="aux"):
        _fetch("m1", server, repo="org/model", revision=REV)
    assert not any("/resolve/" in h["path"] for h in server.requests)


def test_manifest_device_name_rejected():
    with pytest.raises(ValueError, match="device"):
        _snapshots._check_manifest([{"path": "nul.bin", "sha256": "0" * 64, "size": 1}])


def test_manifest_casefold_collision_rejected(server):
    manifest = [
        {"path": "README.md", "sha256": "0" * 64, "size": 1},
        {"path": "readme.md", "sha256": "1" * 64, "size": 1},
    ]
    with pytest.raises(ValueError, match="collide"):
        _snapshots.fetch_snapshot("m1", base_url=server.base + "/x/", manifest=manifest)


def test_tree_404_names_stage(server, tmp_path):
    with pytest.raises(ModelError, match=r"tree: 404"):
        _fetch("m1", server, repo="org/ghost", revision=REV)


# ---- tree pagination ---------------------------------------------------------


def test_tree_pagination(server, tmp_path):
    files = {f"file{i}.json": b'{"i": %d}' % i for i in range(5)}
    r = Repo(REV, files, page_size=2)
    server.add_repo("org/model", r)
    out = _fetch("m1", server, repo="org/model", revision=REV)
    for path, body in files.items():
        assert (out / path).read_bytes() == body
    tree_hits = [h for h in server.requests if "/tree/" in h["path"]]
    assert len(tree_hits) == 3  # ceil(6 entries / 2) — 5 files + no dirs here
    assert "cursor=2" in tree_hits[1]["path"]


# ---- redirects + auth --------------------------------------------------------


def test_auth_never_crosses_to_cdn(server, tmp_path):
    cdn = ModelServer()
    try:
        r = _std_repo()
        server.add_repo("org/model", r)
        cdn.add("/weights", File(r.files["model.safetensors"]))
        server.redirects[f"/org/model/resolve/{REV}/model.safetensors"] = (
            cdn.base + "/weights"
        )
        out = _fetch(
            "m1",
            server,
            repo="org/model",
            revision=REV,
            headers={"Authorization": "Bearer TOPSECRET"},
        )
        assert (out / "model.safetensors").read_bytes() == r.files["model.safetensors"]
        hdrs = cdn.last_headers("/weights")
        assert "authorization" not in hdrs
        # …while the same-origin tree request did carry it
        tree = next(h for h in server.requests if "/tree/" in h["path"])
        assert tree["headers"].get("Authorization") == "Bearer TOPSECRET"
        assert "TOPSECRET" not in json.dumps([h["headers"] for h in cdn.requests])
    finally:
        cdn.close()


# ---- repeat + preflight + progress -------------------------------------------


def test_repeat_fetch_zero_network(server, tmp_path):
    r = _std_repo()
    server.add_repo("org/model", r)
    _fetch("m1", server, repo="org/model", revision=REV)
    server.requests.clear()
    out = _fetch("m1", server, repo="org/model", revision=REV)
    assert out == tmp_path / "models" / "m1" / REV
    assert server.requests == []


def test_preflight_failure_human_units(server, tmp_path, monkeypatch):
    r = _std_repo()
    server.add_repo("org/model", r)

    class _Usage:
        total, used, free = 10**9, 10**9 - 10, 10

    monkeypatch.setattr("shutil.disk_usage", lambda p: _Usage())
    with pytest.raises(ModelError, match=r"not enough disk space: need .*have"):
        _fetch("m1", server, repo="org/model", revision=REV)
    assert not any("/resolve/" in h["path"] for h in server.requests)


def test_progress_callback_exact_total(server, tmp_path):
    r = _std_repo()
    server.add_repo("org/model", r)
    seen = []
    out = _fetch(
        "m1",
        server,
        repo="org/model",
        revision=REV,
        progress=lambda done, total, cur, fd, ft: seen.append((done, total, cur)),
    )
    assert out.exists()
    total = sum(len(b) for b in r.files.values())
    assert seen, "no progress emitted"
    assert all(t == total for _, t, _ in seen)
    dones = [d for d, _, _ in seen]
    assert dones == sorted(dones), "byte progress must be non-decreasing"
    assert dones[-1] == total
    assert any(isinstance(c, str) for _, _, c in seen)


# ---- non-HF manifest ---------------------------------------------------------


def test_nonhf_manifest_synthetic_revision(server, tmp_path):
    bodies = {"config.json": b"{}", "model.bin": os.urandom(50_000)}
    for path, body in bodies.items():
        server.add(f"/files/whisper/{path}", File(body))
    manifest = [
        {"path": p, "sha256": _sha(b), "size": len(b)} for p, b in bodies.items()
    ]
    entries = _snapshots._check_manifest(manifest)
    rev = _snapshots._synthetic_revision(entries)
    assert len(rev) == 40 and all(c in "0123456789abcdef" for c in rev)
    out = _snapshots.fetch_snapshot(
        "m1",
        base_url=server.base + "/files/whisper/",
        manifest=manifest,
    )
    assert out == tmp_path / "models" / "m1" / rev
    for path, body in bodies.items():
        assert (out / path).read_bytes() == body
    # a changed manifest is a changed snapshot (the upgrade semantic)
    entries2 = _snapshots._check_manifest(
        [{**manifest[0], "size": manifest[0]["size"]}, manifest[1]]
    )
    assert _snapshots._synthetic_revision(entries2) == rev  # identical input
    different = _snapshots._check_manifest(
        [{**manifest[0], "sha256": "0" * 64}, manifest[1]]
    )
    assert _snapshots._synthetic_revision(different) != rev


def test_nonhf_manifest_entry_without_hash_rejected():
    with pytest.raises(ValueError, match="sha256"):
        _snapshots._check_manifest([{"path": "a.bin", "size": 3}])
    with pytest.raises(ValueError, match="size"):
        _snapshots._check_manifest([{"path": "a.bin", "sha256": "0" * 64}])
    with pytest.raises(ValueError, match="non-empty"):
        _snapshots._check_manifest([])


# ---- concurrency -------------------------------------------------------------


def test_concurrent_fetch_waits_then_noops(server, tmp_path):
    r = _std_repo()
    server.add_repo("org/model", r)
    results, errors = [], []

    def go():
        try:
            results.append(_fetch("m1", server, repo="org/model", revision=REV))
        except Exception as e:  # noqa: BLE001 - collected, asserted below
            errors.append(e)

    t1 = threading.Thread(target=go)
    t1.start()
    _wait(
        lambda: any("/resolve/" in h["path"] for h in server.requests),
        what="first bytes",
    )
    t2 = threading.Thread(target=go)
    t2.start()
    t1.join(20)
    t2.join(20)
    assert not errors
    assert len(results) == 2
    assert results[0] == results[1]
    for path in r.files:
        hits = sum(1 for h in server.requests if h["path"].endswith(f"/{path}"))
        assert hits == 1, f"{path} downloaded {hits} times"


# ---- fan-out cancellation ----------------------------------------------------


def test_fanout_failure_cancels_rest(server, tmp_path):
    big = os.urandom(2_000_000)
    r = Repo(
        REV,
        {
            "a.json": b"{}",
            "b.json": b"{}",
            "c.json": b"{}",
            "ghost.json": b"this content is never served",
            "big.bin": big,
        },
        lfs={"big.bin"},
    )
    server.add_repo("org/model", r)
    # ghost.json is in the tree but 404s at resolve — non-retryable,
    # so the failure is immediate
    del server.files[f"/org/model/resolve/{REV}/ghost.json"]
    # big.bin crawls deterministically: ~400KB/s, so without
    # cancellation the fetch would take seconds — the 404 cancels it
    # in the first hundred milliseconds.
    server.files[f"/org/model/resolve/{REV}/big.bin"] = File(
        big, throttle=4096, burst_delay=0.01
    )
    with pytest.raises(ModelError, match=r"ghost\.json"):
        _fetch("m1", server, repo="org/model", revision=REV)
    part = tmp_path / "models" / "m1" / f"{REV}.partial" / ".parts" / "big.bin.part"
    size_at_end = part.stat().st_size if part.exists() else 0
    assert size_at_end < len(big), "the sibling download was not cancelled"
    assert not (tmp_path / "models" / "m1" / REV).exists()


# ---- crash + resume ------------------------------------------------------------


def _child_env(tmp_path: Path) -> dict:
    env = dict(os.environ)
    env["PYLAIR_APP_DATA_DIR"] = str(tmp_path)
    env["PYLAIR_MODELS_ALLOW_INSECURE"] = "1"
    return env


_CHILD = """
import sys
from pylair import _snapshots
_snapshots.fetch_snapshot(
    "m1", repo="org/model", revision=sys.argv[1], endpoint=sys.argv[2],
)
print("done", flush=True)
"""


def test_kill_mid_file_resumes(server, tmp_path):
    """A hard kill mid-shard leaves only .partial — never a <revision>/
    without .ready — and the re-fetch keeps verified files, resumes
    the .part by Range, and completes."""
    big = os.urandom(2_000_000)
    r = Repo(
        REV,
        {
            "config.json": b"{}",
            "big.bin": big,
        },
        lfs={"big.bin"},
    )
    server.add_repo("org/model", r)
    # Deterministic crash window: the first attempt dies server-side at
    # exactly 100KB (one-shot fault), the retry crawls at ~800KB/s, so
    # the child is always mid-transfer or in backoff when the kill
    # lands — never complete, never at byte zero.
    server.files[f"/org/model/resolve/{REV}/big.bin"] = File(
        big, drop_after=100_000, throttle=4096, burst_delay=0.005
    )
    staging = tmp_path / "models" / "m1" / f"{REV}.partial"
    part = staging / ".parts" / "big.bin.part"
    rev_dir = tmp_path / "models" / "m1" / REV

    child = subprocess.Popen(
        [sys.executable, "-c", _CHILD, REV, server.base],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=_child_env(tmp_path),
        text=True,
    )
    try:
        _wait(
            lambda: part.exists() and part.stat().st_size >= 100_000, what="part bytes"
        )
        # config.json is small: let it finish verifying first
        _wait(lambda: (staging / "config.json").exists(), what="small file verified")
        child.kill()
        child.wait(timeout=10)
    finally:
        if child.poll() is None:
            child.kill()
            child.wait(timeout=10)

    # crash invariant: either no revision dir, or one with .ready
    if rev_dir.exists():
        assert (rev_dir / ".ready").exists()
    else:
        assert staging.exists() or part.exists()

    partial_size = part.stat().st_size
    assert 0 < partial_size < len(big)
    cfg_hits_before = sum(
        1 for h in server.requests if h["path"].endswith("/config.json")
    )
    server.requests.clear()

    out = _fetch("m1", server, repo="org/model", revision=REV)
    assert (out / "big.bin").read_bytes() == big
    assert (out / "config.json").read_bytes() == b"{}"
    # the verified small file was never re-requested
    assert not any(h["path"].endswith("/config.json") for h in server.requests)
    # the big file resumed by Range from the banked offset
    ranges = [
        h["headers"].get("Range")
        for h in server.requests
        if h["path"].endswith("/big.bin") and "Range" in h["headers"]
    ]
    assert ranges and ranges[0] == f"bytes={partial_size}-"
    assert cfg_hits_before >= 1


def test_crash_before_rename_completes_by_rename(server, tmp_path):
    """A process dying after .ready but before the rename left a fully
    verified .partial — the next fetch performs the rename only, zero
    bytes re-downloaded."""
    r = _std_repo()
    server.add_repo("org/model", r)
    out1 = _fetch("m1", server, repo="org/model", revision=REV)
    # simulate the crash window: move the completed revision back to
    # its staging name, exactly as it looked the instant before rename
    rev_dir = tmp_path / "models" / "m1" / REV
    staging = tmp_path / "models" / "m1" / f"{REV}.partial"
    os.rename(rev_dir, staging)
    server.requests.clear()
    out2 = _fetch("m1", server, repo="org/model", revision=REV)
    assert out2 == out1
    assert not any("/resolve/" in h["path"] for h in server.requests)
    assert (out2 / "model.safetensors").read_bytes() == r.files["model.safetensors"]


def test_partial_never_returned(server, tmp_path):
    """A bare .partial with no .ready is staging, never a result:
    the fetch treats it as resume material, not a complete snapshot."""
    r = _std_repo()
    server.add_repo("org/model", r)
    staging = tmp_path / "models" / "m1" / f"{REV}.partial"
    staging.mkdir(parents=True)
    (staging / "config.json").write_bytes(b"forged")
    out = _fetch("m1", server, repo="org/model", revision=REV)
    # the forged file was not trusted: wrong size vs manifest, so it
    # was re-verified — the real bytes win
    assert (out / "config.json").read_bytes() == r.files["config.json"]


def test_revision_upgrade_stages_side_by_side(server, tmp_path):
    """A second revision for the same name leaves the first untouched —
    revision-addressed storage in action (GC is PR 2)."""
    r1 = _std_repo()
    server.add_repo("org/model", r1)
    out1 = _fetch("m1", server, repo="org/model", revision=REV)
    files2 = dict(r1.files)
    files2["config.json"] = b'{"model_type": "bigger"}'
    r2 = Repo(REV2, files2, lfs={"model.safetensors"})
    server.add_repo("org/model", r2)
    out2 = _fetch("m1", server, repo="org/model", revision=REV2)
    assert out1 != out2
    assert (out1 / "config.json").read_bytes() == b'{"model_type": "tiny"}'
    assert (out2 / "config.json").read_bytes() == b'{"model_type": "bigger"}'
