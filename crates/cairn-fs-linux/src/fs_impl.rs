//! FUSE filesystem implementation (see crate docs).
#![cfg_attr(not(feature = "fuse"), allow(dead_code))]

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use cairn_core::hash::Hash;
use cairn_core::manifest::Manifest;
use cairn_core::{CairnError, ErrorKind};
use cairn_store::{Cas, HeaderCache, Store};
#[cfg(feature = "fuse")]
use fuser::{FileAttr, Filesystem, Request};

/// Hydration metrics collected through the REAL filesystem read path (punch #8: the
/// I1 metric must exist on Linux FUSE before Windows — same shape as the CfAPI
/// callback probe so both platforms report identically).
///
/// Buckets are log-scaled milliseconds; percentiles are computed from cumulative
/// bucket counts (bounded memory, O(1) record, good-enough tails for a 50 ms gate).
#[derive(Default)]
pub struct FsMetrics {
    pub reads_total: std::sync::atomic::AtomicU64,
    pub header_cache_hits: std::sync::atomic::AtomicU64,
    pub full_hydrations: std::sync::atomic::AtomicU64,
    pub bytes_served: std::sync::atomic::AtomicU64,
    first_byte_buckets: Mutex<[u64; BUCKETS.len() + 1]>,
    read_buckets: Mutex<[u64; BUCKETS.len() + 1]>,
}

/// Bucket UPPER bounds in milliseconds (log-scaled); the extra bucket is +inf.
const BUCKETS: [f64; 14] = [
    0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0, 2500.0,
];

fn bucket_index(ms: f64) -> usize {
    BUCKETS
        .iter()
        .position(|&b| ms <= b)
        .unwrap_or(BUCKETS.len())
}

fn percentile(buckets: &[u64; BUCKETS.len() + 1], p: f64) -> f64 {
    let total: u64 = buckets.iter().sum();
    if total == 0 {
        return 0.0;
    }
    let target = ((total as f64) * p).ceil() as u64;
    let mut cum = 0u64;
    for (i, c) in buckets.iter().enumerate() {
        cum += c;
        if cum >= target {
            return if i == 0 {
                BUCKETS[0] / 2.0
            } else if i < BUCKETS.len() {
                BUCKETS[i - 1] / 2.0 + BUCKETS[i] / 2.0
            } else {
                BUCKETS[BUCKETS.len() - 1] * 2.0
            };
        }
    }
    0.0
}

impl FsMetrics {
    fn record_read(&self, ms: f64, first_byte: bool) {
        use std::sync::atomic::Ordering::Relaxed;
        self.reads_total.fetch_add(1, Relaxed);
        let idx = bucket_index(ms);
        if first_byte {
            self.first_byte_buckets.lock().expect("metrics")[idx] += 1;
        }
        self.read_buckets.lock().expect("metrics")[idx] += 1;
    }

    fn record_bytes(&self, n: u64) {
        self.bytes_served
            .fetch_add(n, std::sync::atomic::Ordering::Relaxed);
    }

    fn record_hit(&self, hit: bool) {
        if hit {
            self.header_cache_hits
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        } else {
            self.full_hydrations
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Percentile snapshot for ctl/dashboard surfaces (same fields the Windows
    /// CfAPI probe reports: first-byte and read percentiles through the real path).
    pub fn snapshot(&self) -> FsMetricsSnapshot {
        let fb = self.first_byte_buckets.lock().expect("metrics");
        let rb = self.read_buckets.lock().expect("metrics");
        FsMetricsSnapshot {
            reads_total: self.reads_total.load(std::sync::atomic::Ordering::Relaxed),
            header_cache_hits: self
                .header_cache_hits
                .load(std::sync::atomic::Ordering::Relaxed),
            full_hydrations: self
                .full_hydrations
                .load(std::sync::atomic::Ordering::Relaxed),
            bytes_served: self.bytes_served.load(std::sync::atomic::Ordering::Relaxed),
            first_byte_p50_ms: percentile(&fb, 0.50),
            first_byte_p99_ms: percentile(&fb, 0.99),
            read_p50_ms: percentile(&rb, 0.50),
            read_p99_ms: percentile(&rb, 0.99),
        }
    }
}

/// Point-in-time hydration metrics (ctl/status surface; I1 gate = first_byte_p99 < 50ms).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FsMetricsSnapshot {
    pub reads_total: u64,
    pub header_cache_hits: u64,
    pub full_hydrations: u64,
    pub bytes_served: u64,
    pub first_byte_p50_ms: f64,
    pub first_byte_p99_ms: f64,
    pub read_p50_ms: f64,
    pub read_p99_ms: f64,
}

/// Backing view for the FUSE mount.
pub struct CairnFs {
    store: Store,
    cas: Cas,
    headers: HeaderCache,
    project_id: String,
    ttl: Duration,
    inodes: Mutex<InodeTable>,
    /// Hydration metrics through the real read path (I1 exists on Linux, punch #8).
    pub metrics: FsMetrics,
}

#[derive(Default)]
struct InodeTable {
    by_path: HashMap<String, u64>,
    by_ino: HashMap<u64, String>,
    next: u64,
}

impl InodeTable {
    fn alloc(&mut self, path: &str) -> u64 {
        if let Some(ino) = self.by_path.get(path) {
            return *ino;
        }
        self.next += 1;
        let ino = self.next;
        self.by_path.insert(path.to_string(), ino);
        self.by_ino.insert(ino, path.to_string());
        ino
    }
}

#[cfg(feature = "fuse")]
fn attr(ino: u64, size: u64, is_dir: bool, mtime: i64) -> FileAttr {
    let kind = if is_dir {
        fuser::FileType::Directory
    } else {
        fuser::FileType::RegularFile
    };
    FileAttr {
        ino,
        size,
        blocks: size.div_ceil(512),
        atime: std::time::UNIX_EPOCH + Duration::from_millis(mtime.max(0) as u64),
        mtime: std::time::UNIX_EPOCH + Duration::from_millis(mtime.max(0) as u64),
        ctime: std::time::UNIX_EPOCH + Duration::from_millis(mtime.max(0) as u64),
        crtime: std::time::UNIX_EPOCH + Duration::from_millis(mtime.max(0) as u64),
        kind,
        perm: if is_dir { 0o755 } else { 0o644 },
        nlink: 1,
        uid: 1000,
        gid: 1000,
        rdev: 0,
        blksize: 512,
        flags: 0,
    }
}

impl CairnFs {
    /// Build the filesystem view for a project.
    pub fn new(store: Store, cas: Cas, headers: HeaderCache, project_id: &str) -> Self {
        let mut inodes = InodeTable::default();
        inodes.alloc(""); // root
        for f in store.list_files(project_id) {
            inodes.alloc(&f.path);
        }
        CairnFs {
            store,
            cas,
            headers,
            project_id: project_id.to_string(),
            ttl: Duration::from_secs(1),
            inodes: Mutex::new(inodes),
            metrics: FsMetrics::default(),
        }
    }

    fn path_of(&self, ino: u64) -> Option<String> {
        self.inodes.lock().ok()?.by_ino.get(&ino).cloned()
    }

    /// Serve the file header (I1 gate): head bytes from the header cache. The measured
    /// latency is reported as `cairn_hydration_first_byte_ms` (<50ms cached, SPEC §2).
    pub fn serve_header(&self, path: &str) -> Result<(Vec<u8>, Duration), CairnError> {
        let f = self
            .store
            .get_file(&self.project_id, path)
            .ok_or_else(|| CairnError::new(ErrorKind::NotFound, path.to_string()))?;
        let Some(manifest_hex) = f.manifest_hash else {
            return Ok((Vec::new(), Duration::ZERO));
        };
        self.headers
            .serve_measured(&manifest_hex)
            .map(|(h, dt)| (h.head, dt))
    }

    /// Store-serve read entry point (WO6-5 burst bench, probes): FUSE-parity — same
    /// cache semantics and the SAME FsMetrics recording as the FUSE callback (punch #8:
    /// every read entry point lands in one metric). `offset==0` records first-byte.
    pub fn serve_read(&self, path: &str, offset: u64, size: usize) -> Result<Vec<u8>, CairnError> {
        let f = self
            .store
            .get_file(&self.project_id, path)
            .ok_or_else(|| CairnError::new(ErrorKind::NotFound, path.to_string()))?;
        let Some(manifest_hex) = f.manifest_hash else {
            return Ok(Vec::new());
        };
        self.read_range(&manifest_hex, offset, size)
    }

    /// Mount at `mountpoint` (blocking; requires /dev/fuse + the `fuse` feature, which needs
    /// libfuse3 headers at build time — enabled on FUSE-capable CI runners).
    #[cfg(feature = "fuse")]
    pub fn mount(self: Arc<Self>, mountpoint: &Path) -> Result<(), CairnError> {
        fuser::mount2(self, mountpoint, &[])
            .map_err(|e| CairnError::new(ErrorKind::Io, format!("fuse mount: {e}")))
    }

    fn read_range(
        &self,
        manifest_hex: &str,
        offset: u64,
        size: usize,
    ) -> Result<Vec<u8>, CairnError> {
        // I1 measured HERE (not just the FUSE callback) so every entry point — mount,
        // tests, store-serve — lands in the same metric (punch #8)
        let t0 = std::time::Instant::now();
        let first_byte = offset == 0;
        let out = self.read_range_inner(manifest_hex, offset, size);
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        self.metrics.record_read(ms, first_byte);
        if let Ok(b) = &out {
            self.metrics.record_bytes(b.len() as u64);
        }
        out
    }

    fn read_range_inner(
        &self,
        manifest_hex: &str,
        offset: u64,
        size: usize,
    ) -> Result<Vec<u8>, CairnError> {
        // header cache fast path (I1): head/tail served from local SQLite
        if let Ok(cached) = self.headers.serve(manifest_hex) {
            if offset + size as u64 <= cached.head.len() as u64 {
                self.metrics.record_hit(true);
                return Ok(cached.head[offset as usize..(offset as usize + size)].to_vec());
            }
        }
        self.metrics.record_hit(false);
        let bytes = self.read_whole_verified(manifest_hex)?;
        let start = offset as usize;
        if start >= bytes.len() {
            return Ok(Vec::new());
        }
        let end = (start + size).min(bytes.len());
        Ok(bytes[start..end].to_vec())
    }

    fn read_whole_verified(&self, manifest_hex: &str) -> Result<Vec<u8>, CairnError> {
        // the engine mirrors manifest objects into the local CAS (content-addressed); every
        // chunk is verified on ingest (I2: never materialize corrupt files)
        let mh = Hash::from_hex(manifest_hex)
            .ok_or_else(|| CairnError::new(ErrorKind::ManifestFormat, "bad manifest hash"))?;
        let manifest_bytes = self.cas.get(&mh)?;
        let m = Manifest::parse(&manifest_bytes)?;
        let mut out = Vec::with_capacity(m.total_len() as usize);
        for e in m.flatten() {
            let raw = self.cas.get(&e.chunk_hash)?;
            out.extend_from_slice(&raw);
        }
        Ok(out)
    }
}

#[cfg(feature = "fuse")]
impl Filesystem for CairnFs {
    fn lookup(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: fuser::ReplyEntry) {
        let parent_path = self.path_of(parent).unwrap_or_default();
        let child = if parent_path.is_empty() {
            name.to_string_lossy().into_owned()
        } else {
            format!("{parent_path}/{}", name.to_string_lossy())
        };
        let Some(f) = self.store.get_file(&self.project_id, &child) else {
            reply.error(libc::ENOENT);
            return;
        };
        let mut inodes = self.inodes.lock().expect("inode table");
        let ino = inodes.alloc(&child);
        reply.entry(&self.ttl, &attr(ino, f.size, false, f.mtime), 0);
    }

    fn getattr(&mut self, _req: &Request, ino: u64, reply: fuser::ReplyAttr) {
        if ino == fuser::FUSE_ROOT_ID {
            reply.attr(&self.ttl, &attr(ino, 0, true, 0));
            return;
        }
        let Some(path) = self.path_of(ino) else {
            reply.error(libc::ENOENT);
            return;
        };
        match self.store.get_file(&self.project_id, &path) {
            Some(f) => reply.attr(&self.ttl, &attr(ino, f.size, f.mode == "dir", f.mtime)),
            None => reply.error(libc::ENOENT),
        }
    }

    fn readdir(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: fuser::ReplyDirectory,
    ) {
        let prefix = self.path_of(ino).unwrap_or_default();
        let mut inodes = self.inodes.lock().expect("inode table");
        let mut entries: Vec<(u64, String)> = vec![
            (fuser::FUSE_ROOT_ID, ".".into()),
            (fuser::FUSE_ROOT_ID, "..".into()),
        ];
        for f in self.store.list_files(&self.project_id) {
            let is_child = if prefix.is_empty() {
                !f.path.contains('/')
            } else {
                f.path.starts_with(&format!("{prefix}/"))
                    && !f.path[prefix.len() + 1..].contains('/')
            };
            if is_child {
                let name = f.path.rsplit('/').next().unwrap_or(&f.path).to_string();
                let cino = inodes.alloc(&f.path);
                entries.push((cino, name));
            }
        }
        drop(inodes);
        for (idx, (cino, name)) in entries.into_iter().enumerate().skip(offset.max(0) as usize) {
            if reply.add(cino, (idx + 1) as i64, fuser::FileType::RegularFile, name) {
                break;
            }
        }
        reply.ok();
    }

    fn read(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: fuser::ReplyData,
    ) {
        let Some(path) = self.path_of(ino) else {
            reply.error(libc::ENOENT);
            return;
        };
        let Some(f) = self.store.get_file(&self.project_id, &path) else {
            reply.error(libc::ENOENT);
            return;
        };
        let Some(manifest_hex) = f.manifest_hash else {
            reply.error(libc::ENOENT);
            return;
        };
        let t0 = std::time::Instant::now();
        let bytes = self.read_range(&manifest_hex, offset.max(0) as u64, size as usize);
        tracing::trace!(
            %path,
            ms = t0.elapsed().as_secs_f64() * 1000.0,
            "read (cairn_hydration_first_byte_ms probe)"
        );
        match bytes {
            Ok(b) => reply.data(&b),
            Err(_) => reply.error(libc::EIO),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cairn_core::clock::WallClock;
    use cairn_store::{HeaderCache, Store};
    use std::sync::Arc;

    fn setup_with_file(content: &[u8]) -> (tempfile::TempDir, CairnFs, String) {
        use cairn_core::chunker::StreamHash;
        use cairn_core::manifest::{Compression, Manifest, ManifestEntry};
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("store").as_path(), Arc::new(WallClock)).unwrap();
        let conn = store.conn_handle();
        let cas = Cas::open(&dir.path().join("blobs"), conn.clone()).unwrap();
        let headers = HeaderCache::new(conn);

        // chunk + CAS the content, build a manifest, cache the header
        let sh = StreamHash::compute(content);
        for (s, h) in sh.spans.iter().zip(sh.chunk_hashes.iter()) {
            let off = s.offset as usize;
            let end = off + s.len as usize;
            cas.put(h, &content[off..end]).unwrap();
        }
        let entries: Vec<ManifestEntry> = sh
            .spans
            .iter()
            .zip(sh.chunk_hashes.iter())
            .map(|(s, h)| ManifestEntry {
                offset: s.offset,
                len: s.len,
                chunk_hash: *h,
            })
            .collect();
        let m = Manifest::build(entries, Compression::None, None);
        let (mh, mb) = m.serialize();
        // the engine mirrors the manifest object into the local CAS
        cas.put(&mh, &mb).unwrap();
        // the engine caches the file header (head 2MB + tail 1MB) after sync (SPEC §5.1)
        let head_len = content.len().min(cairn_core::HEADER_HEAD_BYTES);
        let head = &content[..head_len];
        let tail = if content.len() > cairn_core::HEADER_HEAD_BYTES {
            Some(&content[content.len() - cairn_core::HEADER_TAIL_BYTES..])
        } else {
            None
        };
        headers.put(&mh.hex(), head, tail).unwrap();
        store
            .put_file(&cairn_store::FileRow {
                path: "A001.mov".into(),
                project_id: "p1".into(),
                manifest_hash: Some(mh.hex()),
                size: content.len() as u64,
                mode: "file".into(),
                mtime: 0,
                local_state: "synced".into(),
            })
            .unwrap();
        let fs = CairnFs::new(store, cas, headers, "p1");
        (dir, fs, mh.hex())
    }

    /// I1 gate: cached header serve <50ms (SPEC §2).
    #[test]
    fn i1_header_serve_under_50ms() {
        let content: Vec<u8> = (0..8 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
        let (_d, fs, _mh) = setup_with_file(&content);
        for _ in 0..10 {
            let (head, dt) = fs.serve_header("A001.mov").unwrap();
            assert_eq!(head.len(), cairn_core::HEADER_HEAD_BYTES);
            assert_eq!(head, &content[..cairn_core::HEADER_HEAD_BYTES]);
            assert!(
                dt.as_secs_f64() * 1000.0 < cairn_core::I1_TARGET_CACHED_MS,
                "I1 violated: {dt:?}"
            );
        }
    }

    /// Hydration reads are byte-identical with per-chunk verification (I2).
    #[test]
    fn hydration_reads_verified() {
        let content: Vec<u8> = (0..9 * 1024 * 1024)
            .map(|i| ((i * 7) % 253) as u8)
            .collect();
        let (_d, fs, mh) = setup_with_file(&content);
        let read = fs.read_range(&mh, 0, 5 * 1024 * 1024).unwrap();
        assert_eq!(&read[..], &content[..5 * 1024 * 1024]);
        let tail = fs.read_range(&mh, 8 * 1024 * 1024, 1024).unwrap();
        assert_eq!(tail, content[8 * 1024 * 1024..8 * 1024 * 1024 + 1024]);
        // out-of-range read is empty
        let past = fs
            .read_range(&mh, u64::from(content.len() as u32), 16)
            .unwrap();
        assert!(past.is_empty());
    }

    /// I1 THROUGH the filesystem read path (punch #8): the same invariant the Windows
    /// CfAPI probe measures — a cold-ish open (first read, offset 0) against a warm
    /// header cache must land under the 50ms gate, and the metric must EXPOSE it.
    #[test]
    fn i1_through_read_path_measured_by_metrics() {
        let content: Vec<u8> = (0..8 * 1024 * 1024).map(|i| (i % 249) as u8).collect();
        let (_d, fs, mh) = setup_with_file(&content);
        // first 2 MiB read = the editor-open probe (offset 0)
        let head = fs.read_range(&mh, 0, 2 * 1024 * 1024).unwrap();
        assert_eq!(head.len(), 2 * 1024 * 1024);
        // a mid-file read outside the header cache forces a full hydration (counted)
        let mid = fs.read_range(&mh, 4 * 1024 * 1024, 1024).unwrap();
        assert_eq!(mid.len(), 1024);

        let snap = fs.metrics.snapshot();
        assert_eq!(snap.reads_total, 2, "both reads recorded");
        assert_eq!(snap.header_cache_hits, 1, "head read served from I1 cache");
        assert_eq!(snap.full_hydrations, 1, "mid read was a full hydration");
        assert!(
            snap.first_byte_p99_ms > 0.0,
            "first-byte samples must be recorded (metric exists, not vacuous)"
        );
        assert!(
            snap.first_byte_p99_ms < cairn_core::I1_TARGET_CACHED_MS,
            "I1 through the read path: p99 {:.3}ms >= gate",
            snap.first_byte_p99_ms
        );
        assert_eq!(snap.bytes_served, head.len() as u64 + mid.len() as u64);
    }

    /// Percentile math: log-scaled buckets must bracket known latencies.
    #[test]
    fn metrics_percentiles_bracket_latency() {
        let m = FsMetrics::default();
        for _ in 0..95 {
            m.record_read(0.2, false); // bucket 0.25
        }
        for _ in 0..5 {
            m.record_read(8.0, false); // bucket 10
        }
        let snap = m.snapshot();
        assert!(
            snap.read_p50_ms <= 0.5 && snap.read_p50_ms >= 0.1,
            "p50 should sit in the 0.25 bucket, got {}",
            snap.read_p50_ms
        );
        assert!(
            snap.read_p99_ms >= 5.0 && snap.read_p99_ms <= 10.0,
            "p99 should bracket the 8ms outlier, got {}",
            snap.read_p99_ms
        );
        assert_eq!(m.snapshot().reads_total, 100);
    }

    /// Inode allocation is stable per path.
    #[test]
    fn inode_allocation_is_stable() {
        let mut t = InodeTable::default();
        let a = t.alloc("a.mov");
        assert_eq!(a, t.alloc("a.mov"));
        assert_ne!(a, t.alloc("b.mov"));
    }
}
