"""pylair.__version__ comes from the installed dist-info and matches
pyproject.toml; a source checkout with no install falls back cleanly."""

import tomllib
from importlib.metadata import PackageNotFoundError
from pathlib import Path

import pylair

PYPROJECT = Path(__file__).resolve().parents[1] / "pyproject.toml"


def _norm(v: str) -> str:
    # PEP 440 normalizes "0.1.0-rc.9" -> "0.1.0rc9"; strip the separators
    # so the comparison works on the normalized dist-info value too.
    return v.lower().replace("-", "").replace(".", "").replace("_", "")


def test_version_matches_pyproject():
    proj_version = tomllib.loads(PYPROJECT.read_text())["project"]["version"]
    assert _norm(pylair.__version__) == _norm(proj_version)
    assert pylair.__version__ != "0.0.0"


def test_version_fallback_when_not_installed(monkeypatch):
    def boom(name):
        raise PackageNotFoundError(name)

    monkeypatch.setattr(pylair, "version", boom)
    assert pylair._version() == "0.0.0+unknown"
