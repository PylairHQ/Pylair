# m9-models spike — throwaway evidence for decision 0009

Proof code backing `docs/decisions/0009-model-manager.md`. Not product
code — the implementation PRs rewrite it as the real test suite.

- `tls_probe.py` — HTTPS positive/negative probe run by
  `.github/workflows/spike-tls.yml` (on the `models-0009` branch) against
  the exact shipped python-build-standalone artifact per OS. Results:
  Windows 200/refused via system store (410 CAs), macOS 200/refused via
  `/private/etc/ssl/cert.pem` (126 CAs), Ubuntu 200/refused via
  `capath=/etc/ssl/certs`.
- `lock_proof.py` — byte-range lock contention + release-on-death.
  Measured on Windows (msvcrt): second process blocked while held,
  acquired after release and after holder termination.
- `rehash_bench.py` — sha256 re-hash cost of a multi-GB `.part` on
  resume (hashlib state cannot persist). Measured 1.3 GiB/s on Windows.
