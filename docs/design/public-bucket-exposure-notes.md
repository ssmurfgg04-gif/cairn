# Learning notes: public-bucket search engines (GrayHatWarfare) vs Cairn's bucket posture

Date: 2026-09-01 · Context: WO6-9 security round, S3 learning exercise · Read with:
SPEC §9 (data plane), §13 (security), ADR-0005 (SigV4), docs/design/nle-test-matrix.md

## What GrayHatWarfare is and how indexing actually works

GrayHatWarfare indexes **publicly listable** S3-compatible buckets (AWS S3, GCS, Azure,
MinIO, Wasabi…). The mechanics matter for the threat model:

1. **Discovery**: bucket names are harvested from certificate-transparency logs, DNS
   patterns, URL dumps, and crawler hits — not brute force. A bucket referenced in any
   public artifact eventually enters the corpus.
2. **Listing**: for each candidate endpoint it attempts anonymous `ListObjectsV2`
   (`s3:ListBucket` granted to `*`). A bucket that ALLOWS anonymous LIST but denies
   anonymous GET still leaks every object key — the listing itself is the breach.
3. **Indexing**: object keys are indexed for keyword/extension/size search; objects
   with anonymous GET become directly downloadable and are content-indexed.
4. **Consequence**: "we only misconfigured ListBucket" is not a safe failure. Key
   structure IS sensitive data, and key-read misconfigurations compound.

## What this means for Cairn's bucket (concrete posture)

- **Object key surface is content-addressed, deliberately boring.** Chunk keys are
  `t{tenant}/c/{hash[0:2]}/{hash}`; manifests/commits/trees/dicts (ADR-0013) are
  hash-named binary objects. There are NO human filenames to keyword-search, no
  extensions to filter, no folder structure that reads like a user's directory tree.
  Even a full LIST leaks: tenant IDs, chunk-hash prefixes (reveal file sizes via chunk
  boundaries indirectly), and object counts — an inventory oracle, not a content leak.
- **Content exposure is the real risk.** Chunks store media slices (policy `none` for
  braw/prores/mov — SPEC §6). With anonymous GET, GrayHatWarfare-style content indexing
  would surface raw media fragments. Encryption tier T3 (AES-SIV, §13) makes chunk
  bytes opaque, but the DEFAULT tier does not encrypt-at-rest per-chunk — the bucket
  ACL is the only wall. Conclusion: **the bucket must be private by policy, not by
  obscurity of its name**, and the server's presigned-GET TTL (1 h, immutable,
  Range-capable) is the only sanctioned read path.
- **Presign posture**: presigned URLs are per-object, time-boxed, and only issued to
  authenticated devices through the metadata plane; the bucket itself never grants
  anonymous GET. Leaked presign URLs expire (1 h) and do not enable LIST.
- **Regression detection belongs in the canary** (follow-up, post-beta): extend the
  control-plane canary to attempt an anonymous `ListObjectsV2` against the configured
  bucket every cycle and ALERT if it succeeds. That converts "bucket policy drifted"
  from a silent, undiscoverable failure into a paging alert. Honest note: this needs
  the real-bucket soak (`just soak-5gb`, env-gated on studio credentials) to be
  testable end to end — it is designed here, not claimed as working.

## Practical rules this exercise fixes for the beta runbook

1. Bootstrap check in `cairn doctor` / deployment runbook: bucket endpoint must DENY
   anonymous `s3:ListBucket` and `s3:GetObject` (documented operator checklist item).
2. No bucket name ever appears in client-visible artifacts (server URLs are the
   metadata-plane address; presigns embed bucket/host — fine — but docs/logs must not
   paste presigned URLs; the log-leak sweep in `scripts/security-sweep.sh` guards the
   code side).
3. `docs/runbooks/dr.md` gets the "bucket went public" incident row: revoke public
   ACL → rotate presign capability (no server-side secret change needed — presigns are
   derived from the signing key which never left the server) → audit listing exposure
   window via bucket access logs.

## Sources

- buckets.grayhatwarfare.com (product surface: keyword/ext/size search over public
  buckets) and the GrayHatWarfare medium write-up "How to search for Open Amazon S3
  buckets"
- NetApp blog: "Amazon S3 Bucket Security: find & secure open buckets"
- GitHub `awesome-sec-s3` (curated S3 security tooling list)
