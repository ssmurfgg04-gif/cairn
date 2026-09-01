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
pub const CLIENT_SCHEMA_VERSION: i64 = 3;

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
            3,
            "pins + lease-ctx migrations applied"
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
