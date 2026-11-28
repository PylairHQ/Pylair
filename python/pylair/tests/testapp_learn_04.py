"""Bridge the examples/learn/04-shipping backend into the runtime harness.

The example stays a real standalone app; this wrapper only puts its directory
on sys.path so `python -m pylair` and its worker subprocesses import the same
module users run.
"""

import sys
from pathlib import Path

EXAMPLE = (Path(__file__).parent / "../../../examples/learn/04-shipping").resolve()
if str(EXAMPLE) not in sys.path:
    sys.path.insert(0, str(EXAMPLE))

from backend.app import *
