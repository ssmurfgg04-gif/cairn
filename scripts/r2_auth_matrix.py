#!/usr/bin/env python3
"""R2 auth matrix: which combination validates?

C1 header-auth PUT     (is PUT allowed on this token at all?)
C2 query-auth  GET     (presigned GET on the known canary object)
C3 query-auth  PUT + x-amz-content-sha256: UNSIGNED-PAYLOAD header
C4 header-auth PUT with x-amz-content-sha256 header (canonical form)
"""
import hashlib
import hmac
import os
import urllib.request
from datetime import datetime, timezone

ENDPOINT = os.environ["CAIRN_S3_ENDPOINT"].rstrip("/")
BUCKET = os.environ["CAIRN_S3_BUCKET"]
AKID = os.environ["CAIRN_S3_ACCESS_KEY_ID"]
SECRET = os.environ["CAIRN_S3_SECRET_ACCESS_KEY"]
REGION = os.environ.get("CAIRN_S3_REGION", "auto")
HOST = ENDPOINT.split("://", 1)[1]
CANARY_KEY = "tcanary/c/4f/4f7d748b5dda756b27bed281d6de499c9d56ef857b087d86111fa14c79383f52"
PROBE_KEY = "tsoak-probe/probe.bin"


def uri_encode(s, slash=False):
    out = []
    for b in s.encode():
        c = chr(b)
        if c.isalnum() or c in "-._~" or (c == "/" and not slash):
            out.append(c)
        else:
            out.append(f"%{b:02X}")
    return "".join(out)


def signing_key(date_stamp):
    k = ("AWS4" + SECRET).encode()
    for part in (date_stamp.encode(), REGION.encode(), b"s3", b"aws4_request"):
        k = hmac.new(k, part, hashlib.sha256).digest()
    return k


def header_auth(method, key, extra_headers):
    now = datetime.now(timezone.utc)
    amz_date = now.strftime("%Y%m%dT%H%M%SZ")
    date_stamp = now.strftime("%Y%m%d")
    payload_hash = extra_headers.get("x-amz-content-sha256", hashlib.sha256(b"").hexdigest())
    headers = {"host": HOST, "x-amz-content-sha256": payload_hash, "x-amz-date": amz_date}
    for k, v in extra_headers.items():
        if k != "x-amz-content-sha256":
            headers[k] = v
    signed = ";".join(sorted(headers))
    canon_headers = "".join(f"{k}:{headers[k]}\n" for k in sorted(headers))
    canonical = "\n".join([method, f"/{BUCKET}/{key}", "", canon_headers, signed, payload_hash])
    sts = "\n".join([
        "AWS4-HMAC-SHA256", amz_date, f"{date_stamp}/{REGION}/s3/aws4_request",
        hashlib.sha256(canonical.encode()).hexdigest(),
    ])
    sig = hmac.new(signing_key(date_stamp), sts.encode(), hashlib.sha256).hexdigest()
    auth = f"AWS4-HMAC-SHA256 Credential={AKID}/{date_stamp}/{REGION}/s3/aws4_request, SignedHeaders={signed}, Signature={sig}"
    req = urllib.request.Request(
        f"{ENDPOINT}/{BUCKET}/{key}",
        data=(b"probe" * 8 if method == "PUT" else None),
        method=method,
        headers={"Authorization": auth, **headers},
    )
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            return f"HTTP {r.status}"
    except urllib.error.HTTPError as e:
        return f"HTTP {e.code}: " + e.read().decode()[:120].replace("\n", " ")


def query_auth(method, key, payload_hash, extra_headers=None, ttl=600):
    now = datetime.now(timezone.utc)
    amz_date = now.strftime("%Y%m%dT%H%M%S")
    date_stamp = now.strftime("%Y%m%d")
    scope = f"{date_stamp}/{REGION}/s3/aws4_request"
    cred = uri_encode(f"{AKID}/{scope}", slash=True)
    q = [
        f"X-Amz-Algorithm=AWS4-HMAC-SHA256",
        f"X-Amz-Credential={cred}",
        f"X-Amz-Date={amz_date}",
        f"X-Amz-Expires={ttl}",
    ]
    hdrs = dict(extra_headers or {})
    if hdrs:
        q.append("X-Amz-SignedHeaders=" + uri_encode(";".join(sorted(["host"] + list(hdrs)))))
    else:
        q.append("X-Amz-SignedHeaders=host")
    canonical_query = "&".join(q)
    all_headers = {"host": HOST, **hdrs}
    signed = ";".join(sorted(all_headers))
    canon_headers = "".join(f"{k}:{all_headers[k]}\n" for k in sorted(all_headers))
    canonical = "\n".join([method, f"/{BUCKET}/{key}", canonical_query, canon_headers, signed, payload_hash])
    sts = "\n".join([
        "AWS4-HMAC-SHA256", amz_date + "Z" if len(amz_date) == 15 else amz_date,
        scope, hashlib.sha256(canonical.encode()).hexdigest(),
    ])
    sig = hmac.new(signing_key(date_stamp), sts.encode(), hashlib.sha256).hexdigest()
    url = f"{ENDPOINT}/{BUCKET}/{key}?{canonical_query}&X-Amz-Signature={sig}"
    req = urllib.request.Request(
        url,
        data=(b"probe" * 8 if method == "PUT" else None),
        method=method,
        headers=hdrs or {},
    )
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            return f"HTTP {r.status}"
    except urllib.error.HTTPError as e:
        return f"HTTP {e.code}: " + e.read().decode()[:120].replace("\n", " ")


if __name__ == "__main__":
    print("C1 header-auth PUT            :", header_auth("PUT", PROBE_KEY, {}))
    print("C2 query-auth GET (canary)    :", query_auth("GET", CANARY_KEY, "UNSIGNED-PAYLOAD"))
    print("C3 query PUT UNSIGNED+hdr     :", query_auth("PUT", PROBE_KEY, "UNSIGNED-PAYLOAD",
                                                       {"x-amz-content-sha256": "UNSIGNED-PAYLOAD"}))
    print("C4 header-auth PUT sha256 hdr :", header_auth("PUT", PROBE_KEY,
                                                       {"x-amz-content-sha256": "UNSIGNED-PAYLOAD"}))
