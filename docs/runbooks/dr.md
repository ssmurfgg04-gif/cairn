# DR runbook — bucket loss & metadata PITR restore

Trigger: bucket region loss / accidental bucket wipe / metadata corruption.

## Recovery procedure (RTO target < 4h, RPO = last metadata backup)

1. **Freeze writes.** Flip kill switches server-side: `packing_enabled=false`,
   `tiering_enabled=false`. GC never runs in shadow-sweep mode without the beta gate, but
   confirm `gc` workers are not scheduled (`jobs_leader` rows expire within their TTL).
2. **Metadata restore (PITR).** The metadata plane (SQLite-compatible) is backed up
   continuously (WAL shipping) and PITR-restored to the last consistent point:
   `sqlite3 meta.db ".recover" | sqlite3 restored.db` (dev) or the libsql/D1 point-in-time
   restore (prod). Verify: `refs` table non-empty per project; `journal` head seq matches
   `projects.fold_seq` (or is newer).
3. **Bucket rehydrate.** Chunks are content-addressed and verified on every ingest. Restore
   the bucket from provider snapshots (R2/B2). Any chunk that remains missing is detected
   deterministically: `BatchExists` reports it missing on the next client sync → clients
   re-upload from their local CAS (devices hold recently-used chunks; NLE scratch caches are
   explicitly NOT relied on — see SPEC §10 pinning note).
4. **Verify.** Run the headless canary (upload→verify→recall). Run `cairn gc-shadow-report`
   per tenant — MUST be clean. Run `cairn doctor` on a test device.
5. **Unfreeze.** Re-enable packing/tiering. Watch `sync_propagation_p95` and
   `journal_append_p99` for 30 min.

## What is NOT recoverable
- Client-local-only dirty files (not yet appended to the journal) — by definition never
  acknowledged (I2 protects only acknowledged writes).
- Cross-tenant deduplication advantages (never existed; I3).
