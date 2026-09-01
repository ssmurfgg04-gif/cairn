# Research scan — CDC optimization frontier & OTIO merge substrate (2026-09-02)

Scope: ground cairn's chunker review-round work and ADR-0014 Phase 4 against the
published literature and existing open-source tooling. No code changed by this note.

## 1. Content-defined chunking: where the three-zone rewrite sits

Papers/implementations surveyed:
- **FastCDC** (Xia et al., USENIX ATC '16) — the algorithm cairn ports: Gear hash,
  normalized chunking, mask-based cut condition. Byte-at-a-time in reference
  implementations (~200-400 MB/s class).
- **QuickCDC** (Sullivan et al., IEEE Cloud '19) — trades some dedup ratio for
  chunker speed by reducing the bytes hashed per decision; different cut
  distribution (protocol-visible).
- **"The Chonkers Algorithm"** (arXiv, 2024) + the `chonkers` Rust crate —
  SIMD/word-at-a-time Gear evaluation; explicitly notes that changing the hash
  evaluation changes cut points (their docs call the chunk layout "forward only").
- **Vectorized Sequence-Based Chunking** (ResearchGate/2024) — SIMD across the
  hash update; again a different (or version-bumped) chunk function.

**Cairn's position (this round):** the three-zone `push` is deliberately in the
class of optimizations that PRESERVE cut points bit-for-bit — Zone A exploits the
fact that a Gear contribution `T[b] << d` vanishes from the 64-bit state once
d >= 64, so bytes >64 positions below the first possible decision carry no
information. This is not one of the published SIMD rewrites (those change the
evaluable window and thus the layout); it is the no-protocol-break point on the
speed curve. The golden corpus reuse ratios (0.856/0.879) are unchanged, which is
the operational proof. If a future milestone needs 2+ GB/s on one core, the
Chonkers-style word-at-a-time route is available behind a `CHUNKER_VERSION=2`
bump (SPEC §5.1 already contemplates it) with a dual-read window for old objects.

## 2. Phase 4 substrate: OpenTimelineIO (ADR-0014)

- **OpenTimelineIO** (AcademySoftwareFoundation, ASWF) — maintained, schema-
  versioned, C++/Python with a defined edit model (Clip/Gap/Stack/Track) and
  JSON serialization designed for interchange. This is the right base/ours/theirs
  surface: merges happen on schema, not on vendor XML.
- FCPXML (Apple) is the secondary target but is macOS-tooling-shaped; OTIO is the
  neutral substrate.
- Practical guidance adopted into ADR-0014: merges land as NEW journal entries
  (never in-place), schema version pinned per merge, deterministic tie-break after
  token fencing. Existing generic 3-way merge tooling (git mergetools et al.) is
  explicitly the WRONG tool for timeline semantics — the merge must understand
  track/gap/edit structure, hence Phase 4 is a real project, not a config flag.

## 3. Verification links

- Kani (BMC) continues to gate the pure-function invariants (b64/hex/path/bloom/
  commit/sniff/policy shards; all green on the review-round commits).
- The chunker's bit-identity is pinned by differential tests against the original
  byte loop (the right oracle for a cut-point-preserving rewrite), plus the corpus
  reuse manifest as the statistical backstop.
