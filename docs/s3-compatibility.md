# S3 compatibility — what is proven on the wire, and what is inferred

Status: maintained with every bucket-touching change (WO6-4). This file separates
**proven** (executed against a real S3 implementation) from **inferred** (documented
provider behavior we have not yet reproduced for lack of credentials). No guessing.

## Test boundary (legal/ethical, non-negotiable)

The S3 conformance suite (`cairn-x s3-conformance`, `just s3-conformance`) runs against
servers the operator OWNS or a local ephemeral implementation (MinIO in CI). It refuses
to run without `--i-own-the-target`. Buckets discovered via public indexes (GrayHatWarfare
et al.) belong to other people: an "open" bucket is a *misconfiguration by its owner*, and
listing/reading/writing it is unauthorized access regardless of intent. The correct way to
"learn about S3s" is (a) run the conformance suite against your own MinIO/bucket and
(b) read the providers' public documentation — which is what the inferences below cite.

## Proven on the wire (MinIO RELEASE.2025-09-07, strict `MINIO_SITE_REGION`; CI job
`s3-wire-conformance` runs the same suite on every push)

| # | Check | Result | Evidence |
|---|---|---|---|
| 1 | CreateBucket via header-auth PUT (path-style, `/{bucket}` canonical URI) | ✅ 200 / 409 owned | conformance check 0 |
| 2 | Presigned PUT, host-only SignedHeaders, `UNSIGNED-PAYLOAD`, 64 KB | ✅ 200 | check 1 |
| 3 | Presigned GET byte-identity | ✅ 64,000/64,000 | check 2 |
| 4 | Presigned GET `Range: bytes=1000-1099` → `206` exact slice | ✅ (chunk-hydration resume path) | check 3 |
| 5 | Checksum-bound presign (`x-amz-checksum-sha256` in SignedHeaders) rejects corrupt body | ✅ 403 | check 4 |
| 6 | Expired presign URL → 403 | ✅ | check 5 |
| 7 | Tampered `X-Amz-Signature` → 403 | ✅ | check 6 |
| 8 | Header-auth PUT/HEAD/GET/DELETE (server-side manifest/pack paths) | ✅ all | check 7 |
| 9 | Wrong-region signature → 400 (with `MINIO_SITE_REGION` set) | ✅ | check 8 |

**Real quirk found while proving #9**: MinIO *without* `MINIO_SITE_REGION` silently
ACCEPTS wrong-region presigned GETs (200). Region enforcement is a server policy, not a
wire guarantee — production deployments must set the site region (MinIO) / use the correct
region string (AWS). Recorded in the conformance tool's output detail.

## Proven on the wire (Cloudflare R2 — cairn-prod bucket, 2026-09-02, 5GB REAL-S3 soak)

| # | Check | Result | Evidence |
|---|---|---|---|
| R1 | Header-auth SigV4 LIST (stdlib python, canonicalized `%2F` query) | ✅ 200 | `scripts/r2_list_canary.py` |
| R2 | Query-auth (presigned) GET, host-only, `UNSIGNED-PAYLOAD` | ✅ 200 | `scripts/r2_auth_matrix.py` C2 |
| R3 | Query-auth (presigned) PUT, host-only SignedHeaders, `UNSIGNED-PAYLOAD` (header sent unsigned or absent) | ❌ **403 SignatureDoesNotMatch** | auth-matrix C1/C3 pair; both python-stdlib and cairn daemon |
| R4 | Same presigned PUT **with `x-amz-content-sha256` IN SignedHeaders** (`host;x-amz-content-sha256`) + header on the wire | ✅ 200 | auth-matrix V5 (Rust's exact Z-suffixed `X-Amz-Date`, TTL 3600); then the full 5GB soak data plane |
| R5 | Header-auth PUT with `x-amz-content-sha256` header | ✅ 200 | auth-matrix C4 |
| R6 | Presigned GET, host-only, Z-suffixed `X-Amz-Date`, header absent or signed | ✅ 200 (both forms) | auth-matrix G1/G2 (8 MiB canary read-back) |

**R2 quirk (fix shipped in `SigV4Presigner::presign_put_host_only` +
`cairn-sync/plane_grpc.rs::put_presigned`)**: R2's presigned PUT requires EVERY
`x-amz-*` request header to be part of `X-Amz-SignedHeaders` — an unsigned
`x-amz-content-sha256` or `x-amz-checksum-sha256` fails with a *misleading*
`SignatureDoesNotMatch` 403 even when the signature math is byte-correct (V8
vs V9 isolates it: only the header's SIGNED status changes). The presigner now
binds `x-amz-content-sha256: UNSIGNED-PAYLOAD` into SignedHeaders and the
daemon sends exactly that header; the (previously unsigned) checksum header is
dropped on this path — the server cannot bind a body SHA-256 it does not know,
integrity stays with CompleteUpload BLAKE3 sample-verify + verified ranged
reads (SPEC §9.2), and checksum-BOUND sessions (`SigV4Presigner::presign_put`,
already R2-proven in V9/C5c) are the follow-up once clients ship per-chunk
SHA-256s at session creation. Host-only PUT signing — the shape AWS S3 and
MinIO accept — is what made MinIO CI conformance stay green while the
real-bucket soak failed. Presigned GET needs none of this (proven both forms,
R6). The signer's KDF and string-to-sign math needed no change (AWS
known-answer vector still green).

## Proven signing-math correctness (no bucket involved)

- AWS-published known-answer vector (IAM ListUsers header-auth example) — byte-exact
  (`sigv4_aws_known_answer_vector` test).
- Path-style presign shape + determinism (`sigv4_path_style_shape_and_determinism`).
- `amz_date` civil-from-days conversion — 2023 AND 2026 vectors (`amz_date_2026_vectors`).

## Inferred from public provider documentation (pending credentials to prove)

| Behavior | AWS S3 | Cloudflare R2 | Backblaze B2 (S3 API) |
|---|---|---|---|
| Virtual-host addressing (`bucket.host`) | ✅ canonical | ✅ `bucket.<account>.r2.cloudflarestorage.com` | ✅ |
| Path-style addressing (`host/bucket`) | ✅ accepted | ✅ accepted | ✅ (recommended) |
| Region in signing scope | enforced (wrong region → `SignatureDoesNotMatch`/`AuthorizationHeaderMalformed`) | ignored (`auto`) | enforced per-region string |
| Presign max TTL | 7d (SigV4) — Cairn caps at 1h by policy (SPEC §9) | 7d | 7d |
| `x-amz-checksum-sha256` on PUT | enforced when signed | supported | supported |
| Range GET on presigned URLs | ✅ | ✅ | ✅ |

The moment `CAIRN_S3_*` credentials exist for R2/AWS, `just s3-conformance` against them
moves every row above from "inferred" to "proven" with zero code changes (env-driven,
`--path-style=false` for the vhost form). Until then this table is the honest statement.

## Addressing-style guidance (from the conformance work)

- Default (`CAIRN_S3_PATH_STYLE` unset): **virtual-host** — right choice for AWS/R2/B2,
  URLs carry the bucket in the host.
- `CAIRN_S3_PATH_STYLE=1`: **path-style** — REQUIRED for MinIO-on-localhost and common for
  self-hosted gateways (no wildcard DNS); proven end-to-end here. Both forms sign
  identical canonical requests apart from the URI path (bucket in path vs host).
