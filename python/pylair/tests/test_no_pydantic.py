"""pylair must import and work with pydantic NOT installed (decision 0012 §1).
A meta-path blocker makes pydantic unimportable inside a subprocess that
then exercises the full pylair surface — including the detection API."""

import subprocess
import sys

CHILD = r"""
import sys


class Blocker:
    def find_module(self, name, path=None):
        if name == "pydantic" or name.startswith("pydantic."):
            return self
        return None

    def load_module(self, name):
        raise ImportError(f"pydantic is blocked in this test: {name}")

    # py3.12+ machinery
    def find_spec(self, name, path=None, target=None):
        if name == "pydantic" or name.startswith("pydantic."):
            raise ImportError(f"pydantic is blocked in this test: {name}")
        return None


sys.meta_path.insert(0, Blocker())

# the whole pylair surface imports clean
import pylair
from pylair import expose
from pylair import _json, _schema, _pydantic

# detection answers False, never raises
assert not _pydantic.pydantic_loaded()
assert not _pydantic.is_model_class(int)
assert not _pydantic.is_v1_model_class(int)

# the dataclass path works end to end
import dataclasses


@dataclasses.dataclass
class DC:
    name: str


@expose
def echo(item: DC) -> DC:
    return item


out = _json.coerce_kwargs(echo, {"item": {"name": "x"}})
assert out["item"].name == "x"

# schema walking works
sig = pylair._expose._signature_of(echo, None)
assert sig["params"]

# pydantic STILL not loaded — nothing in pylair triggered the import
assert "pydantic" not in sys.modules, "pylair must never import pydantic"
print("NO-PYDANTIC-OK")
"""


def test_pylair_works_without_pydantic():
    proc = subprocess.run(  # noqa: S603 - fixed test input
        [sys.executable, "-c", CHILD],
        capture_output=True,
        text=True,
        cwd=str(__import__("pathlib").Path(__file__).parent),
    )
    assert "NO-PYDANTIC-OK" in proc.stdout, (
        f"stdout: {proc.stdout}\nstderr: {proc.stderr[-800:]}"
    )
