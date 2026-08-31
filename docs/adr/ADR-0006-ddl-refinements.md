# ADR-0006: DDL refinements — trash.manifest_hash, legal_holds, config_flags, jobs, jobs_leader, tenants.deep_archive

Date: 2026-08-31 · Status: Accepted

## Context
Spec §12 GC roots include trash tombstones and ∞ legal holds; §16 requires kill switches and a
canary; jobs need progress + leader election. The §5.2 DDL as written cannot express: (a) which
content a trash row protects (GC needs bytes-level reachability), (b) legal holds, (c) runtime
kill switches, (d) job progress/ETA, (e) leader lease for workers, (f) per-tenant Deep Archive
opt-in.

## Decision (all dialect-portable SQL, no stored procedures)
```sql
ALTER trash ADD manifest_hash TEXT NOT NULL;  -- reachability root while purged_after is in future
CREATE TABLE legal_holds(tenant_id TEXT, project_id TEXT, path TEXT, created_at INTEGER,
                         PRIMARY KEY(tenant_id, project_id, path));
CREATE TABLE config_flags(name TEXT PRIMARY KEY, value TEXT, updated_at INTEGER);
  -- keys: packing_enabled, tiering_enabled, delta_fold_enabled, compression_enabled,
  --       placeholder_driver ('native'|'winfsp')
CREATE TABLE jobs(id TEXT PRIMARY KEY, tenant_id TEXT, kind TEXT, state TEXT, progress REAL,
                  total REAL, detail TEXT, updated_at INTEGER);
CREATE TABLE jobs_leader(name TEXT PRIMARY KEY, holder TEXT, expires_at INTEGER);
ALTER tenants ADD deep_archive INTEGER NOT NULL DEFAULT 0;  -- per-tenant opt-in hook
```

## Consequences
- GC mark phase walks: refs → commit/tree/manifest objects; trash.manifest_hash → manifest
  objects (until purge); upload_sessions <7d → their chunk hashes; legal_holds → path manifests
  at deleted_seq.
- Kill switches are read per job run (not cached at boot): "flags flip without restart harm" is
  then true by construction and is tested.
- `jobs` rows power ctl RecallService progress + ETA and the dashboard.
