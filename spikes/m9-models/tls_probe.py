"""m9-models spike: TLS probe for the bundled python-build-standalone
runtime — the same probes .github/workflows/spike-tls.yml runs on the
three OS legs against the exact shipped PBS artifact.

Positive: https://pypi.org/simple/ must return 200.
Negative: https://self-signed.badssl.com must fail with
CERTIFICATE_VERIFY_FAILED (proving verification is real, not disabled).

Also prints the default verify paths and loaded CA count for the
evidence table in docs/decisions/0009-model-manager.md.
"""

import os
import ssl
import sys
import urllib.request


def main() -> int:
    print(
        "SSL_CERT envs:",
        os.environ.get("SSL_CERT_FILE"),
        os.environ.get("SSL_CERT_DIR"),
    )
    print("paths:", ssl.get_default_verify_paths())
    ctx = ssl.create_default_context()
    print("default-context CA count:", len(ctx.get_ca_certs()))
    for p in ("/etc/ssl/cert.pem", "/etc/ssl/certs/ca-certificates.crt"):
        print(p, "exists:", os.path.exists(p))

    r = urllib.request.urlopen("https://pypi.org/simple/", timeout=15)
    print("pypi.org simple ->", r.status)
    if r.status != 200:
        return 1

    try:
        urllib.request.urlopen("https://self-signed.badssl.com/", timeout=15)
    except Exception as e:  # noqa: BLE001 — spike probe, want any ssl error
        reason = getattr(e, "reason", e)
        print("refused as expected:", reason)
        return 0
    print("ERROR: self-signed accepted")
    return 1


if __name__ == "__main__":
    sys.exit(main())
