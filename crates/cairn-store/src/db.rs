//! Client SQLite store — SPEC §5.3.
//!
//! - WAL mode + `busy_timeout=5000`
//! - migrations via `PRAGMA user_version`
//! - single writer: one serialized connection behind a mutex (daemon has one writer task)

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use cairn_core::clock::SystemClock;
use cairn_core::{CairnError, ErrorKind};
use rusqlite::Connection;

/// Local file row (mirrors `files` table).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRow {
    /// Project-relative NFC path.
    pub path: String,
    /// Project id.
    pub project_id: String,
    /// Manifest hash hex (content identity), if known.
    pub manifest_hash: Option<String>,
    /// Size in bytes.
    pub size: u64,
    /// 'file' | 'dir' | 'symlink'
    pub mode: String,
    /// Last observed mtime (informational only — I4: never trusted for ordering).
    pub mtime: i64,
    /// Local sync state.
    pub local_state: String,
}

/// The client store. Cloning shares the underlying connection (single writer).
#[derive(Clone)]
pub struct Store {
    conn: std::sync::Arc<Mutex<Connection>>,
    root: PathBuf,
    clock: std::sync::Arc<dyn SystemClock>,
}

/// Current client schema version (`PRAGMA user_version`).
pub const CLIENT_SCHEMA_VERSION: i64 = 5;

/// True when `root` lives on a network filesystem where SQLite WAL is
/// unsafe: `-shm` coordination assumes POSIX mmap + local locking, which
/// SMB/NFS violate (SQLITE_PROTOCOL, torn WAL, silent corruption).
/// Detection is prefix-based and Windows-focused (UNC `\\server\share` —
/// the primary deployment target): mapped drives and unix mounts are NOT
/// detected, so this is a tripwire, not a guarantee. Returns false for
/// relative paths (resolved by the caller before open in practice).
#[must_use]
pub fn root_on_network_mount(root: &Path) -> bool {
    let s = root.to_string_lossy();
    s.starts_with(r"\\") || s.starts_with("//")
}

impl Store {
    /// Open (or create) the store at `root` (a directory). Applies migrations.
    pub fn open(root: &Path, clock: std::sync::Arc<dyn SystemClock>) -> Result<Self, CairnError> {
        if root_on_network_mount(root) {
            // Loud, not fatal: refusing to open would strand existing
            // setups; silent corruption strands their data. Doctor gates it.
            tracing::warn!(
                root = %root.display(),
                "store lives on a network share: SQLite WAL is unsafe there (locking/mmap); move the home to a local disk"
            );
        }
        std::fs::create_dir_all(root)
            .map_err(|e| CairnError::new(cairn_core::ErrorKind::Io, format!("mkdir root: {e}")))?;
        let db_path = root.join("db.sqlite");
        let conn = Connection::open(&db_path)
            .map_err(|e| CairnError::new(cairn_core::ErrorKind::Io, format!("open db: {e}")))?;
        conn.busy_timeout(std::time::Duration::from_millis(5000))
            .map_err(|e| {
                CairnError::new(cairn_core::ErrorKind::Io, format!("busy_timeout: {e}"))
            })?;
        // WAL discipline (SQLite reference, THIRD_PARTY.md)
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| CairnError::new(cairn_core::ErrorKind::Io, format!("wal: {e}")))?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(|e| CairnError::new(cairn_core::ErrorKind::Io, format!("synchronous: {e}")))?;
        // Reader-pool round pragmas (ADR-0026, sqlite-kit convention): 32 MiB
        // page cache (the header/serve path reads multi-MB blobs; the default
        // ~2 MiB cache thrashes on every serve) + temp tables in memory.
        conn.pragma_update(None, "cache_size", "-32000")
            .map_err(|e| CairnError::new(cairn_core::ErrorKind::Io, format!("cache_size: {e}")))?;
        conn.pragma_update(None, "temp_store", "MEMORY")
            .map_err(|e| CairnError::new(cairn_core::ErrorKind::Io, format!("temp_store: {e}")))?;
        // Checkpoint cadence: without an explicit autocheckpoint, a long-lived
        // reader (dashboard/scan holding a snapshot) lets the WAL grow without
        // bound during write bursts until the partition fills (SQLITE_FULL).
        // 1000 pages ≈ 4 MiB — the production baseline; passive checkpoints
        // never block writers.
        conn.pragma_update(None, "wal_autocheckpoint", 1000)
            .map_err(|e| {
                CairnError::new(
                    cairn_core::ErrorKind::Io,
                    format!("wal_autocheckpoint: {e}"),
                )
            })?;
        let store = Store {
            conn: std::sync::Arc::new(Mutex::new(conn)),
            root: root.to_path_buf(),
            clock,
        };
        store.migrate()?;
        Ok(store)
    }

    /// Filesystem root (contains db.sqlite, blobs/).
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Shared connection handle (single writer) — CAS/outbox/headers share it.
    #[must_use]
    pub fn conn_handle(&self) -> std::sync::Arc<Mutex<Connection>> {
        std::sync::Arc::clone(&self.conn)
    }

    /// Current `PRAGMA user_version`.
    pub fn schema_version(&self) -> Result<i64, CairnError> {
        let conn = self.conn.lock().expect("store poisoned");
        conn.query_row("PRAGMA user_version", [], |r| r.get(0))
            .map_err(|e| CairnError::new(cairn_core::ErrorKind::Io, format!("user_version: {e}")))
    }

    fn migrate(&self) -> Result<(), CairnError> {
        let conn = self.conn.lock().expect("store poisoned");
        let v: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .map_err(|e| {
                CairnError::new(cairn_core::ErrorKind::Io, format!("user_version: {e}"))
            })?;
        if v < 1 {
            conn.execute_batch(
                r"
                BEGIN;
                CREATE TABLE IF NOT EXISTS files(
                  path TEXT NOT NULL,
                  project_id TEXT NOT NULL,
                  manifest_hash TEXT,
                  size INTEGER NOT NULL DEFAULT 0,
                  mode TEXT NOT NULL DEFAULT 'file',
                  mtime INTEGER NOT NULL DEFAULT 0,
                  local_state TEXT NOT NULL DEFAULT 'synced',
                  PRIMARY KEY(project_id, path)
                );
                CREATE TABLE IF NOT EXISTS outbox(
                  request_id TEXT PRIMARY KEY,
                  project_id TEXT NOT NULL,
                  op BLOB NOT NULL,
                  state TEXT NOT NULL DEFAULT 'pending',
                  attempts INTEGER NOT NULL DEFAULT 0,
                  created_at INTEGER NOT NULL
                );
                CREATE INDEX IF NOT EXISTS outbox_state ON outbox(state, created_at);
                CREATE TABLE IF NOT EXISTS blobs(
                  hash TEXT PRIMARY KEY,
                  size INTEGER NOT NULL,
                  atime INTEGER NOT NULL,
                  pinned INTEGER NOT NULL DEFAULT 0
                );
                CREATE TABLE IF NOT EXISTS dir_headers(
                  pointer_hash TEXT PRIMARY KEY,
                  head BLOB,
                  tail BLOB
                );
                CREATE TABLE IF NOT EXISTS devices(
                  device_id TEXT NOT NULL,
                  project_id TEXT NOT NULL,
                  last_seq INTEGER NOT NULL DEFAULT 0,
                  PRIMARY KEY(device_id, project_id)
                );
                CREATE TABLE IF NOT EXISTS leases_local(
                  path TEXT PRIMARY KEY,
                  token INTEGER NOT NULL,
                  expires_at INTEGER NOT NULL
                );
                CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
                COMMIT;
                ",
            )
            .map_err(|e| CairnError::new(cairn_core::ErrorKind::Io, format!("migrate v1: {e}")))?;
            conn.pragma_update(None, "user_version", 1)
                .map_err(|e| CairnError::new(cairn_core::ErrorKind::Io, format!("set v1: {e}")))?;
        }
        if v < 2 {
            // WO6-2: file-level pins (ctl Pin/Unpin/ListPins). Chunk-level pinned
            // bits live on blobs.pinned (set via Cas::pin); this table is the
            // file-level intent + ListPins surface. Pinned files' chunks are
            // excluded from LRU eviction by construction.
            conn.execute_batch(
                r"
                BEGIN;
                CREATE TABLE IF NOT EXISTS pins(
                  project_id TEXT NOT NULL,
                  path TEXT NOT NULL,
                  pinned_at INTEGER NOT NULL,
                  PRIMARY KEY(project_id, path)
                );
                COMMIT;
                ",
            )
            .map_err(|e| CairnError::new(cairn_core::ErrorKind::Io, format!("migrate v2: {e}")))?;
            conn.pragma_update(None, "user_version", 2)
                .map_err(|e| CairnError::new(cairn_core::ErrorKind::Io, format!("set v2: {e}")))?;
        }
        if v < 3 {
            // ADR-0014 Phase 3: process-bound ephemeral leases. `pid` records the
            // OWNING process on this device — the daemon heartbeat renews while it
            // lives and rows whose process died are reaped, so a crashed editor
            // releases its pen in seconds (no human unblocking). project_id/device_id
            // give the heartbeat the context to renew and server-release correctly.
            // NULLs = legacy rows: expire via TTL exactly as before.
            conn.execute_batch(
                r"
                ALTER TABLE leases_local ADD COLUMN pid INTEGER;
                ALTER TABLE leases_local ADD COLUMN project_id TEXT;
                ALTER TABLE leases_local ADD COLUMN device_id TEXT;
                ",
            )
            .map_err(|e| CairnError::new(cairn_core::ErrorKind::Io, format!("migrate v3: {e}")))?;
            conn.pragma_update(None, "user_version", 3)
                .map_err(|e| CairnError::new(cairn_core::ErrorKind::Io, format!("set v3: {e}")))?;
        }
        if v < 4 {
            // ADR-0031 Phase 1: project-scoped collaboration records (roster /
            // audit / review) carried as journal `state_record` ops. One row per
            // (project, family, record_id); the MERGE semantics (LWW vs
            // append-only union) live in cairn_sync::state_records — this table
            // is the durable materialized view the apply/publish paths share.
            // `device_id` is the journal authorship of the winning/last write
            // (LWW tie-break), `payload` the canonical JSON of the record.
            conn.execute_batch(
                r"
                CREATE TABLE IF NOT EXISTS state_records (
                  project_id TEXT NOT NULL,
                  family     TEXT NOT NULL,
                  record_id  TEXT NOT NULL,
                  key        TEXT NOT NULL,
                  ts_ms      INTEGER NOT NULL,
                  device_id  TEXT NOT NULL,
                  payload    BLOB NOT NULL,
                  tombstone  INTEGER NOT NULL DEFAULT 0,
                  PRIMARY KEY (project_id, family, record_id)
                );
                CREATE INDEX IF NOT EXISTS idx_state_records_family
                  ON state_records(project_id, family, ts_ms);
                ",
            )
            .map_err(|e| CairnError::new(cairn_core::ErrorKind::Io, format!("migrate v4: {e}")))?;
            conn.pragma_update(None, "user_version", 4)
                .map_err(|e| CairnError::new(cairn_core::ErrorKind::Io, format!("set v4: {e}")))?;
        }
        if v < 5 {
            // CONTRACT-DEBT #1 (conflict auto-offer): one PENDING semantic-merge
            // offer per (project, path), created by the engine's CONFLICT arm
            // when the flag is on and the three-way merge would land clean.
            // Passive until a human accepts (cairn_sync::merge_offer); the row
            // stores the manifests needed to RECOMPUTE the merge at accept time
            // (the merge is byte-deterministic, so bytes are never persisted).
            conn.execute_batch(
                r"
                CREATE TABLE IF NOT EXISTS merge_offers (
                  project_id     TEXT NOT NULL,
                  path           TEXT NOT NULL,
                  copy_path      TEXT NOT NULL,
                  base_manifest  TEXT NOT NULL,
                  theirs_manifest TEXT NOT NULL,
                  report_json    TEXT NOT NULL,
                  created_ms     INTEGER NOT NULL,
                  PRIMARY KEY (project_id, path)
                );
                ",
            )
            .map_err(|e| CairnError::new(cairn_core::ErrorKind::Io, format!("migrate v5: {e}")))?;
            conn.pragma_update(None, "user_version", 5)
                .map_err(|e| CairnError::new(cairn_core::ErrorKind::Io, format!("set v5: {e}")))?;
        }
        Ok(())
    }

    // ---- files ----

    /// Upsert a file row.
    pub fn put_file(&self, f: &FileRow) -> Result<(), CairnError> {
        let conn = self.conn.lock().expect("store poisoned");
        conn.execute(
            "INSERT INTO files(path, project_id, manifest_hash, size, mode, mtime, local_state)
             VALUES(?1,?2,?3,?4,?5,?6,?7)
             ON CONFLICT(project_id, path) DO UPDATE SET
               manifest_hash=excluded.manifest_hash, size=excluded.size, mode=excluded.mode,
               mtime=excluded.mtime, local_state=excluded.local_state",
            rusqlite::params![
                f.path,
                f.project_id,
                f.manifest_hash,
                f.size as i64,
                f.mode,
                f.mtime,
                f.local_state
            ],
        )
        .map_err(db_err)?;
        Ok(())
    }

    /// Remove a file row (FUSE unlink / local tombstone — cross-device delete
    /// propagation rides the engine journal). Stored chunks stay until GC.
    pub fn delete_file(&self, project_id: &str, path: &str) -> Result<(), CairnError> {
        let conn = self.conn.lock().expect("store poisoned");
        conn.execute(
            "DELETE FROM files WHERE project_id=?1 AND path=?2",
            rusqlite::params![project_id, path],
        )
        .map_err(db_err)?;
        Ok(())
    }

    /// Fetch a file row.
    #[must_use]
    pub fn get_file(&self, project_id: &str, path: &str) -> Option<FileRow> {
        let conn = self.conn.lock().expect("store poisoned");
        conn.query_row(
            "SELECT path, project_id, manifest_hash, size, mode, mtime, local_state
             FROM files WHERE project_id=?1 AND path=?2",
            rusqlite::params![project_id, path],
            |r| {
                Ok(FileRow {
                    path: r.get(0)?,
                    project_id: r.get(1)?,
                    manifest_hash: r.get(2)?,
                    size: r.get::<_, i64>(3)?.max(0) as u64,
                    mode: r.get(4)?,
                    mtime: r.get(5)?,
                    local_state: r.get(6)?,
                })
            },
        )
        .ok()
    }

    /// List file rows for a project.
    #[must_use]
    pub fn list_files(&self, project_id: &str) -> Vec<FileRow> {
        let conn = self.conn.lock().expect("store poisoned");
        Self::query_files(
            &conn,
            "SELECT path, project_id, manifest_hash, size, mode, mtime, local_state
             FROM files WHERE project_id=?1 ORDER BY path",
            vec![project_id.to_string()],
        )
    }

    /// Files in any of `states` (e.g. `&["dirty", "conflict"]`): predicate
    /// pushdown so hot passes don't decode the whole table to discard most
    /// of it. Same row shape and ordering as [`Store::list_files`]; empty
    /// `states` returns empty (never "all").
    pub fn list_files_in_states(&self, project_id: &str, states: &[&str]) -> Vec<FileRow> {
        if states.is_empty() {
            return Vec::new();
        }
        let placeholders = states.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT path, project_id, manifest_hash, size, mode, mtime, local_state
             FROM files WHERE project_id=?1 AND local_state IN ({placeholders}) ORDER BY path"
        );
        let mut params = vec![project_id.to_string()];
        params.extend(states.iter().map(|s| s.to_string()));
        let conn = self.conn.lock().expect("store poisoned");
        Self::query_files(&conn, &sql, params)
    }

    fn query_files(conn: &rusqlite::Connection, sql: &str, params: Vec<String>) -> Vec<FileRow> {
        let mut stmt = match conn.prepare(sql) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = stmt
            .query_map(rusqlite::params_from_iter(params), |r| {
                Ok(FileRow {
                    path: r.get(0)?,
                    project_id: r.get(1)?,
                    manifest_hash: r.get(2)?,
                    size: r.get::<_, i64>(3)?.max(0) as u64,
                    mode: r.get(4)?,
                    mtime: r.get(5)?,
                    local_state: r.get(6)?,
                })
            })
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default();
        rows
    }

    /// Transition a file's local state (single writer, instant durability).
    pub fn set_file_state(
        &self,
        project_id: &str,
        path: &str,
        state: &str,
    ) -> Result<(), CairnError> {
        let conn = self.conn.lock().expect("store poisoned");
        conn.execute(
            "UPDATE files SET local_state=?3 WHERE project_id=?1 AND path=?2",
            rusqlite::params![project_id, path, state],
        )
        .map_err(db_err)?;
        Ok(())
    }

    /// Mark a file fully pushed: content identity (manifest hash) lands WITH the synced
    /// state. Without this, the self-pull marks fresh rows as placeholders (manifest
    /// mismatch vs the journal) and every restart re-downloads the whole project.
    pub fn mark_synced(
        &self,
        project_id: &str,
        path: &str,
        manifest_hash: &str,
    ) -> Result<(), CairnError> {
        let conn = self.conn.lock().expect("store poisoned");
        conn.execute(
            "UPDATE files SET manifest_hash=?3, local_state='synced' WHERE project_id=?1 AND path=?2",
            rusqlite::params![project_id, path, manifest_hash],
        )
        .map_err(db_err)?;
        Ok(())
    }

    /// Like [`Store::mark_synced`], but ALSO refreshes the row's stat fields (size +
    /// mtime) so the journaled row matches the on-disk file byte-for-byte after a push.
    /// INVARIANT (punch #5 reconciliation): after a successful push, row.stat ==
    /// file.stat — otherwise every stat-based reconciliation (rescan, reconcile sweep)
    /// sees phantom drift on the just-pushed file and re-pushes forever. Callers pass
    /// the stat of the bytes they actually pushed.
    pub fn mark_synced_with_stat(
        &self,
        project_id: &str,
        path: &str,
        manifest_hash: &str,
        size: u64,
        mtime: i64,
    ) -> Result<(), CairnError> {
        let conn = self.conn.lock().expect("store poisoned");
        conn.execute(
            "UPDATE files SET manifest_hash=?3, local_state='synced', size=?4, mtime=?5 \
             WHERE project_id=?1 AND path=?2",
            rusqlite::params![project_id, path, manifest_hash, size, mtime],
        )
        .map_err(db_err)?;
        Ok(())
    }

    // ---- meta / cursors / devices ----

    /// Read a meta key.
    #[must_use]
    pub fn meta_get(&self, key: &str) -> Option<String> {
        let conn = self.conn.lock().expect("store poisoned");
        conn.query_row(
            "SELECT value FROM meta WHERE key=?1",
            rusqlite::params![key],
            |r| r.get(0),
        )
        .ok()
    }

    /// Write a meta key.
    pub fn meta_set(&self, key: &str, value: &str) -> Result<(), CairnError> {
        let conn = self.conn.lock().expect("store poisoned");
        conn.execute(
            "INSERT INTO meta(key, value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            rusqlite::params![key, value],
        )
        .map_err(db_err)?;
        Ok(())
    }

    /// Remove a meta key (the fork markers' lifecycle end; absent key is a no-op).
    pub fn meta_clear(&self, key: &str) -> Result<(), CairnError> {
        let conn = self.conn.lock().expect("store poisoned");
        conn.execute("DELETE FROM meta WHERE key=?1", rusqlite::params![key])
            .map_err(db_err)?;
        Ok(())
    }

    /// Advance the per-device cursor (durable before any append is acknowledged upstream).
    pub fn set_cursor(
        &self,
        device_id: &str,
        project_id: &str,
        last_seq: u64,
    ) -> Result<(), CairnError> {
        let conn = self.conn.lock().expect("store poisoned");
        conn.execute(
            "INSERT INTO devices(device_id, project_id, last_seq) VALUES(?1,?2,?3)
             ON CONFLICT(device_id, project_id) DO UPDATE SET last_seq=excluded.last_seq",
            rusqlite::params![device_id, project_id, last_seq as i64],
        )
        .map_err(db_err)?;
        Ok(())
    }

    /// Read the per-device cursor.
    #[must_use]
    pub fn get_cursor(&self, device_id: &str, project_id: &str) -> u64 {
        let conn = self.conn.lock().expect("store poisoned");
        conn.query_row(
            "SELECT last_seq FROM devices WHERE device_id=?1 AND project_id=?2",
            rusqlite::params![device_id, project_id],
            |r| r.get::<_, i64>(0),
        )
        .map(|v| v.max(0) as u64)
        .unwrap_or(0)
    }

    // ---- local leases ----

    /// Record a local lease token for a path (legacy, no owning PID recorded).
    pub fn put_lease(&self, path: &str, token: u64, expires_at: i64) -> Result<(), CairnError> {
        self.put_lease_pid(path, token, expires_at, None, None, None)
    }

    /// PID-bound lease record (ADR-0014 Phase 3): the daemon heartbeat renews while
    /// the owning process lives; reaping frees rows whose process died, so a crashed
    /// editor's pen self-releases in seconds instead of needing a human.
    pub fn put_lease_pid(
        &self,
        path: &str,
        token: u64,
        expires_at: i64,
        pid: Option<i64>,
        project_id: Option<&str>,
        device_id: Option<&str>,
    ) -> Result<(), CairnError> {
        let conn = self.conn.lock().expect("store poisoned");
        conn.execute(
            "INSERT INTO leases_local(path, token, expires_at, pid, project_id, device_id)
             VALUES(?1,?2,?3,?4,?5,?6)
             ON CONFLICT(path) DO UPDATE SET token=excluded.token, expires_at=excluded.expires_at,
               pid=excluded.pid, project_id=excluded.project_id, device_id=excluded.device_id",
            rusqlite::params![path, token as i64, expires_at, pid, project_id, device_id],
        )
        .map_err(db_err)?;
        Ok(())
    }

    /// Live lease for a path (expired leases dropped).
    #[must_use]
    pub fn get_lease(&self, path: &str) -> Option<(u64, i64)> {
        let conn = self.conn.lock().expect("store poisoned");
        conn.query_row(
            "SELECT token, expires_at FROM leases_local WHERE path=?1",
            rusqlite::params![path],
            |r| Ok((r.get::<_, i64>(0)?.max(0) as u64, r.get(1)?)),
        )
        .ok()
    }

    /// Drop a lease.
    /// All local leases (ctl `cairn lease` surface): (path, token, expires_at).
    pub fn list_leases(&self) -> Vec<(String, u64, i64)> {
        let conn = self.conn.lock().expect("store poisoned");
        let mut stmt = conn
            .prepare("SELECT path, token, expires_at FROM leases_local ORDER BY path")
            .expect("list_leases query");
        stmt.query_map([], |r| {
            Ok((r.get(0)?, r.get::<_, i64>(1)? as u64, r.get(2)?))
        })
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
    }

    /// All local leases with Phase-3 context (ADR-0014 heartbeat/reaper input).
    pub fn list_leases_pid(&self) -> Vec<LeaseRow> {
        let conn = self.conn.lock().expect("store poisoned");
        let mut stmt = conn
            .prepare(
                "SELECT path, token, expires_at, pid, project_id, device_id
                 FROM leases_local ORDER BY path",
            )
            .expect("list_leases_pid query");
        stmt.query_map([], |r| {
            Ok(LeaseRow {
                path: r.get(0)?,
                token: r.get::<_, i64>(1)?.max(0) as u64,
                expires_at: r.get(2)?,
                pid: r.get(3)?,
                project_id: r.get(4)?,
                device_id: r.get(5)?,
            })
        })
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
    }

    pub fn drop_lease(&self, path: &str) -> Result<(), CairnError> {
        let conn = self.conn.lock().expect("store poisoned");
        conn.execute(
            "DELETE FROM leases_local WHERE path=?1",
            rusqlite::params![path],
        )
        .map_err(db_err)?;
        Ok(())
    }

    /// Aggregate summary across ALL projects (dashboard/status surface).
    pub fn all_files_summary(&self) -> (usize, usize) {
        let conn = self.conn.lock().expect("store poisoned");
        let total: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM files WHERE mode<>'tombstone'",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        let conflicts: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM files WHERE local_state='conflict'",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        (total.max(0) as usize, conflicts.max(0) as usize)
    }

    /// Most recently touched file rows across projects (dashboard activity feed).
    #[must_use]
    pub fn recent_file_rows(&self, limit: usize) -> Vec<FileRow> {
        let conn = self.conn.lock().expect("store poisoned");
        let mut stmt = match conn.prepare(
            "SELECT path, project_id, manifest_hash, size, mode, mtime, local_state
             FROM files ORDER BY mtime DESC LIMIT ?1",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = stmt
            .query_map(rusqlite::params![limit as i64], |r| {
                Ok(FileRow {
                    path: r.get(0)?,
                    project_id: r.get(1)?,
                    manifest_hash: r.get(2)?,
                    size: r.get::<_, i64>(3)?.max(0) as u64,
                    mode: r.get(4)?,
                    mtime: r.get(5)?,
                    local_state: r.get(6)?,
                })
            })
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default();
        rows
    }

    /// Max cursor across devices/projects (headline journal position).
    pub fn max_cursor(&self) -> u64 {
        let conn = self.conn.lock().expect("store poisoned");
        conn.query_row("SELECT COALESCE(MAX(last_seq),0) FROM devices", [], |r| {
            r.get::<_, i64>(0)
        })
        .map(|v| v.max(0) as u64)
        .unwrap_or(0)
    }

    /// Most recent pin events across projects: (project, path, pinned_at).
    /// The dashboard's "latest actions" timeline reads real intent events,
    /// not invented history (WO6-2 pins are the durable record).
    #[must_use]
    pub fn recent_pins(&self, limit: usize) -> Vec<(String, String, i64)> {
        let conn = self.conn.lock().expect("store poisoned");
        let mut stmt = match conn.prepare(
            "SELECT project_id, path, pinned_at FROM pins
             ORDER BY pinned_at DESC LIMIT ?1",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        stmt.query_map(rusqlite::params![limit as i64], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
    }

    /// Daily activity buckets for the dashboard chart: `(day_start_ms,
    /// bytes, files)` for every local day that has at least one touched
    /// file, restricted to `mtime >= cutoff_ms`.
    ///
    /// Day boundaries use the caller's timezone so the chart's weekday
    /// labels match what the user's clock says. `tz_offset_minutes` is the
    /// JS `Date#getTimezoneOffset` convention (UTC minus local, Nairobi
    /// UTC+3 is -180), so `local_ms = utc_ms - tz_offset_minutes * 60_000`.
    ///
    /// Honest telemetry (I4): mtime is "last observed write time", so a
    /// bucket reads "bytes last touched that day" — the label on the chart
    /// says exactly that, never "user activity" as a guess.
    #[must_use]
    pub fn daily_activity(&self, cutoff_ms: i64, tz_offset_minutes: i64) -> Vec<(i64, u64, u64)> {
        let conn = self.conn.lock().expect("store poisoned");
        let mut stmt = match conn
            .prepare("SELECT mtime, size FROM files WHERE mode = 'file' AND mtime >= ?1")
        {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows: Vec<(i64, u64)> = stmt
            .query_map(rusqlite::params![cutoff_ms], |r| {
                Ok((r.get(0)?, r.get::<_, i64>(1)?.max(0) as u64))
            })
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default();
        drop(stmt);
        drop(conn);
        let off = tz_offset_minutes.saturating_mul(60_000);
        let mut days: std::collections::BTreeMap<i64, (u64, u64)> =
            std::collections::BTreeMap::new();
        for (mtime, size) in rows {
            // local_ms = utc_ms - offset (JS convention: offset = UTC - local)
            let day = (mtime - off).div_euclid(86_400_000);
            let e = days.entry(day).or_insert((0, 0));
            e.0 = e.0.saturating_add(size);
            e.1 += 1;
        }
        days.into_iter()
            .map(|(day, (bytes, files))| (day * 86_400_000 + off, bytes, files))
            .collect()
    }

    // ---- pins (WO6-2: pin/unpin/list; eviction protection) ----

    /// Pin a file: records the file-level intent AND pins every local chunk
    /// (blobs.pinned=1) so LRU eviction can never reclaim them.
    pub fn pin_file(&self, project_id: &str, path: &str) -> Result<(), CairnError> {
        if self.get_file(project_id, path).is_none() {
            return Err(CairnError::new(
                ErrorKind::NotFound,
                format!("cannot pin unknown file {path}"),
            ));
        }
        let conn = self.conn.lock().expect("store poisoned");
        conn.execute(
            "INSERT INTO pins(project_id, path, pinned_at) VALUES(?1,?2,?3)
             ON CONFLICT(project_id, path) DO UPDATE SET pinned_at=excluded.pinned_at",
            rusqlite::params![project_id, path, self.clock.now_millis()],
        )
        .map_err(|e| CairnError::new(ErrorKind::Io, format!("pin_file: {e}")))?;
        drop(conn);
        self.pin_file_chunks(project_id, path);
        Ok(())
    }

    /// Unpin: clears the intent AND the chunk pins (chunks become evictable again).
    pub fn unpin_file(&self, project_id: &str, path: &str) -> Result<(), CairnError> {
        let conn = self.conn.lock().expect("store poisoned");
        conn.execute(
            "DELETE FROM pins WHERE project_id=?1 AND path=?2",
            rusqlite::params![project_id, path],
        )
        .map_err(|e| CairnError::new(ErrorKind::Io, format!("unpin_file: {e}")))?;
        Ok(())
    }

    /// All pins for a project: (path, size) — the ctl ListPins surface.
    pub fn list_pins(&self, project_id: &str) -> Vec<(String, u64)> {
        let conn = self.conn.lock().expect("store poisoned");
        let mut stmt = conn
            .prepare(
                "SELECT p.path, COALESCE(f.size, 0) FROM pins p
                 LEFT JOIN files f ON f.project_id = p.project_id AND f.path = p.path
                 WHERE p.project_id = ?1 ORDER BY p.path",
            )
            .expect("list_pins query");
        stmt.query_map([project_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64))
        })
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
    }

    /// File-level pin check (ctl restore/eviction protection).
    pub fn is_pinned(&self, project_id: &str, path: &str) -> bool {
        let conn = self.conn.lock().expect("store poisoned");
        conn.query_row(
            "SELECT 1 FROM pins WHERE project_id=?1 AND path=?2",
            rusqlite::params![project_id, path],
            |_| Ok(()),
        )
        .is_ok()
    }

    /// Pin all chunks belonging to a file's current manifest in the LOCAL CAS.
    /// The manifest itself is fetched from the local CAS (it was stored at
    /// hydration/push time); missing manifest = nothing local to pin (returns 0).
    fn pin_file_chunks(&self, project_id: &str, path: &str) -> usize {
        use cairn_core::hash::Hash;
        use cairn_core::manifest::Manifest;
        let Some(row) = self.get_file(project_id, path) else {
            return 0;
        };
        let Some(hex) = row.manifest_hash.clone() else {
            return 0;
        };
        let Some(h) = Hash::from_hex(&hex) else {
            return 0;
        };
        let Ok(cas) = crate::Cas::open(&self.root.join("blobs"), self.conn_handle()) else {
            return 0;
        };
        let Ok(bytes) = cas.get(&h) else {
            return 0;
        };
        let Ok(manifest) = Manifest::parse(&bytes) else {
            return 0;
        };
        // Fanout-safe (review round): `flatten()` is leaf-only — a Node returned zero
        // entries and pinning silently protected NOTHING on >8,192-chunk files, letting
        // LRU eviction free pinned-class chunks. Walk the tree via the CAS (child
        // manifest objects are mirrored locally by the push path).
        let mut n = 0;
        for e in manifest.flatten_deep(&mut |h| cas.get(h).ok()) {
            if cas.pin(&e.chunk_hash).is_ok() {
                n += 1;
            }
        }
        n
    }

    pub fn with_tx<T>(
        &self,
        f: impl FnOnce(&Connection) -> Result<T, CairnError>,
    ) -> Result<T, CairnError> {
        let mut conn = self.conn.lock().expect("store poisoned");
        let tx = conn.transaction().map_err(db_err)?;
        let out = f(&tx)?;
        tx.commit().map_err(db_err)?;
        Ok(out)
    }

    /// Clock accessor (atime stamping).
    #[must_use]
    pub fn clock(&self) -> std::sync::Arc<dyn SystemClock> {
        self.clock.clone()
    }
}

/// Is a process alive on THIS device? (ADR-0014 Phase 3 reaping primitive.)
/// `kill(pid, 0)` returns Ok (signal permitted) or EPERM (exists, not ours) for live
/// processes; ESRCH means gone. Fail-safe for callers: only ESRCH counts as dead.
#[cfg(unix)]
#[must_use]
#[allow(unsafe_code)] // process-alive probe: kill(pid, 0) — same class as the eviction probes
pub fn process_alive(pid: i64) -> bool {
    if pid <= 0 {
        return false;
    }
    // SAFETY: kill with signal 0 performs only the existence/permission check —
    // no memory is touched, no signal is delivered.
    let r = unsafe { libc::kill(pid as libc::pid_t, 0) };
    r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Windows twin of [`process_alive`]: `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)`
/// succeeds for any existing process we can query; a stale PID fails to open.
#[cfg(windows)]
#[must_use]
#[allow(unsafe_code)] // process-alive probe: OpenProcess/CloseHandle — same class as eviction probes
pub fn process_alive(pid: i64) -> bool {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    if pid <= 0 {
        return false;
    }
    // SAFETY: OpenProcess only reads the process table; the handle (if any) is
    // closed on every path below.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid as u32) };
    let alive = handle.is_ok();
    if let Ok(h) = handle {
        // SAFETY: h came from a successful OpenProcess and is closed exactly once.
        unsafe {
            let _ = CloseHandle(h);
        }
    }
    alive
}

/// One local lease row with its Phase-3 context (ADR-0014).
#[derive(Debug, Clone)]
pub struct LeaseRow {
    pub path: String,
    pub token: u64,
    pub expires_at: i64,
    /// Owning process on this device (None = legacy row).
    pub pid: Option<i64>,
    /// Project the lease belongs to (None = legacy row).
    pub project_id: Option<String>,
    /// Acquiring device (None = legacy row).
    pub device_id: Option<String>,
}

fn db_err(e: rusqlite::Error) -> CairnError {
    CairnError::new(cairn_core::ErrorKind::Io, format!("sqlite: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cairn_core::clock::WallClock;
    use std::sync::Arc;

    fn open_tmp() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().expect("tmp");
        let store = Store::open(dir.path(), Arc::new(WallClock)).expect("open");
        (dir, store)
    }

    #[test]
    fn migration_sets_user_version_and_tables_exist() {
        let (_d, s) = open_tmp();
        assert_eq!(s.schema_version().unwrap(), CLIENT_SCHEMA_VERSION);
        assert!(s.get_file("p1", "a.mov").is_none());
    }

    fn file_row(path: &str, state: &str) -> FileRow {
        FileRow {
            path: path.into(),
            project_id: "p1".into(),
            manifest_hash: None,
            size: 10,
            mode: "file".into(),
            mtime: 1,
            local_state: state.into(),
        }
    }

    #[test]
    fn list_files_in_states_matches_list_files_subset() {
        let (_d, s) = open_tmp();
        s.put_file(&file_row("a.mov", "dirty")).unwrap();
        s.put_file(&file_row("b.mov", "synced")).unwrap();
        s.put_file(&file_row("c.mov", "conflict")).unwrap();
        let all = s.list_files("p1");
        assert_eq!(all.len(), 3);
        // path order preserved
        let mut got: Vec<String> = s
            .list_files_in_states("p1", &["dirty", "conflict"])
            .into_iter()
            .map(|r| r.path)
            .collect();
        got.sort();
        assert_eq!(got, vec!["a.mov".to_string(), "c.mov".to_string()]);
        assert!(s.list_files_in_states("p1", &[]).is_empty());
        assert!(s.list_files_in_states("p1", &["nope"]).is_empty());
    }

    #[test]
    fn network_mount_tripwire() {
        use std::path::Path;
        assert!(super::root_on_network_mount(Path::new(
            r"\\nas\share\cairn"
        )));
        assert!(super::root_on_network_mount(Path::new("//nas/share/cairn")));
        assert!(!super::root_on_network_mount(Path::new(
            r"C:\Users\ed\cairn-home"
        )));
        assert!(!super::root_on_network_mount(Path::new("relative/home")));
    }

    /// Hardened open discipline: WAL + NORMAL + bounded autocheckpoint must
    /// be exactly what the connection serves, read back (never trust that
    /// a pragma stuck — pooling layers rebuild connections silently).
    #[test]
    fn open_applies_hardened_pragma_stack() {
        let (_d, s) = open_tmp();
        let conn = s.conn.lock().expect("store poisoned");
        let journal: String = conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(journal.to_lowercase(), "wal");
        let synchronous: i64 = conn
            .query_row("PRAGMA synchronous", [], |r| r.get(0))
            .unwrap();
        assert_eq!(synchronous, 1, "NORMAL == 1");
        let autocheckpoint: i64 = conn
            .query_row("PRAGMA wal_autocheckpoint", [], |r| r.get(0))
            .unwrap();
        assert_eq!(autocheckpoint, 1000);
    }

    /// ADR-0014 Phase 3: pid-bound leases renew in place, a DEAD owner's row is
    /// reaped, and legacy rows (pid=None) are left for TTL expiry.
    #[test]
    fn lease_pid_lifecycle_renew_reap_legacy() {
        let (_d, s) = open_tmp();
        let me = i64::from(std::process::id());
        s.put_lease_pid("live.prproj", 7, 1_000, Some(me), Some("p1"), Some("dev1"))
            .unwrap();
        s.put_lease_pid(
            "dead.prproj",
            8,
            2_000,
            Some(999_999_999),
            Some("p1"),
            Some("dev1"),
        )
        .unwrap();
        s.put_lease("legacy.prproj", 9, 3_000).unwrap();

        let rows = s.list_leases_pid();
        assert_eq!(rows.len(), 3);
        let live = rows.iter().find(|r| r.path == "live.prproj").unwrap();
        assert_eq!(
            (live.token, live.pid, live.project_id.as_deref()),
            (7, Some(me), Some("p1"))
        );

        // "heartbeat": renew the live row in place (same token, new expiry, same pid)
        s.put_lease_pid("live.prproj", 7, 5_000, Some(me), Some("p1"), Some("dev1"))
            .unwrap();
        assert_eq!(s.get_lease("live.prproj"), Some((7, 5_000)));

        // reap: dead owner's row disappears; mine and legacy stay
        let reaped: Vec<String> = s
            .list_leases_pid()
            .into_iter()
            .filter(|r| matches!(r.pid, Some(p) if p > 0 && !process_alive(p)))
            .map(|r| r.path)
            .collect();
        assert_eq!(reaped, vec!["dead.prproj".to_string()]);
        for p in &reaped {
            s.drop_lease(p).unwrap();
        }
        let remaining: Vec<String> = s.list_leases_pid().into_iter().map(|r| r.path).collect();
        assert_eq!(remaining, vec!["legacy.prproj", "live.prproj"]);
        assert!(process_alive(me), "self-probe must see a live process");
        assert!(
            !process_alive(999_999_999),
            "implausibly large pid must be dead"
        );
    }

    #[test]
    fn file_roundtrip_and_state_transition() {
        let (_d, s) = open_tmp();
        s.put_file(&FileRow {
            path: "A001.mov".into(),
            project_id: "p1".into(),
            manifest_hash: Some("ab".repeat(32)),
            size: 42,
            mode: "file".into(),
            mtime: 1,
            local_state: "dirty".into(),
        })
        .unwrap();
        let f = s.get_file("p1", "A001.mov").unwrap();
        assert_eq!(f.size, 42);
        s.set_file_state("p1", "A001.mov", "synced").unwrap();
        assert_eq!(s.get_file("p1", "A001.mov").unwrap().local_state, "synced");
    }

    /// Round 25: the dashboard's honest event sources — cross-project pin
    /// events and timezone-correct daily byte buckets.
    #[test]
    fn recent_pins_and_daily_activity_buckets() {
        let (_d, s) = open_tmp();
        let day = 86_400_000i64;
        let put = |path: &str, size: u64, mtime: i64| {
            s.put_file(&FileRow {
                path: path.into(),
                project_id: "p1".into(),
                manifest_hash: None,
                size,
                mode: "file".into(),
                mtime,
                local_state: "synced".into(),
            })
            .unwrap();
        };
        // two files on day 0, one on day 1, one far in the past (filtered)
        put("a.mov", 100, 1_000);
        put("b.mov", 150, 2_000);
        put("c.mov", 50, day + 3_000);
        put("old.mov", 999, -90 * day);

        // UTC days (tz offset 0): bucket starts land on UTC midnights
        let buckets = s.daily_activity(0, 0);
        assert_eq!(buckets.len(), 2, "old.mov filtered, two days remain");
        assert_eq!(buckets[0], (0, 250, 2));
        assert_eq!(buckets[1], (day, 50, 1));

        // UTC+3 (JS offset -180): 23:00 UTC on day 0 belongs to local day 1
        put("late.wav", 10, day - 3_600_000);
        let east = s.daily_activity(0, -180);
        assert_eq!(east.len(), 2);
        assert_eq!(
            east[0],
            (-180 * 60_000, 250, 2),
            "day 0 unchanged; its bucket start is local midnight in UTC ms (-3h)"
        );
        assert_eq!(
            east[1],
            (day - 180 * 60_000, 60, 2),
            "late.wav joins c.mov on local day 1; bucket start is local midnight in UTC ms"
        );

        // cutoff excludes everything older than it
        assert_eq!(s.daily_activity(day, 0).len(), 1);

        // pins: newest first, real timestamps from the pin clock
        s.pin_file("p1", "a.mov").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        s.pin_file("p1", "b.mov").unwrap();
        let pins = s.recent_pins(10);
        assert_eq!(pins.len(), 2);
        assert_eq!(pins[0].1, "b.mov", "newest pin first");
        assert!(pins[0].2 >= pins[1].2);
        let one = s.recent_pins(1);
        assert_eq!(one.len(), 1, "limit is honored");
    }

    #[test]
    fn cursor_durability_and_leases() {
        let (_d, s) = open_tmp();
        s.set_cursor("d1", "p1", 1234).unwrap();
        assert_eq!(s.get_cursor("d1", "p1"), 1234);
        s.put_lease("scene.prproj", 77, 9_999_999_999_999).unwrap();
        assert_eq!(s.get_lease("scene.prproj").unwrap().0, 77);
        s.drop_lease("scene.prproj").unwrap();
        assert!(s.get_lease("scene.prproj").is_none());
    }

    #[test]
    fn transaction_is_atomic() {
        let (_d, s) = open_tmp();
        let r: Result<(), CairnError> = s.with_tx(|conn| {
            conn.execute("INSERT INTO meta(key,value) VALUES('a','1')", [])
                .map_err(db_err)?;
            conn.execute("INSERT INTO meta(key,value) VALUES('b','2')", [])
                .map_err(db_err)?;
            Err(CairnError::new(cairn_core::ErrorKind::Io, "boom"))
        });
        assert!(r.is_err());
        assert!(
            s.meta_get("a").is_none(),
            "rollback must erase uncommitted writes"
        );
        let r2: Result<(), CairnError> = s.with_tx(|conn| {
            conn.execute("INSERT INTO meta(key,value) VALUES('a','1')", [])
                .map_err(db_err)?;
            Ok(())
        });
        r2.unwrap();
        assert_eq!(s.meta_get("a").unwrap(), "1");
    }
}

#[cfg(test)]
mod pin_tests {
    use super::*;
    use crate::FileRow;

    #[test]
    fn pins_roundtrip_and_migration_v2() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(
            dir.path(),
            std::sync::Arc::new(cairn_core::clock::WallClock),
        )
        .unwrap();
        assert_eq!(
            s.schema_version().unwrap(),
            CLIENT_SCHEMA_VERSION,
            "pins + lease-ctx + state-records migrations applied"
        );
        s.put_file(&FileRow {
            path: "hero.prproj".into(),
            project_id: "p1".into(),
            manifest_hash: None,
            size: 4096,
            mode: "file".into(),
            mtime: 1,
            local_state: "synced".into(),
        })
        .unwrap();
        s.pin_file("p1", "hero.prproj").unwrap();
        let pins = s.list_pins("p1");
        assert_eq!(pins.len(), 1);
        assert_eq!(pins[0].0, "hero.prproj");
        assert_eq!(pins[0].1, 4096);
        assert!(s.is_pinned("p1", "hero.prproj"));
        s.unpin_file("p1", "hero.prproj").unwrap();
        assert!(s.list_pins("p1").is_empty());
        assert!(!s.is_pinned("p1", "hero.prproj"));
        // pinning an unknown row fails loudly (the ctl surface depends on it)
        assert!(s.pin_file("p1", "missing.mov").is_err());
    }
}
