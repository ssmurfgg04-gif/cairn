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
