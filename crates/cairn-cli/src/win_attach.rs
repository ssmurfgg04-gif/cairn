//! Windows attach glue (WO6-1 §2–§5 + WO6-2): when the daemon attaches a root on
//! Windows it (a) registers the sync root with the real cldflt driver, (b) BULK
//! creates placeholders for every known file (CfCreatePlaceholders batch — a 2 GB
//! tree appears at once, no per-file walk), (c) connects the WRITE-BACK callback
//! table backed by [`DaemonSource`], which serves hydration bytes from the local
//! CAS (server misses fetch + verify + cache) and translates filter notifications
//! into engine actions (dirty rows, leases, tombstones).
//!
//! Everything here is cfg(windows): on other platforms the attach path is unchanged.
//! Design: docs/design/write-back.md.

#![cfg(windows)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cairn_core::compress::decompress_chunk;
use cairn_core::hash::Hash;
use cairn_core::manifest::Manifest;
use cairn_core::{CairnError, ErrorKind};
use cairn_fs_win::cfapi::{
    connect_write_back, create_placeholders_batch, register_sync_root, BulkEntry, Connection,
    ValidateOutcome, WriteBackSource,
};
use cairn_store::{Cas, Store};
use cairn_sync::plane::Plane;

/// Extensions that get a LEASE auto-acquired on open (project-file family, v1).
/// Media files are deliberately NOT leased: leases fence the small, contentious
/// head files editors open for write, not the multi-GB media they read.
const LEASED_EXTENSIONS: &[&str] = &[
    ".prproj", ".drp", ".nce", ".avp", ".veg", ".aep", ".blend", ".fcpxmld",
];

/// The daemon-backed write-back source: filter callbacks → engine actions.
pub struct DaemonSource {
    pub store: Store,
    pub cas: Cas,
    pub plane: Arc<dyn Plane>,
    pub tenant_id: String,
    pub project_id: String,
    pub device_id: String,
    /// root path (for resolving NormalizedPath → relative engine path)
    pub root: PathBuf,
    /// tokio handle for spawning async work (lease acquire) from sync callbacks
    pub rt: tokio::runtime::Handle,
}

impl DaemonSource {
    fn rel_path(&self, full: &str) -> String {
        // Filter NormalizedPath quirks (first Windows run, 2026-09-01): the path may
        // carry an NT namespace prefix (\\?\, \??\, \\.\) and the volume prefix can
        // differ in CASE from the registered root (8.3 short names expand to long
        // names). Both break a plain strip_prefix and would silently swallow the
        // close/delete notification. Match the root prefix CASE-INSENSITIVELY but
        // slice the ORIGINAL string — row keys keep the registered casing (SQLite
        // TEXT compares case-sensitively).
        let stripped = full
            .strip_prefix("\\\\?\\")
            .or_else(|| full.strip_prefix("\\??\\"))
            .or_else(|| full.strip_prefix("\\\\.\\"))
            .unwrap_or(full);
        let root_str = self.root.to_string_lossy().replace('/', "\\");
        let root_trim = root_str.trim_end_matches(['\\', '/']);
        let matched_root = if stripped.len() == stripped.to_lowercase().len()
            && stripped
                .get(..root_trim.len())
                .is_some_and(|head| head.eq_ignore_ascii_case(root_trim))
        {
            // root prefix matches (case-insensitive) — cut it off in original casing
            &stripped[root_trim.len()..]
        } else {
            Path::new(stripped)
                .strip_prefix(&self.root)
                .unwrap_or(Path::new(stripped))
                .to_str()
                .unwrap_or(stripped)
        };
        matched_root
            .trim_start_matches(['\\', '/'])
            .replace('\\', "/")
    }

    fn is_project_file(&self, full: &str) -> bool {
        let lower = full.to_lowercase();
        LEASED_EXTENSIONS.iter().any(|ext| lower.ends_with(ext))
    }

    /// Current head manifest hash for a path (None = unknown to the engine).
    fn head(&self, rel: &str) -> Option<String> {
        self.store
            .get_file(&self.project_id, rel)
            .and_then(|r| r.manifest_hash)
    }

    /// Is the manifest's content fully in the local CAS? (bounded: leaf chunks with
    /// early exit; the fanout is small relative to file size)
    fn fully_local(&self, manifest_hex: &str) -> bool {
        let Some(h) = Hash::from_hex(manifest_hex) else {
            return false;
        };
        let Ok(bytes) = self.cas.get(&h) else {
            return false;
        };
        let Ok(m) = Manifest::parse(&bytes) else {
            return false;
        };
        // Fanout-safe (review round): `flatten()` is leaf-only; a Node yielded zeroed
        // placeholder entries and fully_local was permanently false — fanout files
        // never took the fast path. Walk children via the CAS (push mirrors them).
        m.flatten_deep(&mut |h| self.cas.get(h).ok())
            .iter()
            .all(|e| self.cas.contains(&e.chunk_hash))
    }
}

impl cairn_fs_win::cfapi::PlaceholderSource for DaemonSource {
    /// Hydration bytes for FETCH_DATA: the identity IS the file manifest hash.
    /// Assembly: manifest (CAS → plane) → per-chunk bytes (CAS → plane, decompress
    /// stored→raw) → hash-verified chunk puts into the local CAS → ranged slice.
    /// The FETCH_DATA contract demands EXACTLY `len` bytes at `offset`; any miss
    /// fails the hydration loudly (I2 — never serve unverified bytes).
    fn fetch(&self, manifest_hash_hex: &str, offset: u64, len: u32) -> Result<Vec<u8>, i32> {
        let (policy, transform, entries) = self.load_manifest_leaves(manifest_hash_hex)?;
        let start = offset as usize;
        let end = start + len as usize;
        let mut out = Vec::with_capacity(len as usize);
        for e in &entries {
            let e_start = e.offset as usize;
            let e_end = e_start + e.len as usize;
            if e_end <= start || e_start >= end {
                continue; // chunk outside the requested window
            }
            let raw = self.chunk_raw(e.chunk_hash, policy)?;
            let from = start.saturating_sub(e_start);
            let to = (end - e_start).min(e.len as usize);
            out.extend_from_slice(&raw[from..to]);
            if out.len() == len as usize {
                break;
            }
        }
        if out.len() != len as usize {
            return Err(0xC000_000Bu32 as i32); // short read = hydration failure
        }
        // container transform: the stored chunks cover the INNER payload; the filter
        // must see the real wrapper so editors parse the file normally.
        let _ = transform; // wrapper rebuild for ranged reads is byte-position unstable;
                           // transform-active files are served FULLY hydrated via
                           // materialize (their placeholders hydrate on attach), so
                           // ranged FETCH_DATA never sees them in practice.
        Ok(out)
    }
}

impl DaemonSource {
    /// Manifest lookup: local CAS first, plane second (lands in the CAS on arrival —
    /// the reconcile sweep needs it there too).
    fn load_manifest_leaves(
        &self,
        manifest_hash_hex: &str,
    ) -> Result<
        (
            cairn_core::compress::Compression,
            cairn_core::normalize::Transform,
            Vec<cairn_core::manifest::ManifestEntry>,
        ),
        i32,
    > {
        use cairn_core::manifest::Manifest;
        let h = Hash::from_hex(manifest_hash_hex).ok_or(0xC000_000Bu32 as i32)?;
        let bytes = match self.cas.get(&h) {
            Ok(b) => b,
            Err(_) => {
                let fetched = self
                    .rt
                    .block_on(self.plane.get_manifest(&self.tenant_id, manifest_hash_hex))
                    .map_err(|_| 0xC000_0225u32 as i32)?;
                let _ = self.cas.put(&h, &fetched);
                fetched
            }
        };
        let manifest = Manifest::parse(&bytes).map_err(|_| 0xC000_000Bu32 as i32)?;
        let (policy, transform) = match &manifest {
            Manifest::Leaf {
                compression,
                transform,
                ..
            }
            | Manifest::Node {
                compression,
                transform,
                ..
            } => (*compression, *transform),
        };
        let mut all = Vec::new();
        match &manifest {
            Manifest::Leaf { entries, .. } => all.extend(entries.iter().cloned()),
            Manifest::Node { .. } => {
                // Fanout walk with CAS→plane resolution at every depth (review round:
                // the old walk handled exactly one child level — depth-3 grandchildren
                // were silently dropped, surfacing as short-read hydration failures).
                for e in manifest.flatten_deep(&mut |h: &Hash| {
                    if let Ok(b) = self.cas.get(h) {
                        return Some(b);
                    }
                    self.rt
                        .block_on(self.plane.get_manifest(&self.tenant_id, &h.hex()))
                        .ok()
                }) {
                    all.push(e);
                }
            }
        }
        Ok((policy, transform, all))
    }

    /// RAW chunk bytes: CAS first; plane second (stored → decompress → verified
    /// CAS put, exactly like the engine's hydration path).
    fn chunk_raw(
        &self,
        h: Hash,
        policy: cairn_core::compress::Compression,
    ) -> Result<Vec<u8>, i32> {
        if let Ok(raw) = self.cas.get(&h) {
            return Ok(raw);
        }
        let stored = self
            .rt
            .block_on(self.plane.fetch_object(&self.tenant_id, &h.hex()))
            .map_err(|_| 0xC000_0225u32 as i32)?;
        let raw = decompress_chunk(&stored, policy, None).map_err(|_| 0xC000_000Bu32 as i32)?;
        // BLAKE3-verified before landing in the local CAS (I2)
        self.cas.put(&h, &raw).map_err(|_| 0xC000_000Bu32 as i32)?;
        Ok(raw)
    }
}

impl WriteBackSource for DaemonSource {
    fn write_open_validate(&self, full_path: &str, identity: &str) -> ValidateOutcome {
        let rel = self.rel_path(full_path);
        let Some(head) = self.head(&rel) else {
            // unknown to the engine yet (new local file?): treat as stale so the
            // filter hydrates from the identity bytes we know about
            return ValidateOutcome::Stale;
        };
        if head != identity {
            return ValidateOutcome::Stale;
        }
        if self.fully_local(&head) {
            ValidateOutcome::CurrentHydrated
        } else {
            ValidateOutcome::CurrentDehydrated
        }
    }

    fn open_notified(&self, full_path: &str) {
        if !self.is_project_file(full_path) {
            return;
        }
        let rel = self.rel_path(full_path);
        // ADR-0014 Phase 1 (native passthrough): a vendor multi-user engine (Premiere
        // Productions `.prodsys`, operator-declared Resolve collab) owns arbitration for
        // this file — Cairn takes NO lease and adds no second pen. Correctness is
        // unaffected: unleased appends carry token 0 by design (SPEC §8).
        if cairn_sync::native_collab::is_passthrough(&self.root, &rel) {
            tracing::debug!(path = %rel, "native collab passthrough: vendor arbitrates, no lease");
            let _ = self.store.drop_lease(&rel);
            return;
        }
        // lease auto-acquire (WO6-1 §5) — now ADR-0014 Phase 3 EPHEMERAL: 15s TTL bound
        // to THIS process (pid), kept alive by the daemon heartbeat (5s), auto-reaped on
        // process death and released on close. Crashed editors free their pen in
        // seconds — no human unblocking (the old 60s TTL + no reaper was the "manual
        // pen" problem). Expired leases still fail the append with STALE_LEASE, which
        // surfaces as a conflict (never silent overwrite).
        // Phase 2: the lease is taken at the DOMAIN scope when the file falls under a
        // declared `.cairn-domains` root (synced project file — every device resolves
        // the identical scope, so FUSE mounts and CfAPI attaches agree on the pen).
        let store = self.store.clone();
        let plane = Arc::clone(&self.plane);
        let tenant = self.tenant_id.clone();
        let project = self.project_id.clone();
        let device = self.device_id.clone();
        let rel2 = cairn_sync::domains::resolve_from_dir(&self.root, &rel);
        self.rt.spawn(async move {
            match plane
                .acquire_lease(
                    &tenant,
                    &project,
                    &rel2,
                    &device,
                    cairn_sync::LEASE_TTL_MS,
                )
                .await
            {
                Ok((token, expires_at)) => {
                    let _ = store.put_lease_pid(
                        &rel2,
                        token,
                        expires_at,
                        Some(i64::from(std::process::id())),
                        Some(&project),
                        Some(&device),
                    );
                    tracing::info!(path = %rel2, token, "ephemeral lease acquired on open (pid-bound)");
                }
                Err(e) => {
                    // non-fatal: file opens, but save-back races another device
                    tracing::warn!(path = %rel2, "lease acquire failed: {e}");
                }
            }
        });
    }

    fn close_notified(&self, full_path: &str) {
        let rel = self.rel_path(full_path);
        // ADR-0014 Phase 3: the editor closed the file — hand the pen back IMMEDIATELY
        // (best-effort; a failed release is harmless: the 15s TTL expires it anyway).
        if let Some((token, _)) = self.store.get_lease(&rel) {
            let plane = Arc::clone(&self.plane);
            let tenant = self.tenant_id.clone();
            let project = self.project_id.clone();
            let device = self.device_id.clone();
            let rel2 = rel.clone();
            self.rt.spawn(async move {
                match plane
                    .release_lease(&tenant, &project, &rel2, &device, token)
                    .await
                {
                    Ok(()) => tracing::debug!(path = %rel2, "lease released on close"),
                    Err(e) => tracing::debug!(path = %rel2, "lease release on close: {e}"),
                }
            });
            let _ = self.store.drop_lease(&rel);
        }
        // dirty-mark via the SAME predicate the engine trusts (size+mtime vs the
        // journaled row): hydration echoes stay suppressed, real edits dirty.
        let Some(row) = self.store.get_file(&self.project_id, &rel) else {
            // brand-new file in the root: the watcher path handles the rescan
            return;
        };
        let meta = match std::fs::metadata(full_path) {
            Ok(m) => m,
            // fail-safe: a stat we cannot perform is NOT evidence the file matches
            // the journaled stat — mark dirty and let the scan/sweep classify
            // (this also covers the deleted-between-close-and-stat case).
            Err(_) => {
                let _ = self.store.set_file_state(&self.project_id, &rel, "dirty");
                tracing::warn!(path = %rel, "write-back: stat failed at close; marked dirty");
                return;
            }
        };
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| i64::try_from(d.as_millis()).unwrap_or(0))
            .unwrap_or(0);
        if meta.len() != row.size || mtime != row.mtime {
            let _ = self.store.set_file_state(&self.project_id, &rel, "dirty");
            tracing::info!(path = %rel, size = meta.len(), "write-back: file marked dirty");
        }
    }

    fn delete_notified(&self, full_path: &str) {
        let rel = self.rel_path(full_path);
        if self.store.get_file(&self.project_id, &rel).is_some() {
            // engine push turns the row into a FileDeleteOp tombstone (30-day
            // server-side trash); the row state carries the intent durably.
            let _ = self.store.set_file_state(&self.project_id, &rel, "deleted");
            tracing::info!(path = %rel, "write-back: delete intent recorded");
        }
    }
}

/// Windows attach: register + bulk placeholders + write-back connection.
/// Returns the connection guard (the CALLER must keep it alive for the runtime's
/// lifetime — dropping it disconnects the root).
pub fn attach_windows(
    store: &Store,
    root: &Path,
    project_id: &str,
    tenant_id: &str,
    device_id: &str,
    plane: Arc<dyn Plane>,
    rt: tokio::runtime::Handle,
) -> Result<Connection, CairnError> {
    let root_str = root.to_string_lossy().into_owned();
    register_sync_root(&root_str, "Cairn")
        .map_err(|e| CairnError::new(ErrorKind::Io, format!("CfRegisterSyncRoot: {e:#x}")))?;

    let cas = Cas::open(&store.root().join("blobs"), store.conn_handle())?;
    let source = DaemonSource {
        store: store.clone(),
        cas,
        plane,
        tenant_id: tenant_id.to_string(),
        project_id: project_id.to_string(),
        device_id: device_id.to_string(),
        root: root.to_path_buf(),
        rt,
    };

    // bulk placeholders (WO6-2): every known file row becomes a placeholder in ONE
    // filter call per directory — attach of a 2 GB tree is metadata-speed, not
    // content-speed. Chunks stay server-side until first open (hydration on demand).
    let rows = store.list_files(project_id);
    let entries: Vec<BulkEntry> = rows
        .iter()
        .filter(|r| r.mode == "file")
        .filter_map(|r| {
            let identity = r.manifest_hash.clone()?;
            Some(BulkEntry {
                relative_path: r.path.replace('/', "\\"),
                identity_hex: identity,
                size: r.size,
                // punch #5: stamp the journaled mtime so the scan predicate
                // keeps the fresh placeholder CLEAN (lazy) instead of
                // redirtying + re-hydrating the whole attach
                mtime_ms: r.mtime,
            })
        })
        .collect();
    if !entries.is_empty() {
        create_placeholders_batch(&root_str, &entries).map_err(|(idx, e)| {
            let p = entries
                .get(idx)
                .map(|b| b.relative_path.clone())
                .unwrap_or_default();
            CairnError::new(
                ErrorKind::Io,
                format!("CfCreatePlaceholders batch failed at {p}: {e:#x}"),
            )
        })?;
        tracing::info!(project = %project_id, count = entries.len(), "bulk placeholders created");
    }

    let identity_by_path: HashMap<String, String> = rows
        .iter()
        .filter_map(|r| r.manifest_hash.clone().map(|h| (r.path.clone(), h)))
        .collect();
    let _ = identity_by_path; // heads live in the store; DaemonSource reads rows live

    connect_write_back(&root_str, Arc::new(source))
        .map_err(|e| CairnError::new(ErrorKind::Io, format!("CfConnectSyncRoot: {e:#x}")))
}
