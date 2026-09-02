#!/usr/bin/env python3
"""Probe: R2 presigned PUT (query-auth SigV4) — which payload-hash form does R2 accept?

Variants:
  unsigned  — canonical request payload hash = literal "UNSIGNED-PAYLOAD" (cairn's form)
  empty     — canonical request payload hash = sha256("") (the other standard presign form)

The cairn 5GB REAL-S3 soak got 403 SignatureDoesNotMatch on presigned PUT against
R2 while MinIO (CI) accepted the same URLs. This probe isolates the accepted form.
"""
import hashlib
import hmac
import os
import sys
import urllib.request
from datetime import datetime, timezone

ENDPOINT = os.environ["CAIRN_S3_ENDPOINT"].rstrip("/")
BUCKET = os.environ["CAIRN_S3_BUCKET"]
AKID = os.environ["CAIRN_S3_ACCESS_KEY_ID"]
SECRET = os.environ["CAIRN_S3_SECRET_ACCESS_KEY"]
REGION = os.environ.get("CAIRN_S3_REGION", "auto")
HOST = ENDPOINT.split("://", 1)[1]
KEY = "tsoak-probe/probe.bin"


def uri_encode(s, slash=False):
    out = []
    for b in s.encode():
        c = chr(b)
        if c.isalnum() or c in "-._~" or (c == "/" and not slash):
            out.append(c)
        else:
            out.append(f"%{b:02X}")
    return "".join(out)


def presign(method, payload_hash, ttl=600):
    now = datetime.now(timezone.utc)
    amz_date = now.strftime("%Y%m%dT%H%M%S")
    date_stamp = now.strftime("%Y%m%d")
    scope = f"{date_stamp}/{REGION}/s3/aws4_request"
    cred = uri_encode(f"{AKID}/{scope}", slash=True)
    signed_headers = "host"
    canonical_query = (
        f"X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Credential={cred}"
        f"&X-Amz-Date={amz_date}&X-Amz-Expires={ttl}&X-Amz-SignedHeaders={signed_headers}"
    )
    canonical_headers = f"host:{HOST}\n"
    canonical_request = "\n".join(
        [method, f"/{BUCKET}/{KEY}", canonical_query, canonical_headers, signed_headers, payload_hash]
    )
    sts = "\n".join(
        ["AWS4-HMAC-SHA256", amz_date, scope, hashlib.sha256(canonical_request.encode()).hexdigest()]
    )
    k = ("AWS4" + SECRET).encode()
    for part in (date_stamp.encode(), REGION.encode(), b"s3", b"aws4_request"):
        k = hmac.new(k, part, hashlib.sha256).digest()
    sig = hmac.new(k, sts.encode(), hashlib.sha256).hexdigest()
    return f"{ENDPOINT}/{BUCKET}/{KEY}?{canonical_query}&X-Amz-Signature={sig}"


def probe(name, payload_hash, body):
    url = presign("PUT", payload_hash)
    req = urllib.request.Request(url, data=body, method="PUT")
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            print(f"{name}: HTTP {r.status} OK")
            return 0
    except urllib.error.HTTPError as e:
        msg = e.read().decode()[:160].replace("\n", " ")
        print(f"{name}: HTTP {e.code}: {msg}")
        return 2


if __name__ == "__main__":
    body = b"cairn presign probe " * 3
    rc = probe("unsigned-payload", "UNSIGNED-PAYLOAD", body)
    rc += probe("empty-hash", hashlib.sha256(b"").hexdigest(), body)
    # GET probe (read-back) with the form that must mirror whatever PUT succeeded
    sys.exit(rc)
