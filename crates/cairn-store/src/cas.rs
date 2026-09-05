//! Local content-addressed chunk store (SPEC §5.3 `blobs` table + on-disk CAS).
//!
//! Layout: `<root>/blobs/{hash[0:2]}/{hash}`. Writes are atomic (temp + fsync + rename).
//! Chunk identity is BLAKE3 of raw content — every write verifies before landing (I2).
//! `pinned` chunks are excluded from local eviction (ctl pin/unpin, SPEC §10/§11).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use cairn_core::clock::SystemClock;
use cairn_core::hash::Hash;
use cairn_core::{CairnError, ErrorKind};
use rusqlite::Connection;

/// Buffers at least this big verify BLAKE3 on the rayon CPU lane instead of
/// the calling async worker (ADR-0027). Below it the rayon round-trip costs
/// more than the hash itself — the PostHog "small work stays inline" rule.
const CPU_LANE_MIN_BYTES: usize = 256 * 1024;

/// Lock-free atime debounce (ADR-0027): BURST found every cached-open ended
/// in a serialized `UPDATE blobs SET atime` behind the one writer mutex —
/// 32 workers × repeated opens of the same 32 files is a write storm that
/// serves nobody (the value only feeds LRU eviction, which runs on the
/// 24h job cycle). A fixed table of 2048 slots keyed by hash prefix
/// collapses re-touches inside the window; collisions between different
/// hashes at worst skip one atime refresh, which eviction tolerance
/// already absorbs. Zero locks, zero allocation, fixed memory.
struct TouchFilter {
    slots: Vec<AtomicU64>,
}

const TOUCH_WINDOW_MS: u64 = 60_000;
const TOUCH_SLOTS: usize = 2048;

impl TouchFilter {
    fn new() -> Self {
        TouchFilter {
            slots: std::iter::repeat_with(|| AtomicU64::new(0))
                .take(TOUCH_SLOTS)
                .collect(),
        }
    }

    fn slot_of(h: &Hash) -> usize {
        let key = u64::from_le_bytes([
            h.0[0], h.0[1], h.0[2], h.0[3], h.0[4], h.0[5], h.0[6], h.0[7],
        ]);
        (key as usize) % TOUCH_SLOTS
    }

    /// Returns true when this touch should hit the writer (first touch, or
    /// outside the window). `now_ms` is passed in so tests can time-travel.
    fn mark(&self, h: &Hash, now_ms: i64) -> bool {
        let now = now_ms.max(0) as u64;
        let slot = &self.slots[Self::slot_of(h)];
        let last = slot.load(Ordering::Relaxed);
        if last != 0 && now.saturating_sub(last) < TOUCH_WINDOW_MS {
            return false;
        }
        slot.store(now, Ordering::Relaxed);
        true
    }
}

/// Local chunk CAS with SQLite index.
#[derive(Clone)]
pub struct Cas {
    root: PathBuf,
    db: Arc<std::sync::Mutex<Connection>>,
    touch_filter: Arc<TouchFilter>,
}

impl Cas {
    /// Open CAS rooted at `root` (typically `<store_root>/blobs` dir + shared db).
    pub fn open(root: &Path, db: Arc<std::sync::Mutex<Connection>>) -> Result<Self, CairnError> {
        std::fs::create_dir_all(root.join("data"))
            .map_err(|e| CairnError::new(ErrorKind::Io, format!("cas mkdir: {e}")))?;
        Ok(Cas {
            root: root.to_path_buf(),
            db,
            touch_filter: Arc::new(TouchFilter::new()),
        })
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
        // fsync before rename: the rename is the durability barrier (I2).
        // OPEN WITH WRITE ACCESS: Windows' FlushFileBuffers on a GENERIC_READ-
        // only handle fails with ERROR_ACCESS_DENIED (os error 5) — Linux
        // permits fsync on O_RDONLY, so only the windows build ever saw it
        // (round 13, caught live by the W1 matrix row on a windows runner).
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(&tmp)
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
        let bytes = std::fs::read(&path).map_err(|_| {
            CairnError::new(ErrorKind::NotFound, format!("chunk {h} not in local CAS"))
        })?;
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

    /// Read a chunk WITHOUT parking an async worker (ADR-0025 I/O lane).
    ///
    /// `tokio::fs::read` rides the runtime's async file machinery: on Linux with
    /// the io_uring driver armed (tokio `io-uring` feature, probed at runtime
    /// with automatic fallback) reads land on the ring; on every other platform
    /// or driver tokio's blocking pool serves them. Verification is identical
    /// to [`Cas::get`]; buffers of [`CPU_LANE_MIN_BYTES`] and up hash on the
    /// rayon CPU lane so BLAKE3 (fast as it is) can no longer park the I/O
    /// worker that is supposed to be serving the next request (ADR-0027 —
    /// the BURST lockstep finding).
    pub async fn get_async(&self, h: &Hash) -> Result<Vec<u8>, CairnError> {
        let path = self.path_for(h);
        let bytes = tokio::fs::read(&path).await.map_err(|_| {
            CairnError::new(ErrorKind::NotFound, format!("chunk {h} not in local CAS"))
        })?;
        let bytes = if bytes.len() >= CPU_LANE_MIN_BYTES {
            // ownership round-trips through the lane (hash + give the
            // buffer back) — zero copies, the I/O worker never hashes
            let (tx, rx) = tokio::sync::oneshot::channel();
            rayon::spawn(move || {
                let hash = Hash::of(&bytes);
                let _ = tx.send((bytes, hash));
            });
            let (bytes, actual) = rx.await.map_err(|_| {
                CairnError::new(ErrorKind::Io, "cpu verify lane panicked".to_string())
            })?;
            if actual != *h {
                return Err(CairnError::new(
                    ErrorKind::LocalCasCorrupt,
                    format!("local CAS corruption: {h}"),
                ));
            }
            bytes
        } else {
            // I2: verify on ingest, "free at 10+GB/s" (SPEC §9.2)
            let actual = Hash::of(&bytes);
            if actual != *h {
                return Err(CairnError::new(
                    ErrorKind::LocalCasCorrupt,
                    format!("local CAS corruption: {h}"),
                ));
            }
            bytes
        };
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
        db.execute(
            "UPDATE blobs SET pinned=0 WHERE hash=?1",
            rusqlite::params![h.hex()],
        )
        .map_err(|e| CairnError::new(ErrorKind::Io, format!("unpin: {e}")))?;
        Ok(())
    }

    /// Total local CAS bytes (rows, not on-disk du) — the eviction policy input.
    pub fn live_bytes(&self) -> Result<u64, CairnError> {
        let db = self.db.lock().expect("cas db poisoned");
        let v: i64 = db
            .query_row("SELECT COALESCE(SUM(size),0) FROM blobs", [], |r| r.get(0))
            .map_err(|e| CairnError::new(ErrorKind::Io, format!("live_bytes: {e}")))?;
        Ok(v as u64)
    }

    /// Storage stats for the dashboard (WO6-UI): object count, total bytes, and the
    /// pinned subset (eviction-exempt). Real aggregates over the blobs table — the
    /// dashboard renders these verbatim, no placeholders.
    pub fn blob_stats(&self) -> Result<(u64, u64, u64, u64), CairnError> {
        let db = self.db.lock().expect("cas db poisoned");
        let (count, total): (i64, i64) = db
            .query_row(
                "SELECT COUNT(*), COALESCE(SUM(size),0) FROM blobs",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(|e| CairnError::new(ErrorKind::Io, format!("blob_stats: {e}")))?;
        let (pinned_count, pinned_bytes): (i64, i64) = db
            .query_row(
                "SELECT COUNT(*), COALESCE(SUM(size),0) FROM blobs WHERE pinned=1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(|e| CairnError::new(ErrorKind::Io, format!("blob_stats: {e}")))?;
        Ok((
            count as u64,
            total as u64,
            pinned_count as u64,
            pinned_bytes as u64,
        ))
    }

    /// Enumerate every locally-owned chunk hash in hash order (deterministic).
    /// The swarm's HAVE bloom is built from this (ADR-0017 §6: the serving set
    /// is the blobs table, not the file rows).
    pub fn list_hashes(&self) -> Vec<Hash> {
        let Ok(db) = self.db.lock() else {
            return Vec::new(); // poisoned lock: advertise nothing, serve nothing
        };
        let Ok(mut stmt) = db.prepare("SELECT hash FROM blobs ORDER BY hash") else {
            return Vec::new();
        };
        stmt.query_map([], |r| r.get::<_, String>(0))
            .map(|rows| {
                rows.filter_map(|r| r.ok())
                    .filter_map(|hex| Hash::from_hex(&hex))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Local GC: evict least-recently-used unpinned chunks down to `target_bytes`.
    /// Returns evicted hashes. Pinned chunks are never touched (SPEC §10).
    pub fn evict_to(&self, target_bytes: u64) -> Result<Vec<String>, CairnError> {
        self.evict_to_policy(target_bytes, 0)
    }

    /// Policy-guarded eviction (WO6-2): like [`Cas::evict_to`], but chunks whose
    /// atime is younger than `min_age_secs` are PROTECTED — an actively-edited
    /// file's chunks are never reclaimed while everything else is old (the
    /// open-file protection the work order names; on Windows CfDehydratePlaceholder
    /// additionally fails for oplocked/open files at the OS layer).
    pub fn evict_to_policy(
        &self,
        target_bytes: u64,
        min_age_secs: i64,
    ) -> Result<Vec<String>, CairnError> {
        let db = self.db.lock().expect("cas db poisoned");
        // age cutoff: chunks with atime >= cutoff are PROTECTED (WO6-2 min-age guard)
        let cutoff = self.clock_now() - min_age_secs.saturating_mul(1000);
        let mut stmt = db
            .prepare(
                "SELECT hash, size, pinned FROM blobs
                 WHERE pinned=0 AND atime <= ?1
                 ORDER BY atime ASC",
            )
            .map_err(|e| CairnError::new(ErrorKind::Io, format!("evict q: {e}")))?;
        let rows: Vec<(String, i64)> = stmt
            .query_map([cutoff], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })
            .map_err(|e| CairnError::new(ErrorKind::Io, format!("evict map: {e}")))?
            .filter_map(|r| r.ok())
            .collect();
        drop(stmt);
        let mut live: u64 =
            db.query_row("SELECT COALESCE(SUM(size),0) FROM blobs", [], |r| {
                r.get::<_, i64>(0)
            })
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
        // ADR-0027 debounce: re-reads inside the window skip the serialized
        // writer round-trip entirely (the LRU only needs minute granularity)
        if !self.touch_filter.mark(h, self.clock_now()) {
            return Ok(());
        }
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
        let cas = Cas::open(dir.path(), Arc::new(std::sync::Mutex::new(conn))).unwrap();
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
        assert!(
            !evicted.contains(&hb.hex()),
            "pinned chunk is never evicted"
        );
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

    #[test]
    fn touch_filter_collapses_retouches_inside_the_window() {
        let f = TouchFilter::new();
        let h = Hash::of(b"debounce-me");
        // first touch writes
        assert!(f.mark(&h, 1_000));
        // re-reads inside the 60s window: no write, on any thread-clone
        assert!(!f.mark(&h, 1_500));
        assert!(!f.mark(&h, 59_999));
        // past the window it writes again
        assert!(f.mark(&h, 61_001));
        // and the window restarts from the new timestamp
        assert!(!f.mark(&h, 61_500));
    }

    #[tokio::test]
    async fn get_async_roundtrips_and_detects_corruption() {
        let (_d, cas) = open_cas();
        // small buffer: inline verify path
        let small = b"small chunk".to_vec();
        let hs = Hash::of(&small);
        cas.put(&hs, &small).unwrap();
        assert_eq!(cas.get_async(&hs).await.unwrap(), small);

        // big buffer: rayon CPU-lane verify path, byte-identical result
        let big = b"x".repeat(CPU_LANE_MIN_BYTES + 123);
        let hb = Hash::of(&big);
        cas.put(&hb, &big).unwrap();
        let back = cas.get_async(&hb).await.unwrap();
        assert_eq!(back.len(), big.len());
        assert_eq!(back, big);

        // corruption is still caught on the lane (I2 holds)
        let p = cas.root.join("data").join(hb.shard()).join(hb.hex());
        let mut tampered = big.clone();
        tampered[0] = b'y';
        std::fs::write(&p, &tampered).unwrap();
        let err = cas.get_async(&hb).await.unwrap_err();
        // LocalCasCorrupt flattens to the CHECKSUM_MISMATCH taxonomy code
        assert_eq!(err.code(), "CHECKSUM_MISMATCH");
    }
}
