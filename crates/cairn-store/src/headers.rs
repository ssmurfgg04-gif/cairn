//! Header cache — first 2MB + last 1MB per file pointer (SPEC §5.1), the I1 gate:
//! placeholder → first byte of file header served <50ms from here (cached).

use cairn_core::CairnError;
use rusqlite::Connection;
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Header cache over the client store.
#[derive(Clone)]
pub struct HeaderCache {
    db: Arc<Mutex<Connection>>,
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
    /// New header cache over the shared connection.
    #[must_use]
    pub fn new(db: Arc<Mutex<Connection>>) -> Self {
        HeaderCache { db }
    }

    /// Store a pointer's header (idempotent per pointer hash).
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
        .map_err(|_| CairnError::new(cairn_core::ErrorKind::NotFound, "header not cached"))
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
}
