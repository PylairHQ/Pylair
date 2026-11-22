"""m9-models spike: cost of re-hashing a multi-GB .part on resume.

hashlib state cannot be persisted, so a resumed download re-hashes the
existing .part before sending the Range request. This measures that
cost: write N GiB to a temp file, sha256 it in 4 MiB reads.

Measured on Windows 11 (NVMe, mid-range): 2 GiB in 1.56 s
(~1.3 GiB/s) => a 5 GiB model re-hashes in ~4 s, a 20 GiB model in
~15 s. Negligible next to the download itself.
"""

import hashlib
import os
import sys
import tempfile
import time

GIB = int(sys.argv[1]) if len(sys.argv) > 1 else 2


def main() -> None:
    f = tempfile.NamedTemporaryFile(delete=False, suffix=".part")
    try:
        chunk = os.urandom(1 << 24)  # 16 MiB
        total = 0
        for _ in range(GIB * 64):
            f.write(chunk)
            total += len(chunk)
        f.close()

        h = hashlib.sha256()
        got = 0
        t0 = time.monotonic()
        with open(f.name, "rb") as r:
            while b := r.read(1 << 22):
                h.update(b)
                got += len(b)
        dt = time.monotonic() - t0
        print(f"sha256 {got / 2**30:.1f} GiB in {dt:.2f}s -> {got / dt / 2**20:.0f} MiB/s")
    finally:
        f.close()
        os.unlink(f.name)


if __name__ == "__main__":
    main()
