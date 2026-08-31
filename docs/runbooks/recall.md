# Recall from cold runbook

Trigger: user/ctl requests content that was tiered to the cold backend (chunks untouched
>90d, per-tenant Deep Archive opt-in for deeper classes).

## Semantics (SPEC §12)
- Tiering NEVER touches manifests/trees/commits — only chunks (hot → archive).
- Tiering verifies the cold copy checksum BEFORE tombstoning the hot object. A verify
  failure keeps the hot copy (I2: never lose data).
- Recall copies archive → hot, verifying BLAKE3 per chunk before flipping the row back to
  `hot`, with progress + ETA in the `jobs` table (ctl RecallService polls it; dashboard
  renders it).

## Procedure
1. `cairn recall --project p1 [--path shot.mov]` (or ctl StartRecall).
2. Poll: ctl RecallStatus / dashboard recall panel (progress bar + ETA).
3. On failure `CHECKSUM_MISMATCH` during recall: the cold object is corrupt — re-tier from
   any healthy device (BatchExists will report the chunk missing server-side only if the
   hot copy is absent; devices holding the chunk locally re-upload transparently).

## Deep Archive caveats
- Provider restore latency (hours) applies BEFORE the copy step; `jobs.detail` reports the
  waiting state. Users are surfaced an ETA, not a lie.
- Never recall during a GC window: the epoch guard fails the recall and it re-runs (safe).
