# ADR-0005: Object-store trait + internal SigV4 presigner (aws-sdk-s3 excluded from core deps)

Date: 2026-08-31 · Status: Accepted

## Context
Spec §9/§12/§17: client ↔ bucket direct, presigned PUT with checksums, signed range-capable GETs,
tiering to B2. §17 lists `aws-sdk-s3`. The full AWS SDK is a very large dependency for a single
usage pattern (presign PUT/GET + HEAD/GET/PUT/DELETE object), and the dev/test loop needs a
local object store that emulates bucket semantics (reject corrupt uploads, immutable range reads).

## Decision
1. `cairn-server::storage` defines `ObjectStore`: `put/get/head/delete/presign_put/presign_get`.
2. Implementations: `LocalFsStore` (dev/test; HTTP-served presign semantics with HMAC signatures,
   strict checksum enforcement, Range support, immutable cache headers) and an S3-compatible
   signer (`SigV4Presigner`, pure Rust hmac/sha2) that produces standard SigV4 presigned URLs
   valid against any S3-compatible endpoint (AWS S3, Cloudflare R2; B2 via its S3-compatible
   API). `aws-sdk-s3` is deliberately NOT a core dependency; a thin adapter crate may add it
   later behind a feature flag without touching call sites (trait boundary exists for exactly
   this).
3. Checksum enforcement: presigned PUT binds `x-amz-checksum-sha256`; the bucket (or LocalFs
   emulation) rejects mismatches — "bucket rejects corrupt uploads" holds on every backend.
4. Signed GET URLs: SigV4 presigned GET with 1h TTL for S3-shaped backends; HMAC bearer tokens
   for the local backend; both immutable + Range-capable; query strings stripped in logs.

## Rationale
Presigning is a stable, documented wire format (SigV4); rclone's transfer/backoff/presign
patterns are the studied reference (THIRD_PARTY.md). Dropping the SDK cuts build time and
attack surface while remaining wire-compatible with production buckets. Deviation from §17's
crate list is recorded here per the working agreement.

## Consequences
- We own SigV4 correctness. Mitigation: signer is table-tested for canonical-request shape and
  integration-tested against LocalFsStore (which validates the same signed contract).
- R2/B2 compatibility is at the SigV4 level; provider quirks (e.g., B2 key constraints) belong in
  the adapter, not the core.
