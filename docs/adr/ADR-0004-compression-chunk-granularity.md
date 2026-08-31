# ADR-0004: Compression at chunk granularity; file_hash frozen over raw chunk hashes

Date: 2026-08-31 · Status: Accepted

## Context
Spec §6 defines a single-pass pipeline (whole-stream BLAKE3 + FastCDC + per-chunk BLAKE3) with a
compression table: media uncompressed, text zstd-3, NLE project files zstd with per-project
dictionary. §5.1 freezes `file_hash = BLAKE3(concat of chunk hashes in file order)`. If the
chunker ran on compressed bytes, chunk identities would depend on compressor/dictionary versions,
breaking dedup across auto-saves (compressed streams diverge early) and violating the >70% reuse
property.

## Decision
1. Chunking always runs on RAW file bytes. Chunk hashes and `file_hash` are a pure function of
   file content — frozen, never changes silently.
2. Compression is applied per chunk AFTER cutting, with a per-file policy flag recorded in the
   manifest (`none | zstd3 | zstd_dict`), plus optional dictionary hash for NLE project files.
3. Stored object bytes may be compressed; the chunks table stays keyed by raw-content hash;
   BatchExists/dedup semantics unchanged; download path decompresses after chunk reassembly.
4. Media (`braw/prores/mxf/r3d/wav/mp4/mov`) is stored verbatim (flag `none`).

## Consequences
- Reuse property holds by construction; zstd/dictionary upgrades never invalidate identities.
- Slight compression-ratio loss versus whole-stream zstd — acceptable trade for stable dedup.
- Manifest format records: compression flag, dictionary hash, raw chunk hashes. Documented in
  SPEC §6 and unit-tested (round-trip + reuse properties).
