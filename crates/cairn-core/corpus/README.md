# Golden corpus (SPEC §15.3, runbook-beta)

This directory holds **golden corpus ingest** material for the chunk-reuse gate
(>70% per consecutive-save pair, asserted by
`crates/cairn-core/tests/properties.rs::golden_corpus_harness`).

## Layout

- `seed-corpus-NNN/NN.dat` — synthetic autosave sequences (see below). **Git-ignored**
  (large binaries); regenerate deterministically, never hand-edit.
- `manifest.json` — committed. BLAKE3 file-hash of every generated save + the
  per-sequence minimum consecutive-reuse observed at generation time. CI (and any
  operator) can regenerate the bytes from the seed and verify this manifest.

## Seed corpus

`cairn-x corpus-gen` writes 2 sequences × 8 saves × 128MB of deterministic
synthetic "NLE autosave" files: structured project header (stable per sequence),
a large seeded media index with a small re-rendered window per save (~0.05%),
and an append-only render log — the statistical shape of real .prproj/.drp
autosave chains. Observed min consecutive-save reuse at generation: **0.856 /
0.879** (gate: >0.70).

## Commands

```sh
just corpus-gen      # regenerate bytes + manifest (deterministic, seed 20260901)
just corpus-verify   # run the >70% reuse gate over whatever corpus is present
```

## Real studio ingest (beta)

Real NLE save sequences are LFS-gated — they carry studio IP. Per studio,
collect 10+ autosave sequences (.prproj/.drp) + BRAW/ProRes/MXF/WAV samples into
`corpus/<studio>-<seq>/NN.ext` (save order), then `git lfs add corpus/**`. The
harness gates each sequence the same way. See docs/runbook-beta.md §Golden corpus.
