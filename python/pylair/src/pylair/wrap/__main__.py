"""`python -m pylair.wrap` entry point.

Exits with `os._exit`, never `sys.exit`: after the reader thread starts, a
daemon thread blocked on stdin can deadlock interpreter shutdown.
"""

import os
import sys

from pylair.wrap._launcher import main

if __name__ == "__main__":
    code = main()
    sys.stderr.flush()
    os._exit(code)
