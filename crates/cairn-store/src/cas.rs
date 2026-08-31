//! Local content-addressed chunk store (SPEC §5.3 `blobs` table + on-disk CAS).
//!
//! Layout: `<root>/blobs/{hash[0:2]}/{hash}`. Writes are atomic (temp + fsync + rename).
//! Chunk identity is BLAKE3 of raw content — every write verifies before landing (I2).
//! `pinned` chunks are excluded from local eviction (ctl pin/unpin, SPEC §10/§11).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use cairn_core::clock::SystemClock;
use cairn_core::hash::Hash;
use cairn_core::{CairnError, ErrorKind};
use rusqlite::Connection;

/// Local chunk CAS with SQLite index.
#[derive(Clone)]
pub struct Cas {
    root: PathBuf,
    db: Arc<std::sync::Mutex<Connection>>,
}

impl Cas {
    /// Open CAS rooted at `root` (typically `<store_root>/blobs` dir + shared db).
    pub fn open(
        root: &Path,
        db: Arc<std::sync::Mutex<Connection>>,
    ) -> Result<Self, CairnError> {
        std::fs::create_dir_all(root.join("data"))
            .map_err(|e| CairnError::new(ErrorKind::Io, format!("cas mkdir: {e}")))?;
        Ok(Cas { root: root.to_path_buf(), db })
    }

    fn path_for(&self, h: &Hash) -> PathBuf {
        self.root.join("data").join(h.shard()).join(h.hex())
    }

    /// Insert a chunk (verifies BLAKE3 before landing; idempotent by content address).
    pub fn put(&self, expected: &Hash, bytes: &[u8]) -> Result<(), CairnError> {
        let actual = Hash::of(bytes);
        if &actual != expected {
            return Err(CairnError::new(
                ErrorKind::ChunkVerification,
                format!("CAS put rejected: expected {expected}, got {actual}"),
            ));
        }
        let path = self.path_for(expected);
        if path.exists() {
            self.touch(expected)?;
            return Ok(());
        }
        let dir = path.parent().expect("shard dir");
        std::fs::create_dir_all(dir)
            .map_err(|e| CairnError::new(ErrorKind::Io, format!("cas shard mkdir: {e}")))?;
        let tmp = dir.join(format!(".tmp-{}", cairn_core::ids::new_device_id()));
        std::fs::write(&tmp, bytes)
            .map_err(|e| CairnError::new(ErrorKind::Io, format!("cas tmp write: {e}")))?;
        // fsync before rename: the rename is the durability barrier (I2)
        let f = std::fs::File::open(&tmp)
            .map_err(|e| CairnError::new(ErrorKind::Io, format!("cas reopen: {e}")))?;
        f.sync_all()
            .map_err(|e| CairnError::new(ErrorKind::Io, format!("cas fsync: {e}")))?;
        drop(f);
        std::fs::rename(&tmp, &path)
            .map_err(|e| CairnError::new(ErrorKind::Io, format!("cas rename: {e}")))?;
        let now = self.clock_now();
        let db = self.db.lock().expect("cas db poisoned");
        db.execute(
            "INSERT INTO blobs(hash, size, atime, pinned) VALUES(?1,?2,?3,0)
             ON CONFLICT(hash) DO UPDATE SET size=excluded.size, atime=excluded.atime",
            rusqlite::params![expected.hex(), bytes.len() as i64, now],
        )
        .map_err(|e| CairnError::new(ErrorKind::Io, format!("blobs row: {e}")))?;
        Ok(())
    }

    /// Read a chunk (updates atime).
    pub fn get(&self, h: &Hash) -> Result<Vec<u8>, CairnError> {
        let path = self.path_for(h);
        let bytes = std::fs::read(&path)
            .map_err(|_| CairnError::new(ErrorKind::NotFound, format!("chunk {h} not in local CAS")))?;
        // I2: verify on ingest, "free at 10+GB/s" (SPEC §9.2)
        let actual = Hash::of(&bytes);
        if actual != *h {
            return Err(CairnError::new(
                ErrorKind::LocalCasCorrupt,
                format!("local CAS corruption: {h}"),
            ));
        }
        self.touch(h)?;
        Ok(bytes)
    }

    /// Whether a chunk is present locally (by index; content verified on `get`).
    #[must_use]
    pub fn contains(&self, h: &Hash) -> bool {
        self.path_for(h).exists()
    }

    /// Pin a chunk (never evicted).
    pub fn pin(&self, h: &Hash) -> Result<(), CairnError> {
        let db = self.db.lock().expect("cas db poisoned");
        db.execute(
            "UPDATE blobs SET pinned=1 WHERE hash=?1",
            rusqlite::params![h.hex()],
        )
        .map_err(|e| CairnError::new(ErrorKind::Io, format!("pin: {e}")))?;
        Ok(())
    }

    /// Unpin a chunk.
    pub fn unpin(&self, h: &Hash) -> Result<(), CairnError> {
        let db = self.db.lock().expect("cas db poisoned");
        db.execute("UPDATE blobs SET pinned=0 WHERE hash=?1", rusqlite::params![h.hex()])
            .map_err(|e| CairnError::new(ErrorKind::Io, format!("unpin: {e}")))?;
        Ok(())
    }

    /// Local GC: evict least-recently-used unpinned chunks down to `target_bytes`.
    /// Returns evicted hashes. Pinned chunks are never touched (SPEC §10).
    pub fn evict_to(&self, target_bytes: u64) -> Result<Vec<String>, CairnError> {
        let db = self.db.lock().expect("cas db poisoned");
        let mut stmt = db
            .prepare(
                "SELECT hash, size, pinned FROM blobs WHERE pinned=0 ORDER BY atime ASC",
            )
            .map_err(|e| CairnError::new(ErrorKind::Io, format!("evict q: {e}")))?;
        let rows: Vec<(String, i64)> = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
            .map_err(|e| CairnError::new(ErrorKind::Io, format!("evict map: {e}")))?
            .filter_map(|r| r.ok())
            .collect();
        drop(stmt);
        let mut live: u64 = db
            .query_row("SELECT COALESCE(SUM(size),0) FROM blobs", [], |r| r.get::<_, i64>(0))
            .map_err(|e| CairnError::new(ErrorKind::Io, format!("sum: {e}")))? as u64;
        let mut evicted = Vec::new();
        for (hash, size) in rows {
            if live <= target_bytes {
                break;
            }
            let h = Hash::from_hex(&hash);
            let Some(h) = h else { continue };
            let path = self.path_for(&h);
            if std::fs::remove_file(&path).is_ok() {
                live = live.saturating_sub(size as u64);
                evicted.push(hash);
            }
        }
        for h in &evicted {
            db.execute("DELETE FROM blobs WHERE hash=?1", rusqlite::params![h])
                .map_err(|e| CairnError::new(ErrorKind::Io, format!("evict row: {e}")))?;
        }
        Ok(evicted)
    }

    /// Integrity scan of a sample of local chunks (doctor).
    pub fn verify_sample(&self, limit: usize) -> Result<(usize, Vec<String>), CairnError> {
        let db = self.db.lock().expect("cas db poisoned");
        let mut stmt = db
            .prepare("SELECT hash FROM blobs LIMIT ?1")
            .map_err(|e| CairnError::new(ErrorKind::Io, format!("verify q: {e}")))?;
        let hashes: Vec<String> = stmt
            .query_map(rusqlite::params![limit as i64], |r| r.get(0))
            .map_err(|e| CairnError::new(ErrorKind::Io, format!("verify map: {e}")))?
            .filter_map(|r| r.ok())
            .collect();
        drop(stmt);
        let mut bad = Vec::new();
        for hex in hashes {
            if let Some(h) = Hash::from_hex(&hex) {
                let path = self.path_for(&h);
                if let Ok(bytes) = std::fs::read(&path) {
                    if Hash::of(&bytes) != h {
                        bad.push(hex);
                    }
                } else {
                    bad.push(hex);
                }
            }
        }
        Ok((limit, bad))
    }

    fn touch(&self, h: &Hash) -> Result<(), CairnError> {
        let db = self.db.lock().expect("cas db poisoned");
        db.execute(
            "UPDATE blobs SET atime=?2 WHERE hash=?1",
            rusqlite::params![h.hex(), self.clock_now()],
        )
        .map_err(|e| CairnError::new(ErrorKind::Io, format!("touch: {e}")))?;
        Ok(())
    }

    fn clock_now(&self) -> i64 {
        cairn_core::clock::WallClock.now_millis()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_cas() -> (tempfile::TempDir, Cas) {
        let dir = tempfile::tempdir().expect("tmp");
        let conn = Connection::open(dir.path().join("cas.db")).expect("db");
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS blobs(hash TEXT PRIMARY KEY, size INTEGER, atime INTEGER, pinned INTEGER DEFAULT 0);",
        )
        .unwrap();
        let cas = Cas::open(
            dir.path(),
            Arc::new(std::sync::Mutex::new(conn)),
        )
        .unwrap();
        (dir, cas)
    }

    #[test]
    fn put_get_verify_roundtrip() {
        let (_d, cas) = open_cas();
        let bytes = b"hello cairn chunk".to_vec();
        let h = Hash::of(&bytes);
        cas.put(&h, &bytes).unwrap();
        assert!(cas.contains(&h));
        assert_eq!(cas.get(&h).unwrap(), bytes);
    }

    #[test]
    fn rejects_hash_mismatch_never_lands_corrupt() {
        let (_d, cas) = open_cas();
        let bytes = b"payload";
        let wrong = Hash::of(b"not-the-payload");
        let err = cas.put(&wrong, bytes).unwrap_err();
        assert_eq!(err.code(), "CHECKSUM_MISMATCH");
        assert!(!cas.contains(&wrong), "corrupt chunk must never land (I2)");
    }

    #[test]
    fn eviction_respects_pins() {
        let (_d, cas) = open_cas();
        let a = b"aaa".repeat(1000);
        let b = b"bbb".repeat(1000);
        let ha = Hash::of(&a);
        let hb = Hash::of(&b);
        cas.put(&ha, &a).unwrap();
        cas.put(&hb, &b).unwrap();
        cas.pin(&hb).unwrap();
        let evicted = cas.evict_to(0).unwrap();
        assert!(evicted.contains(&ha.hex()));
        assert!(!evicted.contains(&hb.hex()), "pinned chunk is never evicted");
        assert!(cas.contains(&hb));
        assert!(!cas.contains(&ha));
    }

    #[test]
    fn verify_sample_detects_tampering() {
        let (_d, cas) = open_cas();
        let bytes = b"tamper-me".to_vec();
        let h = Hash::of(&bytes);
        cas.put(&h, &bytes).unwrap();
        // tamper on disk
        let p = cas.root.join("data").join(h.shard()).join(h.hex());
        std::fs::write(&p, b"tampered").unwrap();
        let (_checked, bad) = cas.verify_sample(100).unwrap();
        assert_eq!(bad, vec![h.hex()]);
    }
}
