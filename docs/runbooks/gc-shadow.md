# GC shadow report runbook

Trigger: weekly beta gate check; before ANY non-shadow sweep; after DR.

The GC is reachability-based (NOT refcounts). Roots: refs → commit → tree → manifest →
chunks; trash tombstones (manifest_hash column, ADR-0006); upload sessions <7d; legal holds
(∞). Unreachable chunks enter `deleting` with a 14-day grace before sweep.

## Beta gate (M6/M8 AC)
1. Generate churn: the `gc_churn` integration test runs 10k synthetic ops; for production,
   enable GC shadow for a full beta month.
2. Weekly: `cairn gc-shadow-report --tenant t1`.
3. Gate: `violations MUST be 0` and `clean=true`. A violation means a reachable object was
   flagged — STOP all sweeps, file a bug (I2 hazard), attach the report JSON.

## Interpretation
- `would_delete_count` counts unreachable chunks entering grace (normal under churn).
- `scanned_objects` should track the chunks table cardinality per tenant.
- Shadow runs never delete and never set `state='deleting'`.

## Sweep enablement (post-beta)
1. Shadow clean for the full gate window.
2. Flip `packing_enabled` and schedule `gc_pass(shadow=false)` off-peak.
3. Monitor `gc_shadow_violations` metric — alert if > 0, ever.
