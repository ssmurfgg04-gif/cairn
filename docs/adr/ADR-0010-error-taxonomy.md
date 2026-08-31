# ADR-0010: Error taxonomy — single retry-class table, codes carried in proto ErrorDetail

Date: 2026-08-31 · Status: Accepted

## Context
Spec §14 requires one table in code + docs, and "every error carries code + retryability hint in
proto"; "never 500-as-catchall".

## Decision
1. `cairn-core::error::RetryClass`: `Auto` (full jitter, max 5, idempotent ops only), `Never`
   (fatal-client; surface via doctor), `Conflict` (explicit resolution: CONFLICT, STALE_LEASE,
   REF_CAS), `Server` (respond precisely).
2. Every failure crosses the wire as `cairn.v4.ErrorDetail { code, retry_class, message }`
   serialized into tonic Status details (JSON), never a bare 500/unknown.
3. Server error codes (stable strings, documented in ctl-api.md):
   `CONFLICT, STALE_LEASE, REF_CAS, UNAUTHENTICATED, PERMISSION_DENIED, NOT_FOUND,
   SESSION_EXPIRED, CHECKSUM_MISMATCH, BATCH_TOO_LARGE, RATE_LIMITED, INTERNAL, UNAVAILABLE,
   COMPACTION_REQUIRED, SESSION_FULL`.
4. Client mapping lives in one place (`cairn-sync::retry`): Auto → full-jitter backoff retry
   (max 5); Never → stop + `doctor` finding; Conflict → state-machine resolution paths;
   Server → structured report, no blind retry.

## Consequences
- One source of truth: the Rust table + this ADR + ctl-api.md stay in lockstep (CI test asserts
  proto codes ⊆ documented codes).
- Full-jitter, max-5, idempotent-only retry is enforced by a single call-site helper.
