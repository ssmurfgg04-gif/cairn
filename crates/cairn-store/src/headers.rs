//! Header cache — first 2MB + last 1MB per file pointer (SPEC §5.1), the I1 gate:
//! placeholder → first byte of file header served <50ms from here (cached).
//!
//! Reader pool (WO6-5 finding, burst CI evidence 2026-09-02): serving from the store's
//! single `Mutex<Connection>` serializes every header serve behind every other store
//! op — at 32 concurrent opens the p95 tail blew the 50ms I1 gate on busy CI runners
//! (header-serve p50 48ms vs 3µs local). `with_read_pool` opens dedicated query-only
//! SQLite connections (WAL: concurrent readers are safe and see the latest commit),
//! so burst reads never queue behind writes or each other. Falls back to the shared
//! connection whenever a pooled reader is unavailable — the pool is an optimization,
//! never a dependency.

use cairn_core::CairnError;
use rusqlite::Connection;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Header cache over the client store.
#[derive(Clone)]
pub struct HeaderCache {
    db: Arc<Mutex<Connection>>,
    /// Idle pooled read connections (dedicated readers; `serve` pops/pushes).
    readers: Arc<Mutex<Vec<Connection>>>,
    /// DB path for lazily topping the pool up if it drains (None = pool disabled).
    db_path: Option<std::path::PathBuf>,
    /// Pool telemetry (burst bench surface): peak simultaneous readers in use.
    peak_in_use: Arc<std::sync::atomic::AtomicUsize>,
    in_use: Arc<std::sync::atomic::AtomicUsize>,
}

/// Served header bytes: head (up to 2MB) + optional tail (up to 1MB).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedHeader {
    /// First bytes of the file (container/moov region for NLEs).
    pub head: Vec<u8>,
    /// Last bytes of the file (indexes, moov-at-end).
    pub tail: Option<Vec<u8>>,
}

impl HeaderCache {
    /// New header cache over the shared connection (no pool — single-reader mode).
    #[must_use]
    pub fn new(db: Arc<Mutex<Connection>>) -> Self {
        HeaderCache {
            db,
            readers: Arc::new(Mutex::new(Vec::new())),
            db_path: None,
            peak_in_use: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            in_use: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    /// New header cache with a dedicated reader pool (burst/32-worker I1 mode).
    /// Readers are `PRAGMA query_only` connections to the same WAL db; failures to
    /// open degrade silently to shared-connection serving (never a hard dependency).
    pub fn with_read_pool(db: Arc<Mutex<Connection>>, db_path: &Path, readers: usize) -> Self {
        let mut pool = Vec::new();
        for i in 0..readers {
            if let Ok(c) = Connection::open(db_path) {
                let _ = c.busy_timeout(std::time::Duration::from_millis(5000));
                if c.pragma_update(None, "query_only", "ON").is_ok()
                    && c.pragma_update(None, "journal_mode", "WAL").is_ok()
                {
                    pool.push(c);
                }
            } else if i == 0 {
                tracing::debug!("header read pool: no extra readers (shared-conn mode)");
            }
        }
        HeaderCache {
            db,
            readers: Arc::new(Mutex::new(pool)),
            db_path: Some(db_path.to_path_buf()),
            peak_in_use: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            in_use: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    /// Store a pointer's header (idempotent per pointer hash). Always via the shared
    /// connection: writes serialize; reads (the hot 99%) do not.
    pub fn put(
        &self,
        pointer_hash: &str,
        head: &[u8],
        tail: Option<&[u8]>,
    ) -> Result<(), CairnError> {
        let db = self.db.lock().expect("headers poisoned");
        db.execute(
            "INSERT INTO dir_headers(pointer_hash, head, tail) VALUES(?1,?2,?3)
             ON CONFLICT(pointer_hash) DO UPDATE SET head=excluded.head, tail=excluded.tail",
            rusqlite::params![pointer_hash, head, tail],
        )
        .map_err(|e| CairnError::new(cairn_core::ErrorKind::Io, format!("header put: {e}")))?;
        Ok(())
    }

    /// Serve the cached header. Measured latency goes to `cairn_hydration_first_byte_ms` (I1).
    pub fn serve(&self, pointer_hash: &str) -> Result<CachedHeader, CairnError> {
        let err = || CairnError::new(cairn_core::ErrorKind::NotFound, "header not cached");
        // fast path: a dedicated pooled reader (no contention with store writes)
        let conn = self.readers.lock().expect("header pool poisoned").pop();
        if let Some(c) = conn {
            self.in_use
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.peak_in_use.fetch_max(
                self.in_use.load(std::sync::atomic::Ordering::Relaxed),
                std::sync::atomic::Ordering::Relaxed,
            );
            let res = c
                .query_row(
                    "SELECT head, tail FROM dir_headers WHERE pointer_hash=?1",
                    rusqlite::params![pointer_hash],
                    |r| {
                        Ok(CachedHeader {
                            head: r.get(0)?,
                            tail: r.get(1)?,
                        })
                    },
                )
                .map_err(|_| err());
            self.readers.lock().expect("header pool poisoned").push(c);
            self.in_use
                .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            return res;
        }
        // pool drained (or disabled): lazily top up once, else share the main conn
        if let Some(path) = &self.db_path {
            if let Ok(c) = Connection::open(path) {
                if c.pragma_update(None, "query_only", "ON").is_ok() {
                    let res = c
                        .query_row(
                            "SELECT head, tail FROM dir_headers WHERE pointer_hash=?1",
                            rusqlite::params![pointer_hash],
                            |r| {
                                Ok(CachedHeader {
                                    head: r.get(0)?,
                                    tail: r.get(1)?,
                                })
                            },
                        )
                        .map_err(|_| err());
                    self.readers.lock().expect("header pool poisoned").push(c);
                    return res;
                }
            }
        }
        let db = self.db.lock().expect("headers poisoned");
        db.query_row(
            "SELECT head, tail FROM dir_headers WHERE pointer_hash=?1",
            rusqlite::params![pointer_hash],
            |r| {
                Ok(CachedHeader {
                    head: r.get(0)?,
                    tail: r.get(1)?,
                })
            },
        )
        .map_err(|_| err())
    }

    /// Serve + measure (I1 instrumentation point; daemon reports the gauge).
    pub fn serve_measured(
        &self,
        pointer_hash: &str,
    ) -> Result<(CachedHeader, std::time::Duration), CairnError> {
        let t0 = Instant::now();
        let h = self.serve(pointer_hash)?;
        Ok((h, t0.elapsed()))
    }

    /// Pool telemetry: peak simultaneous readers observed (burst bench surface).
    #[must_use]
    pub fn peak_readers_in_use(&self) -> usize {
        self.peak_in_use.load(std::sync::atomic::Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_hdr() -> (tempfile::TempDir, HeaderCache) {
        let dir = tempfile::tempdir().unwrap();
        let conn = Connection::open(dir.path().join("h.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE dir_headers(pointer_hash TEXT PRIMARY KEY, head BLOB, tail BLOB);",
        )
        .unwrap();
        (dir, HeaderCache::new(Arc::new(Mutex::new(conn))))
    }

    /// I1 gate: cached header serve <50ms (SPEC §2). Generous CI bound: assert <50ms; on this
    /// box it is measured at well under 1ms for 2MB blobs.
    #[test]
    fn i1_cached_header_serve_under_50ms() {
        let (_d, hdr) = open_hdr();
        let head = vec![7u8; cairn_core::HEADER_HEAD_BYTES];
        let tail = vec![9u8; cairn_core::HEADER_TAIL_BYTES];
        hdr.put("ptr-1", &head, Some(&tail)).unwrap();
        for _ in 0..10 {
            let (_h, dt) = hdr.serve_measured("ptr-1").unwrap();
            assert!(
                dt.as_secs_f64() * 1000.0 < cairn_core::I1_TARGET_CACHED_MS,
                "I1 violated: cached serve took {dt:?}"
            );
        }
    }

    #[test]
    fn roundtrip_and_miss() {
        let (_d, hdr) = open_hdr();
        hdr.put("p", b"0123456789", Some(b"tail")).unwrap();
        let h = hdr.serve("p").unwrap();
        assert_eq!(h.head, b"0123456789");
        assert_eq!(h.tail, Some(b"tail".to_vec()));
        assert!(hdr.serve("missing").is_err());
    }

    /// Reader pool actually PARALLELIZES: 8 threads hammering one pointer must be
    /// observed using >1 pooled reader simultaneously (the WO6-5 fix, pinned).
    #[test]
    fn read_pool_serves_concurrently() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("db.sqlite");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE dir_headers(pointer_hash TEXT PRIMARY KEY, head BLOB, tail BLOB);",
        )
        .unwrap();
        let hdr = HeaderCache::with_read_pool(Arc::new(Mutex::new(conn)), &db_path, 4);
        hdr.put("hot", &vec![3u8; 4096], None).unwrap();
        let mut handles = Vec::new();
        for _ in 0..8 {
            let h = hdr.clone();
            handles.push(std::thread::spawn(move || {
                for _ in 0..200 {
                    h.serve("hot").unwrap();
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert!(
            hdr.peak_readers_in_use() > 1,
            "pool never used >1 reader concurrently — it is not parallel"
        );
    }
}
