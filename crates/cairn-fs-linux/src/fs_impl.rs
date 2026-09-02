//! FUSE filesystem implementation (see crate docs).
#![cfg_attr(not(feature = "fuse"), allow(dead_code))]

use std::collections::{HashMap, HashSet};
#[cfg(feature = "fuse")]
use std::ffi::OsStr;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::FileExt;
#[cfg(feature = "fuse")]
use std::path::Path;
use std::path::PathBuf;
#[cfg(feature = "fuse")]
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use cairn_core::chunker::FastCdc;
use cairn_core::hash::Hash;
use cairn_core::manifest::{Compression, Manifest, ManifestEntry};
use cairn_core::normalize::Transform;
use cairn_core::{CairnError, ErrorKind};
use cairn_store::{Cas, HeaderCache, Store};
#[cfg(feature = "fuse")]
use fuser::{FileAttr, Filesystem, Request};

/// Phase-3 lease TTL for editor write-opens (ADR-0014): ephemeral, pid-bound, renewed
/// by the mount heartbeat. A crashed editor's row is reaped on the NEXT acquire (pid
/// probe), so conflicts self-heal in seconds — the routine is automatic recovery, the
/// admin override (`cairn ctl lease drop`) is the escape hatch, not the daily path.
pub const LEASE_TTL_MS: i64 = 15_000;
/// Heartbeat cadence: renew every open write's lease well inside the 15s TTL.
pub const HEARTBEAT_SECS: u64 = 5;
/// Streaming window for release-commit ingest (bounded RAM: chunk boundaries are
/// computed incrementally by `FastCdc::push`, identical to whole-buffer `cut`).
const WRITE_WINDOW: usize = 4 << 20;
/// Temp spool directory (inside the store root): write-back staging before commit.
const STAGING_DIR: &str = "staging";

/// Content-derived lease token: deterministic per (path, pid, acquire-time) so
/// heartbeat renewals keep the SAME token stable (fencing semantics), while a fresh
/// acquire after a dead-owner reap gets a NEW token (stale fenced writers lose).
fn lease_token(path: &str, pid: u32, now_ms: i64) -> u64 {
    let h = Hash::of(format!("lease\0{path}\0{pid}\0{now_ms}").as_bytes());
    let hex = h.hex();
    u64::from_str_radix(&hex[..16], 16).unwrap_or(0)
}

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
    /// Files open for write through the mount (write-back spool).
    writes: Mutex<WriteTable>,
    /// This device's id (lease rows record the acquiring device, ADR-0014 Phase 3).
    device_id: String,
    /// Mount-resolved native-collab layout (marker mode + passthrough directories).
    native: Mutex<NativeLayout>,
    /// Hydration metrics through the real read path (I1 exists on Linux, punch #8).
    pub metrics: FsMetrics,
    /// Shutdown flag: the heartbeat loop exits (within one beat) once this is set,
    /// so the daemon actually TERMINATES after unmount — the live-mount run caught
    /// `heartbeat.join()` blocking forever without it (process survived `fusermount -u`).
    stopped: std::sync::atomic::AtomicBool,
}

/// One file open for write: spooled to a temp file inside the store's staging dir,
/// committed on the LAST release (chunk → CAS → manifest → file row → header cache).
struct OpenWrite {
    path: String,
    temp: std::fs::File,
    temp_path: PathBuf,
    /// Lease-owning editor pid (recorded at first open; refreshed on re-open).
    pid: u32,
    /// Open write handles referencing this spool (commit on the last release).
    refs: usize,
    /// True once existing store content has been copied into the spool (lazy seed).
    seeded: bool,
    /// O_TRUNC open: existing content is DISCARDED — the lazy seed must not run.
    no_seed: bool,
    /// Editor-provided mtime (setattr), else commit time.
    mtime_ms: Option<i64>,
}

#[derive(Default)]
struct WriteTable {
    by_fh: HashMap<u64, OpenWrite>,
    next_fh: u64,
}

impl WriteTable {
    fn alloc(&mut self) -> u64 {
        self.next_fh += 1;
        self.next_fh
    }

    fn fh_for_path(&self, path: &str) -> Option<u64> {
        self.by_fh
            .iter()
            .find(|(_, w)| w.path == path)
            .map(|(fh, _)| *fh)
    }
}

/// Mount-side native-collab layout (ADR-0014): FUSE serves a virtual tree — there is
/// no workspace root to `read_dir` — so the sibling-`.prodsys` rule is resolved against
/// the SYNCED path set: every ancestor directory of any synced path containing a
/// `.prodsys` component is passthrough-owned by Premiere (exactly the paths a real
/// Productions layout produces), and the marker (`.cairn-native-collab`) is read as a
/// synced project file rather than from disk.
#[derive(Default)]
struct NativeLayout {
    marker_mode: Option<String>,
    passthrough_dirs: HashSet<String>,
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

#[cfg(feature = "fuse")]
fn time_or_now_ms(t: fuser::TimeOrNow) -> i64 {
    let now = std::time::SystemTime::now();
    let sys = match t {
        fuser::TimeOrNow::SpecificTime(s) => s,
        fuser::TimeOrNow::Now => now,
    };
    sys.duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Newtype enabling `Filesystem` for the Arc-shared mount view (orphan rule): Deref
/// forwards to `CairnFs`; every callback only touches Mutex-protected state.
#[cfg(feature = "fuse")]
pub struct SharedFs(pub Arc<CairnFs>);

#[cfg(feature = "fuse")]
impl std::ops::Deref for SharedFs {
    type Target = CairnFs;
    fn deref(&self) -> &CairnFs {
        &self.0
    }
}

impl CairnFs {
    /// Build the filesystem view for a project.
    pub fn new(store: Store, cas: Cas, headers: HeaderCache, project_id: &str) -> Self {
        Self::with_device(store, cas, headers, project_id, "fuse-mount")
    }

    /// Like [`CairnFs::new`] with an explicit device id (lease rows / diagnostics).
    pub fn with_device(
        store: Store,
        cas: Cas,
        headers: HeaderCache,
        project_id: &str,
        device_id: &str,
    ) -> Self {
        let mut inodes = InodeTable::default();
        inodes.alloc(""); // root
        for f in store.list_files(project_id) {
            inodes.alloc(&f.path);
        }
        let fs = CairnFs {
            store,
            cas,
            headers,
            project_id: project_id.to_string(),
            ttl: Duration::from_secs(1),
            inodes: Mutex::new(inodes),
            writes: Mutex::new(WriteTable::default()),
            device_id: device_id.to_string(),
            native: Mutex::new(NativeLayout::default()),
            metrics: FsMetrics::default(),
            stopped: std::sync::atomic::AtomicBool::new(false),
        };
        fs.refresh_native_layout();
        fs
    }

    fn now_ms(&self) -> i64 {
        self.store.clock().now_millis()
    }

    // === Native collaboration (ADR-0014) ============================================

    /// Re-derive the native-collab layout from the synced path set: marker mode from
    /// the `.cairn-native-collab` project file (content via the read path) and the
    /// passthrough directory set from `.prodsys` path components. Called at mount and
    /// whenever the layout could have changed (create/unlink of the marker or prodsys
    /// files). Cheap: O(files in project).
    pub fn refresh_native_layout(&self) {
        let mut layout = NativeLayout::default();
        let files = self.store.list_files(&self.project_id);
        for f in &files {
            let comps: Vec<&str> = f.path.split('/').collect();
            if comps.iter().any(|c| c.ends_with(".prodsys")) {
                // every ancestor dir of a prodsys-owned path is Premiere-owned
                for i in 1..comps.len() {
                    layout.passthrough_dirs.insert(comps[..i].join("/"));
                }
            }
        }
        // marker content via the normal read path (no lease involvement: reads never
        // lease) — bounded: markers are tiny; cap at 64KiB
        if let Some(row) = self
            .store
            .get_file(&self.project_id, cairn_sync::native_collab::MARKER_FILE)
        {
            if let Ok(bytes) = self.serve_read(cairn_sync::native_collab::MARKER_FILE, 0, 65_536) {
                let _ = row;
                layout.marker_mode = Some(String::from_utf8_lossy(&bytes).trim().to_string());
            }
        }
        *self.native.lock().expect("native layout") = layout;
    }

    /// Arbitration owner for a virtual path (mount semantics — `detect_pure` plus the
    /// sibling-`.prodsys` rule resolved against the synced path set).
    fn native_for(&self, rel_path: &str) -> cairn_sync::native_collab::NativeCollab {
        let layout = self.native.lock().expect("native layout");
        // 1. own-path component rule
        if rel_path.split(['/', '\\']).any(|c| c.ends_with(".prodsys")) {
            return cairn_sync::native_collab::NativeCollab::PremiereProductions;
        }
        // 2. sibling rule: an ancestor dir that is Premiere-owned (derived from any
        //    synced path carrying a `.prodsys` component under it)
        let comps: Vec<&str> = rel_path.split('/').collect();
        for i in 1..comps.len() {
            if layout.passthrough_dirs.contains(&comps[..i].join("/")) {
                return cairn_sync::native_collab::NativeCollab::PremiereProductions;
            }
        }
        // 3. operator-declared marker
        cairn_sync::native_collab::detect_pure(rel_path, layout.marker_mode.as_deref())
    }

    /// ADR-0014 Phase 2: the lease scope for a path — the longest declared
    /// `.cairn-domains` root containing it (one pen per subproject state boundary),
    /// or the path itself. The domains file is an ORDINARY synced project file, so
    /// this is re-read per decision: config propagates through the sync engine with
    /// no wire/server change, and a missing/half-baked file degrades to per-file.
    fn lease_scope(&self, rel_path: &str) -> String {
        // The file lives in the SYNCED STATE: authored through any device's mount
        // it lands in the store (a virtual FUSE mount has no on-disk project tree —
        // the live-mount run caught the disk-only read missing it). Resolve from
        // the store first, then fall back to a .cairn-domains placed at the store
        // root (manual placement / the unit-test contract).
        if let Some(f) = self.store.get_file(&self.project_id, ".cairn-domains") {
            if let Some(mh) = f.manifest_hash {
                let size = (f.size as usize).min(1 << 20);
                if let Ok(bytes) = self.read_ranged_verified(&mh, 0, size) {
                    if let Ok(text) = String::from_utf8(bytes) {
                        return cairn_sync::domains::Domains::parse(&text).scope_for(rel_path);
                    }
                }
            }
        }
        cairn_sync::domains::resolve_from_dir(self.store.root(), rel_path)
    }

    // === Write-back path (leases + spool + commit) ==================================

    /// Reap lease rows owned by dead processes (ADR-0014 Phase 3): a crashed editor's
    /// file frees within seconds — the NEXT writer's acquire repairs the table without
    /// human help.
    fn reap_dead_leases(&self) {
        for row in self.store.list_leases_pid() {
            if let Some(p) = row.pid {
                if p > 0 && !cairn_store::db::process_alive(p) {
                    let _ = self.store.drop_lease(&row.path);
                }
            }
        }
    }

    /// Open `path` for write (FUSE create / write-open). Acquires the pid-bound lease
    /// (unless the path is native-passthrough), spools to a staging temp file, and
    /// returns an fh. Existing content is seeded LAZILY on first positioned write, so
    /// the common full-rewrite (`write` at offset 0) pays no copy at all.
    pub fn open_write(&self, path: &str, pid: u32) -> Result<u64, i32> {
        self.open_write_opts(path, pid, false, false)
    }

    /// Open for write with create/truncate flags (FUSE create, O_TRUNC opens).
    pub fn open_write_opts(
        &self,
        path: &str,
        pid: u32,
        create: bool,
        truncate: bool,
    ) -> Result<u64, i32> {
        let existing = self.store.get_file(&self.project_id, path);
        if !create && existing.is_none() {
            let fresh = {
                let writes = self.writes.lock().expect("write table");
                writes.fh_for_path(path).is_some()
            };
            if !fresh {
                return Err(libc::ENOENT);
            }
        }
        let native = self.native_for(path);

        let mut writes = self.writes.lock().expect("write table");
        // Re-open of an already-spooled path: refresh lease, bump refcount.
        if let Some(first) = writes.fh_for_path(path) {
            let temp_path = writes.by_fh[&first].temp_path.clone();
            if !native.is_passthrough() {
                let scope = self.lease_scope(path);
                let token = self
                    .store
                    .get_lease(&scope)
                    .map_or_else(|| lease_token(&scope, pid, self.now_ms()), |(t, _)| t);
                self.store
                    .put_lease_pid(
                        &scope,
                        token,
                        self.now_ms() + LEASE_TTL_MS,
                        Some(i64::from(pid)),
                        Some(&self.project_id),
                        Some(&self.device_id),
                    )
                    .map_err(|e| {
                        eprintln!("cairn-fs-linux: re-open put_lease_pid({scope}) failed: {e}");
                        libc::EIO
                    })?;
            }
            let fh = writes.alloc();
            let temp = std::fs::OpenOptions::new()
                .append(true)
                .open(&temp_path)
                .map_err(|e| {
                    eprintln!(
                        "cairn-fs-linux: re-open spool {} failed: {e}",
                        temp_path.display()
                    );
                    libc::EIO
                })?;
            writes.by_fh.insert(
                fh,
                OpenWrite {
                    path: path.to_string(),
                    temp,
                    temp_path,
                    pid,
                    refs: 1,
                    seeded: true,
                    no_seed: false,
                    mtime_ms: None,
                },
            );
            return Ok(fh);
        }

        // First handle on this path: lease acquire (Phase 3) unless passthrough.
        // Phase 2: the row lives at the DOMAIN scope when the file falls under a
        // declared `.cairn-domains` root — a second file in the same domain conflicts
        // (one state boundary, one pen); other domains and unscoped files proceed.
        if !native.is_passthrough() {
            self.reap_dead_leases();
            let scope = self.lease_scope(path);
            if let Some(row) = self
                .store
                .list_leases_pid()
                .into_iter()
                .find(|r| r.path == scope)
            {
                let foreign_alive = matches!(row.pid, Some(p) if p > 0 && p != i64::from(pid) && cairn_store::db::process_alive(p));
                if row.expires_at > self.now_ms() && foreign_alive {
                    tracing::warn!(
                        %path,
                        scope = %scope,
                        owner_pid = row.pid.unwrap_or_default(),
                        "lease held by a live editor — EBUSY (admin override: cairn ctl lease drop)"
                    );
                    return Err(libc::EBUSY);
                }
            }
            let token = lease_token(&scope, pid, self.now_ms());
            self.store
                .put_lease_pid(
                    &scope,
                    token,
                    self.now_ms() + LEASE_TTL_MS,
                    Some(i64::from(pid)),
                    Some(&self.project_id),
                    Some(&self.device_id),
                )
                .map_err(|e| {
                    eprintln!("cairn-fs-linux: put_lease_pid({scope}) failed: {e}");
                    libc::EIO
                })?;
        }

        // Spool temp (create staging dir once).
        let staging = self.store.root().join(STAGING_DIR);
        let _ = std::fs::create_dir_all(&staging);
        let temp_path = staging.join(format!(
            "w{}-{}-{}",
            self.now_ms(),
            pid,
            path.replace('/', "_")
        ));
        let temp = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&temp_path)
            .map_err(|e| {
                eprintln!(
                    "cairn-fs-linux: spool open {} failed: {e}",
                    temp_path.display()
                );
                libc::EIO
            })?;
        let fh = writes.alloc();
        writes.by_fh.insert(
            fh,
            OpenWrite {
                path: path.to_string(),
                temp,
                temp_path,
                pid,
                refs: 1,
                seeded: false,
                no_seed: truncate,
                mtime_ms: None,
            },
        );
        Ok(fh)
    }

    /// Positional write into the spool (FUSE write). Seeds existing content lazily:
    /// a write at offset X first copies [0, X) from the store — a full rewrite from 0
    /// copies nothing, an append at EOF copies the file once, an in-place patch copies
    /// only the prefix before the patch point.
    pub fn write_fh(&self, fh: u64, offset: i64, data: &[u8]) -> Result<usize, i32> {
        if offset < 0 {
            return Err(libc::EINVAL);
        }
        let mut writes = self.writes.lock().expect("write table");
        let w = writes.by_fh.get_mut(&fh).ok_or(libc::EBADF)?;
        let offset = offset as u64;
        // in-place patch (read-modify-write): seed [0, offset) BEFORE the write and
        // [end_of_patch, size) AFTER it — the spool ends up byte-exact for the whole
        // file, the patch range excluded (it carries the new bytes)
        let existing_size = if !w.seeded && !w.no_seed {
            self.store
                .get_file(&self.project_id, &w.path)
                .map(|f| f.size)
        } else {
            None
        };
        if !w.seeded && !w.no_seed && offset > 0 {
            self.seed_range_locked(w, 0, offset)?;
        }
        w.temp.write_all_at(data, offset).map_err(|e| {
            eprintln!("cairn-fs-linux: spool write fh {fh} @ {offset} failed: {e}");
            libc::EIO
        })?;
        if let Some(sz) = existing_size {
            let end = offset + data.len() as u64;
            if end < sz {
                self.seed_range_locked(w, end, sz)?;
            }
        }
        w.seeded = true;
        Ok(data.len())
    }

    /// Copy [from, to) of the CURRENT store content into the spool at the same
    /// offsets (ranged verified reads; bounded RAM — chunk window by chunk window).
    fn seed_range_locked(&self, w: &mut OpenWrite, from: u64, to: u64) -> Result<(), i32> {
        let Some(f) = self.store.get_file(&self.project_id, &w.path) else {
            return Ok(()); // new file — nothing to seed
        };
        let Some(mh) = f.manifest_hash else {
            return Ok(()); // zero-byte row — nothing to seed
        };
        let end = to.min(f.size);
        let mut copied = from.min(end);
        let buf = vec![0u8; WRITE_WINDOW.min(end as usize).max(1)];
        while copied < end {
            let want = (end - copied).min(buf.len() as u64) as usize;
            let chunk = self.read_ranged_verified(&mh, copied, want).map_err(|e| {
                eprintln!("cairn-fs-linux: ranged read {mh} @ {copied} failed: {e:?}");
                libc::EIO
            })?;
            w.temp.write_all_at(&chunk, copied).map_err(|e| {
                eprintln!("cairn-fs-linux: seed write {copied} failed: {e}");
                libc::EIO
            })?;
            copied += chunk.len() as u64;
        }
        Ok(())
    }

    /// FUSE flush: advisory — data is durable in the spool; the commit lands on the
    /// last release. Editors calling fsync get a real spool fsync.
    pub fn fsync_fh(&self, fh: u64, datasync: bool) -> Result<(), i32> {
        let writes = self.writes.lock().expect("write table");
        let w = writes.by_fh.get(&fh).ok_or(libc::EBADF)?;
        if datasync {
            w.temp.sync_data().map_err(|_| libc::EIO)
        } else {
            w.temp.sync_all().map_err(|_| libc::EIO)
        }
    }

    /// FUSE release (close of one handle): commit on the LAST close of the path.
    pub fn release_fh(&self, fh: u64) -> Result<(), i32> {
        let w = {
            let mut writes = self.writes.lock().expect("write table");
            let w = writes.by_fh.get_mut(&fh).ok_or(libc::EBADF)?;
            w.refs -= 1;
            if w.refs > 0 {
                return Ok(());
            }
            writes.by_fh.remove(&fh).expect("just checked")
        };
        let res = self.commit_spool(&w);
        if res.is_err() {
            // keep the spool for diagnosis; drop the lease so the file is not stuck
            let _ = self.store.drop_lease(&self.lease_scope(&w.path));
            tracing::error!(path = %w.path, "write-back commit FAILED — spool kept");
        }
        res
    }

    /// Streaming ingest of a spooled write: chunk (default profile, identical cut
    /// points to the engine's `StreamHash::compute`) → CAS (raw, verified on put) →
    /// manifest tree (children FIRST — fanout safety) → file row (dirty: the sync
    /// engine's scan/outbox owns the push) → header-cache refresh (I1 stays warm).
    fn commit_spool(&self, w: &OpenWrite) -> Result<(), i32> {
        self.store
            .set_file_state(&self.project_id, &w.path, "hashing")
            .map_err(|_| libc::EIO)?;
        let mut reader = std::fs::File::open(&w.temp_path).map_err(|_| libc::EIO)?;
        let mut chunker = FastCdc::default();
        let mut spans = Vec::new();
        let mut win = vec![0u8; WRITE_WINDOW];
        loop {
            let n = reader.read(&mut win).map_err(|_| libc::EIO)?;
            if n == 0 {
                break;
            }
            chunker.push(&win[..n], &mut spans);
        }
        chunker.finish(&mut spans);

        let mut entries = Vec::with_capacity(spans.len());
        for s in &spans {
            reader
                .seek(SeekFrom::Start(s.offset))
                .map_err(|_| libc::EIO)?;
            let mut raw = vec![0u8; s.len as usize];
            reader.read_exact(&mut raw).map_err(|_| libc::EIO)?;
            let h = Hash::of(&raw);
            self.cas.put(&h, &raw).map_err(|_| libc::EIO)?;
            entries.push(ManifestEntry {
                offset: s.offset,
                len: s.len,
                chunk_hash: h,
            });
        }
        // engine parity: Zstd3 policy, no dict, no container transform
        let built =
            Manifest::build_tree_with_transform(entries, Compression::Zstd3, None, Transform::None);
        // children first (leaf-first): a crash between the two leaves unreferenced
        // children that GC reclaims — never a dangling parent (review round)
        for (child_hash, child_bytes) in &built.child_objects {
            self.cas
                .put(child_hash, child_bytes)
                .map_err(|_| libc::EIO)?;
        }
        let manifest = built.manifest;
        let (manifest_hash, manifest_bytes) = manifest.serialize();
        self.cas
            .put(&manifest_hash, &manifest_bytes)
            .map_err(|_| libc::EIO)?;

        let size = std::fs::metadata(&w.temp_path)
            .map(|m| m.len())
            .unwrap_or_else(|_| spans.iter().map(|s| u64::from(s.len)).sum());
        self.store
            .put_file(&cairn_store::FileRow {
                path: w.path.clone(),
                project_id: self.project_id.clone(),
                manifest_hash: Some(manifest_hash.hex()),
                size,
                mode: "file".into(),
                mtime: w.mtime_ms.unwrap_or_else(|| self.now_ms()),
                local_state: "dirty".into(),
            })
            .map_err(|_| libc::EIO)?;

        // header cache refresh: the NEXT editor open serves fresh bytes <50ms (I1)
        let head_len = size.min(cairn_core::HEADER_HEAD_BYTES as u64) as usize;
        reader.seek(SeekFrom::Start(0)).map_err(|_| libc::EIO)?;
        let mut head = vec![0u8; head_len];
        reader.read_exact(&mut head).map_err(|_| libc::EIO)?;
        let tail = if size > cairn_core::HEADER_HEAD_BYTES as u64 {
            let tail_len = (size - cairn_core::HEADER_HEAD_BYTES as u64)
                .min(cairn_core::HEADER_TAIL_BYTES as u64) as usize;
            reader
                .seek(SeekFrom::End(-(tail_len as i64)))
                .map_err(|_| libc::EIO)?;
            let mut t = vec![0u8; tail_len];
            reader.read_exact(&mut t).map_err(|_| libc::EIO)?;
            Some(t)
        } else {
            None
        };
        self.headers
            .put(&manifest_hash.hex(), &head, tail.as_deref())
            .map_err(|_| libc::EIO)?;

        let _ = std::fs::remove_file(&w.temp_path);
        // layout may have changed (marker/prodsys files created through the mount)
        self.refresh_native_layout();
        // release the pid-bound lease: the editor closed the file (Phase 3;
        // Phase 2 scope — the row lives at the domain root when scoped)
        let scope = self.lease_scope(&w.path);
        if self.native_for(&w.path).is_passthrough() {
            let _ = self.store.drop_lease(&scope); // no lease was taken; harmless
        } else {
            self.store.drop_lease(&scope).map_err(|_| libc::EIO)?;
        }
        tracing::info!(path = %w.path, size, chunks = spans.len(), "write-back committed");
        Ok(())
    }

    /// Truncate to `size` (FUSE setattr): spool + seed prefix + set_len + commit.
    /// Grow zero-fills; shrink drops the tail. Runs inline (no handle).
    pub fn truncate_entry(&self, path: &str, size: u64, pid: u32) -> Result<(), i32> {
        let fh = self.open_write_opts(path, pid, false, true)?;
        {
            let mut writes = self.writes.lock().expect("write table");
            let w = writes.by_fh.get_mut(&fh).ok_or(libc::EBADF)?;
            if size > 0 {
                self.seed_range_locked(w, 0, size)?;
                w.seeded = true;
            }
            w.temp.set_len(size).map_err(|_| libc::EIO)?;
            w.mtime_ms = Some(self.now_ms());
        }
        self.release_fh(fh)
    }

    /// Editor-visible remove (FUSE unlink). Local view removal — cross-device delete
    /// propagation rides the engine journal (the mount never invents journal ops).
    /// A path with an open write spool cannot be removed (EBUSY).
    pub fn unlink_entry(&self, path: &str) -> Result<(), i32> {
        {
            let writes = self.writes.lock().expect("write table");
            if writes.fh_for_path(path).is_some() {
                return Err(libc::EBUSY);
            }
        }
        if self.store.get_file(&self.project_id, path).is_none() {
            return Err(libc::ENOENT);
        }
        self.store
            .delete_file(&self.project_id, path)
            .map_err(|_| libc::EIO)?;
        self.refresh_native_layout();
        Ok(())
    }

    /// Rename = copy-through-commit to the new name + local unlink of the old.
    /// O(size) but exact; both sides must be closed (EBUSY otherwise).
    pub fn rename_entry(&self, from: &str, to: &str, pid: u32) -> Result<(), i32> {
        {
            let writes = self.writes.lock().expect("write table");
            if writes.fh_for_path(from).is_some() || writes.fh_for_path(to).is_some() {
                return Err(libc::EBUSY);
            }
        }
        let Some(f) = self.store.get_file(&self.project_id, from) else {
            return Err(libc::ENOENT);
        };
        if self.store.get_file(&self.project_id, to).is_some() {
            return Err(libc::EEXIST);
        }
        let fh = self.open_write_opts(to, pid, true, false)?;
        {
            let mut writes = self.writes.lock().expect("write table");
            let w = writes.by_fh.get_mut(&fh).ok_or(libc::EBADF)?;
            if f.size > 0 {
                let Some(mh) = &f.manifest_hash else {
                    return Err(libc::EIO);
                };
                let mut copied = 0u64;
                let buf = vec![0u8; WRITE_WINDOW];
                while copied < f.size {
                    let want = (f.size - copied).min(buf.len() as u64) as usize;
                    let chunk = self
                        .read_ranged_verified(mh, copied, want)
                        .map_err(|_| libc::EIO)?;
                    w.temp.write_all_at(&chunk, copied).map_err(|_| libc::EIO)?;
                    copied += chunk.len() as u64;
                }
                w.seeded = true;
                w.mtime_ms = Some(f.mtime);
            }
        }
        self.release_fh(fh)?;
        self.unlink_entry(from)
    }

    /// Heartbeat tick (ADR-0014 Phase 3): renew every open write's pid-bound lease.
    /// Called by the mount's heartbeat thread every [`HEARTBEAT_SECS`].
    pub fn heartbeat_once(&self) {
        let paths: Vec<(String, u32)> = {
            let writes = self.writes.lock().expect("write table");
            writes
                .by_fh
                .values()
                .map(|w| (w.path.clone(), w.pid))
                .collect::<Vec<_>>()
        };
        let mut seen = HashSet::new();
        for (path, pid) in paths {
            if !seen.insert(path.clone()) {
                continue;
            }
            if self.native_for(&path).is_passthrough() {
                continue;
            }
            let scope = self.lease_scope(&path);
            let token = self
                .store
                .get_lease(&scope)
                .map_or_else(|| lease_token(&scope, pid, self.now_ms()), |(t, _)| t);
            if self
                .store
                .put_lease_pid(
                    &scope,
                    token,
                    self.now_ms() + LEASE_TTL_MS,
                    Some(i64::from(pid)),
                    Some(&self.project_id),
                    Some(&self.device_id),
                )
                .is_err()
            {
                tracing::warn!(%path, scope = %scope, "lease heartbeat renew failed");
            }
        }
    }

    /// Unmount shutdown: release leases owned by open spools (editor pids die with the
    /// mount process; rows would also self-heal via the pid reaper, but clean is free).
    pub fn shutdown(&self) {
        self.stopped
            .store(true, std::sync::atomic::Ordering::Release);
        let paths: Vec<String> = {
            let writes = self.writes.lock().expect("write table");
            writes.by_fh.values().map(|w| w.path.clone()).collect()
        };
        for p in paths {
            let _ = self.store.drop_lease(&self.lease_scope(&p));
        }
    }

    /// Heartbeat-loop stop check (set by [`Self::shutdown`]).
    pub fn is_stopped(&self) -> bool {
        self.stopped.load(std::sync::atomic::Ordering::Acquire)
    }

    // === Virtual directory resolution ===============================================

    /// What lives at `path`? A synced file row, a synthesized directory (some synced
    /// path descends from it), or nothing. Directory entries EXIST in the mount even
    /// though only files are stored — the old lookup could never resolve nested paths.
    fn resolve_entry(&self, path: &str) -> Option<cairn_store::FileRow> {
        if let Some(f) = self.store.get_file(&self.project_id, path) {
            return Some(f);
        }
        let prefix = format!("{path}/");
        if self
            .store
            .list_files(&self.project_id)
            .iter()
            .any(|f| f.path.starts_with(&prefix))
        {
            return Some(cairn_store::FileRow {
                path: path.to_string(),
                project_id: self.project_id.clone(),
                manifest_hash: None,
                size: 0,
                mode: "dir".into(),
                mtime: 0,
                local_state: "clean".into(),
            });
        }
        None
    }

    /// Immediate children (files + synthesized dirs) of `path` ("" = root), with kinds.
    fn children_of(&self, prefix: &str) -> Vec<(String, bool)> {
        let mut files = HashSet::new();
        let mut dirs = HashSet::new();
        for f in self.store.list_files(&self.project_id) {
            let rest = if prefix.is_empty() {
                Some(f.path.as_str())
            } else {
                f.path.strip_prefix(&format!("{prefix}/"))
            };
            if let Some(rest) = rest {
                if rest.is_empty() {
                    continue;
                }
                match rest.split_once('/') {
                    Some((dir, _)) => {
                        dirs.insert(dir.to_string());
                    }
                    None => {
                        files.insert(rest.to_string());
                    }
                }
            }
        }
        let mut out: Vec<(String, bool)> = dirs
            .into_iter()
            .map(|d| (d, true))
            .chain(files.into_iter().map(|f| (f, false)))
            .collect();
        out.sort();
        out
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

    /// Mount at `mountpoint` (blocking; requires /dev/fuse + the `fuse` feature).
    /// Pure-Rust fuser: no libfuse headers needed at build time — only runtime /dev/fuse.
    #[cfg(feature = "fuse")]
    pub fn mount(self: Arc<Self>, mountpoint: &Path) -> Result<(), CairnError> {
        fuser::mount2(SharedFs(self), mountpoint, &[])
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
        self.read_ranged_verified(manifest_hex, offset, size)
    }

    /// RANGED read (review round): the old path assembled the WHOLE file in RAM
    /// (`Vec::with_capacity(total_len)`) for every non-header-cache read — a 50GB BRAW
    /// scrub was an instant OOM. Fetch and verify ONLY the chunks intersecting
    /// `[offset, offset+size)`; peak RAM is those chunks (≤16MB each). Fanout-safe via
    /// `flatten_deep` (the old `flatten()` returned nothing for Node manifests, so
    /// files beyond 8,192 chunks read as empty). I2 preserved: every contributing chunk is
    /// hash-verified before its bytes enter the response.
    fn read_ranged_verified(
        &self,
        manifest_hex: &str,
        offset: u64,
        size: usize,
    ) -> Result<Vec<u8>, CairnError> {
        let mh = Hash::from_hex(manifest_hex)
            .ok_or_else(|| CairnError::new(ErrorKind::ManifestFormat, "bad manifest hash"))?;
        let manifest_bytes = self.cas.get(&mh)?;
        let m = Manifest::parse(&manifest_bytes)?;
        let entries = m.flatten_deep(&mut |h| self.cas.get(h).ok());
        let want_end = offset.saturating_add(size as u64);
        let mut out: Vec<u8> = Vec::with_capacity(size.min(1 << 20));
        let mut pos = 0u64; // running file offset of the current entry start
        for e in entries {
            let chunk_start = pos;
            let chunk_end = pos + u64::from(e.len);
            pos = chunk_end;
            if chunk_end <= offset {
                continue; // entirely before the window
            }
            if chunk_start >= want_end {
                break; // entirely after the window (entries are sorted)
            }
            let raw = self.cas.get(&e.chunk_hash)?;
            if raw.len() != e.len as usize || Hash::of(&raw) != e.chunk_hash {
                return Err(CairnError::new(
                    ErrorKind::ChunkVerification,
                    format!("chunk {} failed verification", e.chunk_hash),
                ));
            }
            let from = offset.max(chunk_start) - chunk_start;
            let to = want_end.min(chunk_end) - chunk_start;
            out.extend_from_slice(&raw[from as usize..to as usize]);
            if out.len() >= size {
                break;
            }
        }
        Ok(out)
    }
}

#[cfg(feature = "fuse")]
impl Filesystem for SharedFs {
    // NOTE: `Filesystem` is implemented on this newtype (orphan rule bars foreign
    // trait × foreign Arc). Deref forwards every callback to the shared view; all
    // state is Mutex-protected, so the mount runs while the heartbeat thread holds
    // its own Arc clone.
    fn lookup(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: fuser::ReplyEntry) {
        let parent_path = self.path_of(parent).unwrap_or_default();
        let child = if parent_path.is_empty() {
            name.to_string_lossy().into_owned()
        } else {
            format!("{parent_path}/{}", name.to_string_lossy())
        };
        // files AND synthesized directories resolve here — the old lookup returned
        // ENOENT for every intermediate directory, making nested paths unreachable
        let Some(f) = self.resolve_entry(&child) else {
            reply.error(libc::ENOENT);
            return;
        };
        let mut inodes = self.inodes.lock().expect("inode table");
        let ino = inodes.alloc(&child);
        let is_dir = f.mode == "dir";
        reply.entry(&self.ttl, &attr(ino, f.size, is_dir, f.mtime), 0);
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
        match self.resolve_entry(&path) {
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
        let mut entries: Vec<(u64, bool, String)> = vec![
            (fuser::FUSE_ROOT_ID, true, ".".into()),
            (fuser::FUSE_ROOT_ID, true, "..".into()),
        ];
        for (name, is_dir) in self.children_of(&prefix) {
            let child = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            let cino = inodes.alloc(&child);
            entries.push((cino, is_dir, name));
        }
        drop(inodes);
        for (idx, (cino, is_dir, name)) in
            entries.into_iter().enumerate().skip(offset.max(0) as usize)
        {
            let kind = if is_dir {
                fuser::FileType::Directory
            } else {
                fuser::FileType::RegularFile
            };
            if reply.add(cino, (idx + 1) as i64, kind, name) {
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

    /// Create (O_CREAT): spool + lease; content commits at release.
    fn create(
        &mut self,
        req: &Request,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        flags: i32,
        reply: fuser::ReplyCreate,
    ) {
        let parent_path = self.path_of(parent).unwrap_or_default();
        let child = if parent_path.is_empty() {
            name.to_string_lossy().into_owned()
        } else {
            format!("{parent_path}/{}", name.to_string_lossy())
        };
        let fh = match self.open_write_opts(&child, req.pid(), true, flags & libc::O_TRUNC != 0) {
            Ok(fh) => fh,
            Err(e) => {
                eprintln!("cairn-fs-linux: create({child:?}) rejected with errno {e}");
                reply.error(e);
                return;
            }
        };
        let ino = self.inodes.lock().expect("inode table").alloc(&child);
        // a fresh create has no bytes yet; the attr is refreshed on lookup/getattr.
        // Kernel contract: attr.ino is the dentry inode (must be the table-allocated
        // ino — NOT the fh, which would collide with root ino 1 on the first create),
        // and the 4th reply argument is the file handle every later read/write carries.
        reply.created(&self.ttl, &attr(ino, 0, false, 0), 0, fh, 0);
    }

    /// Open for write through the spool (O_WRONLY/O_RDWR): lease acquire happens here.
    fn open(&mut self, req: &Request, ino: u64, flags: i32, reply: fuser::ReplyOpen) {
        let write_intent =
            flags & libc::O_WRONLY != 0 || flags & libc::O_RDWR != 0 || flags & libc::O_TRUNC != 0;
        if !write_intent {
            reply.opened(0, 0);
            return;
        }
        let Some(path) = self.path_of(ino) else {
            reply.error(libc::ENOENT);
            return;
        };
        match self.open_write_opts(&path, req.pid(), false, flags & libc::O_TRUNC != 0) {
            Ok(fh) => reply.opened(fh, 0),
            Err(e) => reply.error(e),
        }
    }

    fn write(
        &mut self,
        _req: &Request,
        ino: u64,
        fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: fuser::ReplyWrite,
    ) {
        let _ = ino; // the spool is keyed by fh; inode path identity was checked at open
        match self.write_fh(fh, offset, data) {
            Ok(n) => reply.written(n as u32),
            Err(e) => reply.error(e),
        }
    }

    fn flush(
        &mut self,
        _req: &Request,
        _ino: u64,
        _fh: u64,
        _lock_owner: u64,
        reply: fuser::ReplyEmpty,
    ) {
        // advisory flush: the spool holds the data; commit happens at last release
        reply.ok();
    }

    fn fsync(
        &mut self,
        _req: &Request,
        _ino: u64,
        fh: u64,
        datasync: bool,
        reply: fuser::ReplyEmpty,
    ) {
        match self.fsync_fh(fh, datasync) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn release(
        &mut self,
        _req: &Request,
        _ino: u64,
        fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: fuser::ReplyEmpty,
    ) {
        match self.release_fh(fh) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn setattr(
        &mut self,
        req: &Request,
        ino: u64,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        atime: Option<fuser::TimeOrNow>,
        mtime: Option<fuser::TimeOrNow>,
        _ctime: Option<std::time::SystemTime>,
        fh: Option<u64>,
        _crtime: Option<std::time::SystemTime>,
        _chgtime: Option<std::time::SystemTime>,
        _bkuptime: Option<std::time::SystemTime>,
        _flags: Option<u32>,
        reply: fuser::ReplyAttr,
    ) {
        let Some(path) = self.path_of(ino) else {
            reply.error(libc::ENOENT);
            return;
        };
        let ms = mtime.or(atime).map(time_or_now_ms);
        if let Some(ms) = ms {
            // editor-provided mtime lands on the spool (committed with the file)
            let mut writes = self.writes.lock().expect("write table");
            if let Some(w) = fh.and_then(|f| writes.by_fh.get_mut(&f)) {
                w.mtime_ms = Some(ms);
            } else if let Some(f) = self.store.get_file(&self.project_id, &path) {
                let _ = self
                    .store
                    .put_file(&cairn_store::FileRow { mtime: ms, ..f });
            }
        }
        if let Some(size) = size {
            match fh {
                Some(fh)
                    if self
                        .writes
                        .lock()
                        .expect("write table")
                        .by_fh
                        .contains_key(&fh) =>
                {
                    let res = (|| {
                        let mut writes = self.writes.lock().expect("write table");
                        let w = writes.by_fh.get_mut(&fh).ok_or(libc::EBADF)?;
                        if size > 0 && !w.no_seed {
                            self.seed_range_locked(w, 0, size)?;
                            w.seeded = true;
                        }
                        w.temp.set_len(size).map_err(|_| libc::EIO)?;
                        w.mtime_ms = Some(self.now_ms());
                        Ok(())
                    })();
                    if let Err(e) = res {
                        reply.error(e);
                        return;
                    }
                }
                _ => {
                    if let Err(e) = self.truncate_entry(&path, size, req.pid()) {
                        reply.error(e);
                        return;
                    }
                }
            }
        }
        let f = self.resolve_entry(&path);
        match f {
            Some(f) => reply.attr(&self.ttl, &attr(ino, f.size, f.mode == "dir", f.mtime)),
            None => reply.error(libc::ENOENT),
        }
    }

    fn unlink(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: fuser::ReplyEmpty) {
        let parent_path = self.path_of(parent).unwrap_or_default();
        let child = if parent_path.is_empty() {
            name.to_string_lossy().into_owned()
        } else {
            format!("{parent_path}/{}", name.to_string_lossy())
        };
        match self.unlink_entry(&child) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn rmdir(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: fuser::ReplyEmpty) {
        let parent_path = self.path_of(parent).unwrap_or_default();
        let child = if parent_path.is_empty() {
            name.to_string_lossy().into_owned()
        } else {
            format!("{parent_path}/{}", name.to_string_lossy())
        };
        // virtual directories vanish when their last file does; refuse non-empty
        if !self.children_of(&child).is_empty() {
            reply.error(libc::ENOTEMPTY);
            return;
        }
        if self.resolve_entry(&child).is_none() {
            reply.error(libc::ENOENT);
            return;
        }
        self.inodes
            .lock()
            .expect("inode table")
            .by_path
            .remove(&child);
        reply.ok();
    }

    fn mkdir(
        &mut self,
        _req: &Request,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: fuser::ReplyEntry,
    ) {
        let parent_path = self.path_of(parent).unwrap_or_default();
        let child = if parent_path.is_empty() {
            name.to_string_lossy().into_owned()
        } else {
            format!("{parent_path}/{}", name.to_string_lossy())
        };
        // directories are virtual (derived from file paths): accept and expose
        let ino = self.inodes.lock().expect("inode table").alloc(&child);
        reply.entry(&self.ttl, &attr(ino, 0, true, 0), 0);
    }

    fn rename(
        &mut self,
        req: &Request,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        _flags: u32,
        reply: fuser::ReplyEmpty,
    ) {
        let p = self.path_of(parent).unwrap_or_default();
        let from = if p.is_empty() {
            name.to_string_lossy().into_owned()
        } else {
            format!("{p}/{}", name.to_string_lossy())
        };
        let np = self.path_of(newparent).unwrap_or_default();
        let to = if np.is_empty() {
            newname.to_string_lossy().into_owned()
        } else {
            format!("{np}/{}", newname.to_string_lossy())
        };
        match self.rename_entry(&from, &to, req.pid()) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
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

    // === Write-back path (leases + spool + commit) ==================================

    fn empty_setup() -> (tempfile::TempDir, CairnFs) {
        use cairn_core::clock::WallClock;
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("store").as_path(), Arc::new(WallClock)).unwrap();
        let conn = store.conn_handle();
        let cas = Cas::open(&dir.path().join("blobs"), conn.clone()).unwrap();
        let headers = HeaderCache::new(conn);
        let fs = CairnFs::with_device(store, cas, headers, "p1", "dev-test");
        (dir, fs)
    }

    /// A live "foreign editor" pid for lease-conflict tests: a real spawned process.
    fn live_foreign_pid() -> u32 {
        std::process::Command::new("sleep")
            .arg("3")
            .spawn()
            .expect("spawn sleep")
            .id()
    }

    /// The write-back roundtrip: open → positional write → release commit → the
    /// content is served back verified, the row is dirty (engine pushes it), and
    /// the header cache is warm for the next editor (I1).
    #[test]
    fn write_back_commits_and_serves_verified() {
        let (_d, fs) = empty_setup();
        let pid = std::process::id();
        let payload: Vec<u8> = (0..5 * 1024 * 1024u64 as usize)
            .map(|i| (i % 251) as u8)
            .collect();

        let fh = fs
            .open_write_opts("renders/A001.mov", pid, true, false)
            .unwrap();
        // full rewrite from 0 (the common NLE render path) — no seed copy
        let n = fs.write_fh(fh, 0, &payload).unwrap();
        assert_eq!(n, payload.len());
        // append at EOF (offset payload.len) — allowed on a fresh spool
        let n2 = fs.write_fh(fh, payload.len() as i64, b"TAIL").unwrap();
        assert_eq!(n2, 4);
        fs.release_fh(fh).unwrap();

        // row exists, dirty, right size
        let row = fs.store.get_file("p1", "renders/A001.mov").unwrap();
        assert_eq!(row.size, (payload.len() + 4) as u64);
        assert_eq!(row.local_state, "dirty");

        // served content is byte-exact (ranged verified read through the manifest)
        let back = fs.serve_read("renders/A001.mov", 0, payload.len()).unwrap();
        assert_eq!(back, payload);
        let tail = fs
            .serve_read("renders/A001.mov", payload.len() as u64, 4)
            .unwrap();
        assert_eq!(tail, b"TAIL");

        // lease released on close (Phase 3: ephemeral)
        assert!(fs.store.get_lease("renders/A001.mov").is_none());
        // header cache warmed: serve_header succeeds with full head bytes
        let (head, _dt) = fs.serve_header("renders/A001.mov").unwrap();
        assert_eq!(head, &payload[..head.len()]);
        // staging temp removed
        assert!(fs
            .store
            .root()
            .join("staging")
            .read_dir()
            .map(|mut d| d.next().is_none())
            .unwrap_or(true));
    }

    /// In-place patch (the editor rewrites only part of an existing file): the
    /// prefix before the patch point is lazily seeded from the store, and the
    /// commit lands a NEW manifest while old chunks stay for GC.
    #[test]
    fn positioned_patch_seeds_prefix_and_commits() {
        use cairn_core::chunker::StreamHash;
        let content: Vec<u8> = (0..2 * 1024 * 1024).map(|i| (i % 249) as u8).collect();
        let (_d1, fs, _mh) = setup_with_file(&content);

        let pid = std::process::id();
        let fh = fs.open_write("A001.mov", pid).unwrap();
        // patch at 1MiB (a 512-byte edit): seeds [0, 1MiB) from the store first
        let patch = [0xABu8; 512];
        fs.write_fh(fh, 1024 * 1024, &patch).unwrap();
        fs.release_fh(fh).unwrap();

        let mut expected = content.clone();
        expected[1024 * 1024..1024 * 1024 + 512].copy_from_slice(&patch);
        let row = fs.store.get_file("p1", "A001.mov").unwrap();
        assert_eq!(row.size, expected.len() as u64);
        let sh = StreamHash::compute(&expected);
        // byte-exact roundtrip through the committed manifest
        let back = fs.serve_read("A001.mov", 0, expected.len()).unwrap();
        assert_eq!(back, expected);
        let _ = sh; // chunk-profile parity is pinned by the engine's own tests
    }

    /// O_TRUNC open discards existing content (no lazy seed) and commits empty.
    #[test]
    fn truncate_open_discards_content() {
        let content: Vec<u8> = vec![7u8; 3 * 1024 * 1024];
        let (_d1, fs, _mh) = setup_with_file(&content);
        let pid = std::process::id();
        let fh = fs.open_write_opts("A001.mov", pid, false, true).unwrap();
        fs.write_fh(fh, 0, b"fresh").unwrap();
        fs.release_fh(fh).unwrap();
        let row = fs.store.get_file("p1", "A001.mov").unwrap();
        assert_eq!(row.size, 5);
        assert_eq!(fs.serve_read("A001.mov", 0, 16).unwrap(), b"fresh");
    }

    /// Lease conflict: a LIVE foreign editor holds the file → EBUSY (with an
    /// override hint logged), and after the lease drops the open succeeds.
    #[test]
    fn live_lease_conflicts_until_dropped() {
        let (_d, fs) = empty_setup();
        let foreign = live_foreign_pid();
        // the file must EXIST for a write-open to reach the lease check (FUSE
        // returns ENOENT for missing paths before any arbitration)
        let fh0 = fs
            .open_write_opts("scene.prproj", std::process::id(), true, false)
            .unwrap();
        fs.write_fh(fh0, 0, b"v1").unwrap();
        fs.release_fh(fh0).unwrap();
        let now = fs.now_ms();
        fs.store
            .put_lease_pid(
                "scene.prproj",
                42,
                now + 60_000,
                Some(i64::from(foreign)),
                Some("p1"),
                Some("dev-other"),
            )
            .unwrap();
        let me = std::process::id();
        assert_eq!(fs.open_write("scene.prproj", me), Err(libc::EBUSY));
        // override: the lease is dropped (admin action or owner release)
        fs.store.drop_lease("scene.prproj").unwrap();
        let fh = fs.open_write("scene.prproj", me).unwrap();
        assert!(fs.store.get_lease("scene.prproj").is_some());
        fs.release_fh(fh).unwrap();
        std::process::Command::new("kill")
            .arg(foreign.to_string())
            .output()
            .ok();
    }

    /// Dead-owner self-heal (the core of "no manual pen"): the previous editor
    /// CRASHED (pid gone) — the next acquire reaps the stale row and succeeds
    /// with a fresh token, no human in the loop.
    #[test]
    fn dead_owner_lease_is_reaped_on_acquire() {
        let (_d, fs) = empty_setup();
        // file exists (a prior editor committed it) and its lease row is stale
        let fh0 = fs
            .open_write_opts("scene.prproj", std::process::id(), true, false)
            .unwrap();
        fs.write_fh(fh0, 0, b"v1").unwrap();
        fs.release_fh(fh0).unwrap();
        let now = fs.now_ms();
        fs.store
            .put_lease_pid(
                "scene.prproj",
                7,
                now + 60_000,
                Some(4_000_000), // a pid that does not exist
                Some("p1"),
                Some("dev-crashed"),
            )
            .unwrap();
        let me = std::process::id();
        let fh = fs.open_write("scene.prproj", me).expect("reap → acquire");
        // fresh token (stale fenced writers lose)
        let (token, _exp) = fs.store.get_lease("scene.prproj").unwrap();
        assert_ne!(token, 7);
        fs.release_fh(fh).unwrap();
    }

    /// ADR-0014 Phase 2 (domain decomposition): with `.cairn-domains` present, the
    /// lease row lives at the DOMAIN root — a second file in the same domain hits
    /// the live foreign pen (EBUSY), while other domains and unscoped files proceed
    /// independently. This is the >90% collision reduction, enforced by config.
    #[test]
    fn domain_scope_shares_lease_within_domain_and_isolates_across() {
        let (d, fs) = empty_setup();
        // config is an ordinary synced project file at the store root
        std::fs::write(
            d.path().join("store/.cairn-domains"),
            "sequences/A001\nsequences/B002\n",
        )
        .unwrap();

        let foreign = live_foreign_pid();
        let now = fs.now_ms();
        // editor A's pen on domain A001 (planted the way a real acquire leaves it)
        fs.store
            .put_lease_pid(
                "sequences/A001",
                42,
                now + 60_000,
                Some(i64::from(foreign)),
                Some("p1"),
                Some("dev-a"),
            )
            .unwrap();

        // second editor, ANOTHER file in the SAME domain → conflicts on the domain pen
        // (create-mode open so the path reaches arbitration — FUSE ENOENTs otherwise)
        let me = std::process::id();
        assert_eq!(
            fs.open_write_opts("sequences/A001/scene02.prproj", me, true, false),
            Err(libc::EBUSY),
            "same domain = one state boundary = one pen"
        );

        // DIFFERENT domain → independent pen (decomposition at work)
        let fh = fs
            .open_write_opts("sequences/B002/scene01.prproj", me, true, false)
            .expect("disjoint domain must proceed");
        // row lives at the DOMAIN scope while open (release-on-close drops it after)
        assert!(fs.store.get_lease("sequences/B002").is_some());
        assert!(fs
            .store
            .get_lease("sequences/B002/scene01.prproj")
            .is_none());
        fs.release_fh(fh).unwrap();

        // unscoped file → per-file lease (Phase 3 behavior unchanged)
        let fh = fs
            .open_write_opts("audio/vo/take3.wav", me, true, false)
            .expect("unscoped per-file");
        assert!(fs.store.get_lease("audio/vo/take3.wav").is_some());
        fs.release_fh(fh).unwrap();
        std::process::Command::new("kill")
            .arg(foreign.to_string())
            .output()
            .ok();
    }

    /// Phase 2 config propagates WITHOUT remount: the domains file is re-read per
    /// decision (it is a synced file — a teammate's push takes effect on the next
    /// write-open). Also covers longest-root-wins scoping through the mount.
    #[test]
    fn domain_config_applies_live_without_remount() {
        let (d, fs) = empty_setup();
        let me = std::process::id();

        // no config yet: per-file lease (held while open, released on close — Phase 3)
        let fh = fs
            .open_write_opts("sequences/A001/scene.prproj", me, true, false)
            .unwrap();
        assert!(fs.store.get_lease("sequences/A001/scene.prproj").is_some());
        fs.release_fh(fh).unwrap();
        assert!(fs.store.get_lease("sequences/A001/scene.prproj").is_none());

        // teammate syncs the domains file — next open scopes to the domain
        std::fs::write(
            d.path().join("store/.cairn-domains"),
            "sequences/A001/sub\n",
        )
        .unwrap();
        let fh = fs
            .open_write_opts("sequences/A001/sub/shot.prproj", me, true, false)
            .expect("own re-acquire inside now-scoped domain");
        assert!(fs.store.get_lease("sequences/A001/sub").is_some());
        assert!(fs
            .store
            .get_lease("sequences/A001/sub/shot.prproj")
            .is_none());
        fs.release_fh(fh).unwrap();
    }

    /// Native passthrough (ADR-0014 Phase 1): `.prodsys` paths and operator-declared
    /// markers take NO lease — Cairn stands down, the vendor engine arbitrates.
    #[test]
    fn passthrough_paths_skip_leases() {
        let (_d, fs) = empty_setup();
        // marker as a synced project file (content read via the mount's read path)
        let fh = fs
            .open_write_opts(
                cairn_sync::native_collab::MARKER_FILE,
                std::process::id(),
                true,
                false,
            )
            .unwrap();
        fs.write_fh(fh, 0, b"resolve-collab").unwrap();
        fs.release_fh(fh).unwrap();
        fs.refresh_native_layout();

        // now EVERY path is passthrough: write proceeds with no lease row at all
        let fh = fs
            .open_write_opts("Projects/a.rpp", std::process::id(), true, false)
            .unwrap();
        fs.write_fh(fh, 0, b"edit").unwrap();
        fs.release_fh(fh).unwrap();
        assert!(fs.store.get_lease("Projects/a.rpp").is_none());

        // `.prodsys` component rule works even without a marker
        let (_d2, fs2) = empty_setup();
        let fh = fs2
            .open_write_opts(
                "Show.prodsys/Show_01.prproj",
                std::process::id(),
                true,
                false,
            )
            .unwrap();
        fs2.write_fh(fh, 0, b"x").unwrap();
        fs2.release_fh(fh).unwrap();
        assert!(fs2.store.get_lease("Show.prodsys/Show_01.prproj").is_none());
    }

    /// Sibling-`.prodsys` rule against the VIRTUAL tree: a synced file under
    /// `Show/Show.prodsys/` makes `Show/Sequences/*` passthrough (Premiere owns
    /// the production), while files outside stay Cairn-leased.
    #[test]
    fn sibling_prodsys_resolved_from_synced_paths() {
        let (_d, fs) = empty_setup();
        let pid = std::process::id();
        let fh = fs
            .open_write_opts("Show/Show.prodsys/proddb.bin", pid, true, false)
            .unwrap();
        fs.write_fh(fh, 0, b"db").unwrap();
        fs.release_fh(fh).unwrap();
        assert_eq!(
            fs.native_for("Show/Sequences/scene.prproj"),
            cairn_sync::native_collab::NativeCollab::PremiereProductions
        );
        assert_eq!(
            fs.native_for("Other/other.prproj"),
            cairn_sync::native_collab::NativeCollab::Cairn
        );
    }

    /// Heartbeat renews the open write's lease with the SAME token (fencing
    /// stability) inside the TTL window; closed files are not renewed.
    #[test]
    fn heartbeat_renews_open_write_leases() {
        let (_d, fs) = empty_setup();
        let pid = std::process::id();
        let fh = fs.open_write_opts("live.prproj", pid, true, false).unwrap();
        let (token, exp1) = fs.store.get_lease("live.prproj").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        fs.heartbeat_once();
        let (token2, exp2) = fs.store.get_lease("live.prproj").unwrap();
        assert_eq!(token, token2, "heartbeat keeps the fencing token stable");
        assert!(exp2 >= exp1, "heartbeat pushes the expiry forward");
        fs.release_fh(fh).unwrap();
        // after close the heartbeat does not resurrect the lease
        fs.heartbeat_once();
        assert!(fs.store.get_lease("live.prproj").is_none());
    }

    /// Directory navigation: intermediate directories resolve (lookup/getattr path)
    /// and readdir exposes them with the right kind — the old mount could not even
    /// open a nested path.
    #[test]
    fn nested_paths_resolve_through_synthesized_dirs() {
        let (_d, fs) = empty_setup();
        let pid = std::process::id();
        for p in ["media/b-roll/clip.mov", "audio/a.wav"] {
            let fh = fs.open_write_opts(p, pid, true, false).unwrap();
            fs.write_fh(fh, 0, b"x").unwrap();
            fs.release_fh(fh).unwrap();
        }
        // directory entries resolve
        let d = fs.resolve_entry("media").unwrap();
        assert_eq!(d.mode, "dir");
        let d2 = fs.resolve_entry("media/b-roll").unwrap();
        assert_eq!(d2.mode, "dir");
        // children_of: files + dirs distinguished, deep nesting synthesized
        let root = fs.children_of("");
        assert_eq!(
            root,
            vec![("audio".to_string(), true), ("media".to_string(), true),]
        );
        let media = fs.children_of("media");
        assert_eq!(media, vec![("b-roll".to_string(), true)]);
        let broll = fs.children_of("media/b-roll");
        assert_eq!(broll, vec![("clip.mov".to_string(), false)]);
        // a missing path resolves to nothing
        assert!(fs.resolve_entry("media/missing.mov").is_none());
    }

    /// unlink removes the local row; open spools block removal (EBUSY).
    #[test]
    fn unlink_removes_row_and_blocks_open_spools() {
        let content: Vec<u8> = vec![9u8; 1024];
        let (_d1, fs, _mh) = setup_with_file(&content);
        let pid = std::process::id();
        let fh = fs.open_write("A001.mov", pid).unwrap();
        assert_eq!(fs.unlink_entry("A001.mov"), Err(libc::EBUSY));
        fs.release_fh(fh).unwrap();
        fs.unlink_entry("A001.mov").unwrap();
        assert!(fs.store.get_file("p1", "A001.mov").is_none());
        assert_eq!(fs.unlink_entry("A001.mov"), Err(libc::ENOENT));
    }

    /// rename commits the content under the new name and removes the old row.
    #[test]
    fn rename_copies_commit_and_drops_old() {
        let content: Vec<u8> = (0..1024u32).map(|i| i as u8).collect();
        let (_d1, fs, _mh) = setup_with_file(&content);
        let pid = std::process::id();
        fs.rename_entry("A001.mov", "A002.mov", pid).unwrap();
        assert!(fs.store.get_file("p1", "A001.mov").is_none());
        let row = fs.store.get_file("p1", "A002.mov").unwrap();
        assert_eq!(row.size, content.len() as u64);
        assert_eq!(fs.serve_read("A002.mov", 0, 1024).unwrap(), content);
    }

    /// truncate_entry shrinks to the requested size (seeded prefix + set_len).
    #[test]
    fn truncate_entry_shrinks_committed_content() {
        let content: Vec<u8> = (0..2 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
        let (_d1, fs, _mh) = setup_with_file(&content);
        fs.truncate_entry("A001.mov", 1024 * 1024, std::process::id())
            .unwrap();
        let row = fs.store.get_file("p1", "A001.mov").unwrap();
        assert_eq!(row.size, 1024 * 1024);
        let back = fs.serve_read("A001.mov", 0, 1024 * 1024).unwrap();
        assert_eq!(back, &content[..1024 * 1024]);
    }
}
