-- Cairn server DDL (SPEC §5.2 + ADR-0006). Portable SQL: runs on SQLite (dev) and libsql/D1
-- (prod). No stored procedures. Every row and key is tenant-scoped (I3).
CREATE TABLE IF NOT EXISTS tenants(
  id TEXT PRIMARY KEY,
  region TEXT NOT NULL DEFAULT '',
  deep_archive INTEGER NOT NULL DEFAULT 0,
  created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS users(
  id TEXT PRIMARY KEY,
  tenant_id TEXT NOT NULL,
  email TEXT NOT NULL,
  role TEXT NOT NULL DEFAULT 'member',
  created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS users_tenant ON users(tenant_id);
CREATE TABLE IF NOT EXISTS devices(
  id TEXT PRIMARY KEY,
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL DEFAULT '',
  token_hash TEXT NOT NULL,
  scopes TEXT NOT NULL DEFAULT 'sync',
  revoked INTEGER NOT NULL DEFAULT 0,
  last_seen INTEGER NOT NULL DEFAULT 0,
  created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS devices_tenant ON devices(tenant_id);
CREATE TABLE IF NOT EXISTS projects(
  tenant_id TEXT NOT NULL,
  project_id TEXT NOT NULL,
  name TEXT NOT NULL DEFAULT '',
  next_lease_token INTEGER NOT NULL DEFAULT 0,
  fold_seq INTEGER NOT NULL DEFAULT 0,
  created_at INTEGER NOT NULL,
  PRIMARY KEY(tenant_id, project_id)
);
CREATE TABLE IF NOT EXISTS journal(
  tenant_id TEXT NOT NULL,
  project_id TEXT NOT NULL,
  seq INTEGER NOT NULL,
  request_id TEXT NOT NULL,
  device_id TEXT NOT NULL,
  path TEXT NOT NULL,
  op BLOB NOT NULL,
  server_ts INTEGER NOT NULL,
  PRIMARY KEY(tenant_id, project_id, seq)
);
CREATE UNIQUE INDEX IF NOT EXISTS journal_request_id
  ON journal(tenant_id, project_id, request_id);
CREATE INDEX IF NOT EXISTS journal_path_seq
  ON journal(tenant_id, project_id, path, seq);
CREATE TABLE IF NOT EXISTS journal_cursors(
  device_id TEXT NOT NULL,
  project_id TEXT NOT NULL,
  last_seq INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY(device_id, project_id)
);
CREATE TABLE IF NOT EXISTS refs(
  tenant_id TEXT NOT NULL,
  project_id TEXT NOT NULL,
  ref_name TEXT NOT NULL,
  commit_hash TEXT NOT NULL,
  version INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY(tenant_id, project_id, ref_name)
);
CREATE TABLE IF NOT EXISTS chunks(
  tenant_id TEXT NOT NULL,
  hash TEXT NOT NULL,
  size INTEGER NOT NULL,
  tier TEXT NOT NULL DEFAULT 'hot',
  state TEXT NOT NULL DEFAULT 'present',
  last_touched INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY(tenant_id, hash)
);
CREATE INDEX IF NOT EXISTS chunks_tier ON chunks(tenant_id, tier, last_touched);
CREATE TABLE IF NOT EXISTS manifests(
  tenant_id TEXT NOT NULL,
  hash TEXT NOT NULL,
  size INTEGER NOT NULL,
  entry_count INTEGER NOT NULL,
  PRIMARY KEY(tenant_id, hash)
);
CREATE TABLE IF NOT EXISTS upload_sessions(
  id TEXT PRIMARY KEY,
  tenant_id TEXT NOT NULL,
  device_id TEXT NOT NULL,
  chunk_hashes BLOB NOT NULL,
  expires_at INTEGER NOT NULL,
  state TEXT NOT NULL DEFAULT 'open'
);
CREATE TABLE IF NOT EXISTS leases(
  tenant_id TEXT NOT NULL,
  project_id TEXT NOT NULL,
  path TEXT NOT NULL,
  device_id TEXT NOT NULL,
  token INTEGER NOT NULL,
  expires_at INTEGER NOT NULL,
  PRIMARY KEY(tenant_id, project_id, path)
);
CREATE TABLE IF NOT EXISTS packs(
  tenant_id TEXT NOT NULL,
  pack_key TEXT NOT NULL,
  size INTEGER NOT NULL,
  state TEXT NOT NULL DEFAULT 'building',
  created_at INTEGER NOT NULL,
  PRIMARY KEY(tenant_id, pack_key)
);
CREATE TABLE IF NOT EXISTS pack_index(
  tenant_id TEXT NOT NULL,
  object_hash TEXT NOT NULL,
  pack_key TEXT NOT NULL,
  offset INTEGER NOT NULL,
  len INTEGER NOT NULL,
  PRIMARY KEY(tenant_id, object_hash)
);
CREATE TABLE IF NOT EXISTS trash(
  tenant_id TEXT NOT NULL,
  project_id TEXT NOT NULL,
  path TEXT NOT NULL,
  deleted_seq INTEGER NOT NULL,
  purge_after INTEGER NOT NULL,
  manifest_hash TEXT NOT NULL DEFAULT '',
  PRIMARY KEY(tenant_id, project_id, path)
);
CREATE TABLE IF NOT EXISTS legal_holds(
  tenant_id TEXT NOT NULL,
  project_id TEXT NOT NULL,
  path TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  PRIMARY KEY(tenant_id, project_id, path)
);
CREATE TABLE IF NOT EXISTS config_flags(
  name TEXT PRIMARY KEY,
  value TEXT NOT NULL,
  updated_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS jobs(
  id TEXT PRIMARY KEY,
  tenant_id TEXT NOT NULL DEFAULT '',
  kind TEXT NOT NULL,
  state TEXT NOT NULL DEFAULT 'running',
  progress REAL NOT NULL DEFAULT 0,
  total REAL NOT NULL DEFAULT 0,
  detail TEXT NOT NULL DEFAULT '',
  updated_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS jobs_leader(
  name TEXT PRIMARY KEY,
  holder TEXT NOT NULL,
  expires_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS metering(
  tenant_id TEXT NOT NULL,
  day TEXT NOT NULL,
  bytes_stored INTEGER NOT NULL DEFAULT 0,
  bytes_uploaded INTEGER NOT NULL DEFAULT 0,
  bytes_downloaded INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY(tenant_id, day)
);
CREATE TABLE IF NOT EXISTS audit_log(
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  tenant_id TEXT NOT NULL DEFAULT '',
  actor TEXT NOT NULL,
  action TEXT NOT NULL,
  resource TEXT NOT NULL DEFAULT '',
  ts INTEGER NOT NULL,
  detail TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS audit_ts ON audit_log(tenant_id, ts);
