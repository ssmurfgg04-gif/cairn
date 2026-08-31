//! Client SQLite store — SPEC §5.3.
//!
//! - WAL mode + `busy_timeout=5000`
//! - migrations via `PRAGMA user_version`
//! - single writer: one serialized connection behind a mutex (daemon has one writer task)

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use cairn_core::clock::SystemClock;
use cairn_core::CairnError;
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
pub const CLIENT_SCHEMA_VERSION: i64 = 1;

impl Store {
    /// Open (or create) the store at `root` (a directory). Applies migrations.
    pub fn open(root: &Path, clock: std::sync::Arc<dyn SystemClock>) -> Result<Self, CairnError> {
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
        let mut stmt = match conn.prepare(
            "SELECT path, project_id, manifest_hash, size, mode, mtime, local_state
             FROM files WHERE project_id=?1 ORDER BY path",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = stmt
            .query_map(rusqlite::params![project_id], |r| {
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

    /// Record a local lease token for a path.
    pub fn put_lease(&self, path: &str, token: u64, expires_at: i64) -> Result<(), CairnError> {
        let conn = self.conn.lock().expect("store poisoned");
        conn.execute(
            "INSERT INTO leases_local(path, token, expires_at) VALUES(?1,?2,?3)
             ON CONFLICT(path) DO UPDATE SET token=excluded.token, expires_at=excluded.expires_at",
            rusqlite::params![path, token as i64, expires_at],
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

    /// Run a closure in a single serialized transaction (single-writer discipline).
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
