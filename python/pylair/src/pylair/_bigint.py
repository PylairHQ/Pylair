"""pylair.BigInt — opt-in exact big integers over the wire.

An int subclass: a value typed BigInt ALWAYS encodes as
``{"$bigint": "<decimal>"}`` on the way out, decodes back to BigInt on
the way in, and maps to TS ``bigint`` in generated clients (docs/codegen.md).
Plain int is unchanged: JSON numbers inside ±2^53-1, a clear error outside.
"""

from __future__ import annotations

import re

BIGINT_TAG = "$bigint"
# Hostile peers can ship a digit string of any length; parsing a
# megabyte-long integer is quadratic work for a 16-byte frame. The cap
# (~12k bits) is far beyond any real id/counter need.
BIGINT_MAX_DIGITS = 4096
# Canonical decimal only: optional minus, no leading zeros. Anything
# else stays plain data — decode is defensive like $b64/$blob.
BIGINT_RE = re.compile(r"-?(0|[1-9][0-9]*)")


class BigInt(int):
    """An int that always crosses the bridge as ``{"$bigint": ...}``.

    Use it on the value (``return pylair.BigInt(n)``) or as a signature
    annotation — codegen then emits TS ``bigint`` instead of ``number``.
    """

    __slots__ = ()
