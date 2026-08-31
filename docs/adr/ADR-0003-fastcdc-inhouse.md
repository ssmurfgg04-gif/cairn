# ADR-0003: FastCDC implemented in-house after restic study (Gear table, mask 2^22)

Date: 2026-08-31 · Status: Accepted

## Context
Spec §17: "fastcdc (or port restic's chunker — study it first either way)". The chunker is the
dedup foundation; boundary behavior (min 1MB, avg 4MB via mask 2^22, max 16MB) and streaming
single-pass semantics are contractual.

## Decision
`cairn-core` ships a compact, dependency-free FastCDC/Gear implementation ported from the
studied approaches (FastCDC 2016 paper; restic's BSD-2 chunker for Gear-table generation and
rolling-boundary discipline; fastcdc-rs for API ergonomics). Fixed 256-entry Gear table derived
from a documented splitmix64 stream; boundary condition `gear & (2^22 - 1) == 0` within
`[min=1MB, max=16MB]`; streaming push API with single-pass consumption compatible with
simultaneous whole-stream BLAKE3.

## Rationale
- The `fastcdc` crate's exact cut semantics and version churn would put a contractual property
  (reuse ratios) behind an external version matrix we cannot pin forever.
- Our implementation is ~150 lines, exhaustively property-tested (stability under insertion,
  size distribution, reuse >70% across synthetic save sequences), and keeps `cairn-core` pure
  and audit-friendly.
- restic's approach is the studied reference; provenance recorded in THIRD_PARTY.md.

## Consequences
- We own the chunker's correctness. The property suite in §15.2 is therefore a hard CI gate.
- The Gear table is versioned: `CHUNKER_VERSION = 1`; changing it is a breaking protocol change
  (new chunk identities) and requires an ADR.
