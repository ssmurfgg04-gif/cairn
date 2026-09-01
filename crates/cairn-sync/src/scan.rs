//! Initial root scan (WO1): walk the attached workspace, insert per-file rows, and mark
//! everything new/changed `dirty` so the engine's push phase picks it up. Idempotent by
//! construction — the scan may be interrupted at ANY point (kill -9) and re-run:
//! rows already `synced` with unchanged size+mtime are skipped, everything else is
//! (re-)marked dirty and the engine re-chunks; server-side request_id dedup guarantees
//! zero duplicate journal entries across restarts (I2).

use std::path::Path;

use cairn_core::pathutil::{is_ignored, nfc_normalize};
use cairn_core::{CairnError, ErrorKind};
use cairn_store::state::LocalState;
use cairn_store::{Cas, FileRow, Store};

use crate::workspace::workspace_dir;

/// Scan outcome (doctor/status surface).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ScanStats {
    pub files_seen: u64,
    pub dirs_seen: u64,
    pub new_dirty: u64,
    pub redirtied: u64,
    pub skipped_unchanged: u64,
    pub bytes_seen: u64,
}

/// Millis-since-epoch mtime in the same encoding as `FileRow.mtime` (rows and
/// stat comparisons must agree bit-for-bit — punch #5 echo check + sweep).
pub fn mtime_millis(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// Walk `root` (the attached workspace) and reconcile rows for `project_id`.
/// Deterministic order (sorted per directory). Symlinks are recorded but not chunked
/// (SPEC §10); ignore-list entries (.cairn, .DS_Store, …) are skipped entirely.
pub fn scan_root(store: &Store, project_id: &str, root: &Path) -> Result<ScanStats, CairnError> {
    let mut stats = ScanStats::default();
    walk(store, project_id, root, root, &mut stats)?;
    Ok(stats)
}

fn walk(
    store: &Store,
    project_id: &str,
    root: &Path,
    dir: &Path,
    stats: &mut ScanStats,
) -> Result<(), CairnError> {
    let entries = std::fs::read_dir(dir)
        .map_err(|e| CairnError::new(ErrorKind::Io, format!("readdir {}: {e}", dir.display())))?;
    let mut names: Vec<std::fs::DirEntry> = entries.filter_map(|e| e.ok()).collect();
    names.sort_by_key(|e| e.file_name());
    for entry in names {
        let name = entry.file_name().to_string_lossy().into_owned();
        let rel = match dir.strip_prefix(root) {
            Ok(rest) if !rest.as_os_str().is_empty() => {
                format!("{}/{}", rest.to_string_lossy().replace('\\', "/"), name)
            }
            _ => name.clone(),
        };
        let rel = nfc_normalize(&rel);
        if is_ignored(&rel) {
            continue;
        }
        let path = entry.path();
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(_) => continue, // raced deletion: next pass sees the truth
        };
        if meta.is_dir() {
            stats.dirs_seen += 1;
            store.put_file(&FileRow {
                path: rel.clone(),
                project_id: project_id.into(),
                manifest_hash: None,
                size: 0,
                mode: "dir".into(),
                mtime: mtime_millis(&meta),
                local_state: LocalState::Synced.as_str().into(),
            })?;
            walk(store, project_id, root, &path, stats)?;
            continue;
        }
        if meta.file_type().is_symlink() {
            // symlink objects ride the journal without chunking (SPEC §10); the walking
            // skeleton records them but does not sync their targets
            stats.files_seen += 1;
            store.put_file(&FileRow {
                path: rel.clone(),
                project_id: project_id.into(),
                manifest_hash: None,
                size: 0,
                mode: "symlink".into(),
                mtime: mtime_millis(&meta),
                local_state: LocalState::Synced.as_str().into(),
            })?;
            continue;
        }
        let size = i64::try_from(meta.len()).unwrap_or(i64::MAX).max(0) as u64;
        let mtime = mtime_millis(&meta);
        stats.files_seen += 1;
        stats.bytes_seen = stats.bytes_seen.saturating_add(size);
        reconcile_file(store, project_id, &rel, size, mtime, stats)?;
    }
    Ok(())
}

fn reconcile_file(
    store: &Store,
    project_id: &str,
    rel: &str,
    size: u64,
    mtime: i64,
    stats: &mut ScanStats,
) -> Result<(), CairnError> {
    if let Some(existing) = store.get_file(project_id, rel) {
        let unchanged = existing.mode == "file"
            && existing.size == size
            && existing.mtime == mtime
            && matches!(
                LocalState::parse(&existing.local_state),
                Some(LocalState::Synced) | Some(LocalState::Clean) | Some(LocalState::Pinned)
            );
        if unchanged {
            stats.skipped_unchanged += 1;
            return Ok(());
        }
        // changed (or mid-pipeline when we crashed) → force re-chunk; idempotent upstream
        store.set_file_state(project_id, rel, LocalState::Dirty.as_str())?;
        stats.redirtied += 1;
        return Ok(());
    }
    store.put_file(&FileRow {
        path: rel.into(),
        project_id: project_id.into(),
        manifest_hash: None,
        size,
        mode: "file".into(),
        mtime,
        local_state: LocalState::Dirty.as_str().into(),
    })?;
    stats.new_dirty += 1;
    Ok(())
}

/// Convenience wrapper: scan the project's bound workspace.
pub fn scan_project(store: &Store, project_id: &str) -> Result<ScanStats, CairnError> {
    let root = workspace_dir(store, project_id);
    if !root.is_dir() {
        return Err(CairnError::new(
            ErrorKind::Io,
            format!("workspace {} is not a directory", root.display()),
        ));
    }
    scan_root(store, project_id, &root)
}

/// Periodic reconciliation sweep (punch #5, belt-and-braces). The watcher is
/// event-driven and can miss (events dropped while the process is wedged, external
/// edits that bypass mtime, size-preserving in-place writes during a race window).
/// The sweep closes that class with two passes:
///
/// 1. **stat walk** (cheap, full coverage) — [`scan_root`] re-stats every file;
///    any size OR mtime drift from the journaled row re-dirties it.
/// 2. **bounded rehash sample** — a rotating window of `sample_budget_files` synced
///    rows (byte-capped at `sample_budget_bytes`) is re-chunked in memory and its
///    chunk-hash sequence compared against the journaled manifest. A mismatch is
///    silent divergence (size AND mtime preserved) — redirtied. This is the only
///    mechanism in the system that catches content drift with NO stat signal.
///
/// Cost is bounded: the stat walk is stat-only for unchanged files; the rehash sample
/// is capped by both budgets. Transform-active manifests (normalization) are SKIPPED
/// honestly — re-chunking raw wrapper bytes would not match the inner-payload
/// manifest — and counted (`skipped_transform`), never misclassified as divergence.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SweepStats {
    pub stat_redirtied: u64,
    pub rehashed: u64,
    pub bytes_rehashed: u64,
    pub rehash_dirty: u64,
    pub skipped_transform: u64,
    pub budget_exhausted: bool,
}

pub fn reconcile_sweep(
    store: &Store,
    project_id: &str,
    root: &Path,
    sweep_counter: u64,
    sample_budget_files: usize,
    sample_budget_bytes: u64,
) -> Result<SweepStats, CairnError> {
    let mut out = SweepStats::default();

    // pass 1: full stat-level reconciliation (also redirties anything the watcher missed)
    let stats = scan_root(store, project_id, root)?;
    out.stat_redirtied = stats.redirtied + stats.new_dirty;

    // pass 2: rotating bounded rehash sample over SYNCED file rows
    let cas = Cas::open(&store.root().join("blobs"), store.conn_handle())?;
    let mut candidates: Vec<FileRow> = store
        .list_files(project_id)
        .into_iter()
        .filter(|r| {
            r.mode == "file"
                && r.manifest_hash.is_some()
                && matches!(
                    LocalState::parse(&r.local_state),
                    Some(LocalState::Synced) | Some(LocalState::Clean) | Some(LocalState::Pinned)
                )
        })
        .collect();
    if candidates.is_empty() {
        return Ok(out);
    }
    candidates.sort_by(|a, b| a.path.cmp(&b.path));
    let window = sample_budget_files.max(1);
    let start = (sweep_counter as usize * window) % candidates.len();
    let sample: Vec<FileRow> = candidates
        .iter()
        .cycle()
        .skip(start)
        .take(window.min(candidates.len()))
        .cloned()
        .collect();

    for row in sample {
        if out.bytes_rehashed >= sample_budget_bytes {
            out.budget_exhausted = true;
            break;
        }
        let full = root.join(&row.path);
        let Ok(bytes) = std::fs::read(&full) else {
            continue; // raced deletion: pass 1 already handled the stat truth
        };
        if out.bytes_rehashed + bytes.len() as u64 > sample_budget_bytes {
            out.budget_exhausted = true;
            break;
        }
        let Some(manifest) = load_manifest(&cas, row.manifest_hash.as_deref()) else {
            continue; // manifest not (yet) local — not evidence of divergence
        };
        if manifest_transform(&manifest) != cairn_core::normalize::Transform::None {
            out.skipped_transform += 1;
            continue;
        }
        // compare chunk-hash sequences: the manifest describes the content the journal
        // believes is on disk; re-chunking with the same deterministic FastCDC params
        // must reproduce it byte-for-byte if nothing diverged.
        let sh = cairn_core::chunker::StreamHash::compute(&bytes);
        let fresh: Vec<String> = sh.chunk_hashes.iter().map(|h| h.hex()).collect();
        let journaled: Vec<String> = manifest
            .flatten_with(&mut |h| load_manifest(&cas, Some(&h.hex())))
            .iter()
            .map(|e| e.chunk_hash.hex())
            .collect();
        out.rehashed += 1;
        out.bytes_rehashed += bytes.len() as u64;
        if fresh != journaled {
            // silent divergence (size+mtime both preserved): force a re-push
            store.set_file_state(project_id, &row.path, LocalState::Dirty.as_str())?;
            out.rehash_dirty += 1;
        }
    }
    Ok(out)
}

/// Load + parse a manifest from the local CAS. Returns None on any absence/corruption —
/// the sweep is reconciliation, not repair; the engine's pull path owns manifest fetch.
fn load_manifest(
    cas: &cairn_store::Cas,
    hash_hex: Option<&str>,
) -> Option<cairn_core::manifest::Manifest> {
    let hex = hash_hex?;
    let bytes = cas.get(&cairn_core::hash::Hash::from_hex(hex)?).ok()?;
    cairn_core::manifest::Manifest::parse(&bytes).ok()
}

/// Container transform of a manifest (leaf or fanout node — uniform per file).
fn manifest_transform(m: &cairn_core::manifest::Manifest) -> cairn_core::normalize::Transform {
    match m {
        cairn_core::manifest::Manifest::Leaf { transform, .. } => *transform,
        cairn_core::manifest::Manifest::Node { transform, .. } => *transform,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cairn_core::clock::WallClock;
    use std::sync::Arc;

    fn setup() -> (tempfile::TempDir, Store, tempfile::TempDir) {
        let home = tempfile::tempdir().unwrap();
        let store = Store::open(home.path(), Arc::new(WallClock)).unwrap();
        let ws = tempfile::tempdir().unwrap();
        (home, store, ws)
    }

    #[test]
    fn scan_marks_new_files_dirty_and_is_idempotent() {
        let (_h, store, ws) = setup();
        std::fs::write(ws.path().join("a.mov"), b"0123456789").unwrap();
        std::fs::create_dir(ws.path().join("sub")).unwrap();
        std::fs::write(ws.path().join("sub/b.txt"), b"hello").unwrap();

        let s1 = scan_root(&store, "p1", ws.path()).unwrap();
        assert_eq!(s1.new_dirty, 2, "two files dirty after first scan");
        assert_eq!(s1.dirs_seen, 1);

        // before the engine runs, rows are still Dirty — the scan must keep them dirty
        // (idempotent re-entry, I2), NOT flip them to synced
        let s2 = scan_root(&store, "p1", ws.path()).unwrap();
        assert_eq!(s2.new_dirty, 0);
        assert_eq!(s2.redirtied, 2, "dirty rows stay dirty until pushed");

        // engine pushes: rows become synced → scan now skips them
        store.set_file_state("p1", "a.mov", "synced").unwrap();
        store.set_file_state("p1", "sub/b.txt", "synced").unwrap();
        let s3 = scan_root(&store, "p1", ws.path()).unwrap();
        assert_eq!(s3.skipped_unchanged, 2);

        // content change → re-dirtied exactly once
        std::fs::write(ws.path().join("a.mov"), b"CHANGED").unwrap();
        let s4 = scan_root(&store, "p1", ws.path()).unwrap();
        assert_eq!(s4.redirtied, 1);
        assert_eq!(s4.skipped_unchanged, 1);
    }

    #[test]
    fn scan_ignores_ignore_list_and_records_symlinks() {
        let (_h, store, ws) = setup();
        std::fs::write(ws.path().join(".cairn-tmp"), b"nope").unwrap();
        std::fs::write(ws.path().join("real.mov"), b"x").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("real.mov", ws.path().join("link.mov")).unwrap();

        let s = scan_root(&store, "p1", ws.path()).unwrap();
        assert!(store.get_file("p1", ".cairn-tmp").is_none(), "ignored");
        assert!(store.get_file("p1", "real.mov").is_some());
        #[cfg(unix)]
        {
            let link = store.get_file("p1", "link.mov").unwrap();
            assert_eq!(link.mode, "symlink");
            assert_eq!(s.files_seen, 2);
        }
    }

    #[test]
    fn nfc_paths_are_stable() {
        let (_h, store, ws) = setup();
        // decomposed é (U+0065 U+0301) must land as composed é (NFC)
        let decomposed = "caf\u{0065}\u{0301}.txt";
        std::fs::write(ws.path().join("cafe\u{0301}.txt"), b"z").unwrap();
        let _ = scan_root(&store, "p1", ws.path()).unwrap();
        let composed = nfc_normalize(decomposed);
        assert!(store.get_file("p1", &composed).is_some());
    }

    // ---- reconcile_sweep (punch #5) -------------------------------------------

    use cairn_core::chunker::StreamHash;
    use cairn_core::manifest::{Manifest, ManifestEntry};

    /// Build + store the manifest the engine would build for `bytes` (plain content,
    /// no compression), returning its hash. Mirrors engine::process_file entry mapping.
    fn store_manifest_for(cas: &Cas, path: &std::path::Path) -> String {
        let bytes = std::fs::read(path).unwrap();
        let sh = StreamHash::compute(&bytes);
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
        let m = Manifest::build(entries, cairn_core::manifest::Compression::None, None);
        let (h, ser) = m.serialize();
        cas.put(&h, &ser).unwrap();
        h.hex()
    }

    #[test]
    fn sweep_stat_pass_redirties_size_or_mtime_drift() {
        let (_h, store, ws) = setup();
        let f = ws.path().join("a.mov");
        std::fs::write(&f, b"0123456789").unwrap();
        let cas = Cas::open(&store.root().join("blobs"), store.conn_handle()).unwrap();
        let mh = store_manifest_for(&cas, &f);
        let meta = std::fs::metadata(&f).unwrap();
        store
            .put_file(&FileRow {
                path: "a.mov".into(),
                project_id: "p1".into(),
                manifest_hash: Some(mh),
                size: meta.len(),
                mode: "file".into(),
                mtime: mtime_millis(&meta),
                local_state: "synced".into(),
            })
            .unwrap();

        // watcher missed an append (size+mtime both changed)
        std::fs::write(&f, b"0123456789 appended").unwrap();
        let out = reconcile_sweep(&store, "p1", ws.path(), 0, 8, u64::MAX).unwrap();
        assert_eq!(out.stat_redirtied, 1, "stat drift must re-dirty");
        assert_eq!(
            out.rehash_dirty, 0,
            "divergent file is skipped (already dirty)"
        );
    }

    #[test]
    fn sweep_rehash_catches_size_and_mtime_preserving_edit() {
        let (_h, store, ws) = setup();
        let f = ws.path().join("lut.cube");
        std::fs::write(&f, b"RED 1.0 0.0 0.0\nGREEN 0.0 1.0 0.0\n").unwrap();
        let cas = Cas::open(&store.root().join("blobs"), store.conn_handle()).unwrap();
        let mh = store_manifest_for(&cas, &f);
        let meta = std::fs::metadata(&f).unwrap();
        let journaled_mtime = mtime_millis(&meta);
        store
            .put_file(&FileRow {
                path: "lut.cube".into(),
                project_id: "p1".into(),
                manifest_hash: Some(mh),
                size: meta.len(),
                mode: "file".into(),
                mtime: journaled_mtime,
                local_state: "synced".into(),
            })
            .unwrap();

        // in-place byte flip, SAME size, mtime RESTORED — no stat signal at all
        std::fs::write(&f, b"RED 0.0 1.0 0.0\nGREEN 1.0 0.0 0.0\n").unwrap();
        filetime::set_file_mtime(
            &f,
            filetime::FileTime::from_unix_time(
                journaled_mtime.div_euclid(1000),
                u32::try_from(journaled_mtime.rem_euclid(1000)).unwrap_or(0) * 1_000_000,
            ),
        )
        .unwrap();

        let out = reconcile_sweep(&store, "p1", ws.path(), 0, 8, u64::MAX).unwrap();
        assert_eq!(
            out.stat_redirtied, 0,
            "no stat signal — stat pass must stay quiet"
        );
        assert_eq!(out.rehashed, 1);
        assert_eq!(
            out.rehash_dirty, 1,
            "THE silent-divergence catch (punch #5)"
        );
        // the row is dirty again → the engine will re-push it on the next pass
        let row = store.get_file("p1", "lut.cube").unwrap();
        assert_eq!(row.local_state, "dirty");
    }

    #[test]
    fn sweep_clean_file_reports_no_divergence() {
        let (_h, store, ws) = setup();
        let f = ws.path().join("ok.mov");
        std::fs::write(&f, b"stable content").unwrap();
        let cas = Cas::open(&store.root().join("blobs"), store.conn_handle()).unwrap();
        let mh = store_manifest_for(&cas, &f);
        let meta = std::fs::metadata(&f).unwrap();
        store
            .put_file(&FileRow {
                path: "ok.mov".into(),
                project_id: "p1".into(),
                manifest_hash: Some(mh),
                size: meta.len(),
                mode: "file".into(),
                mtime: mtime_millis(&meta),
                local_state: "synced".into(),
            })
            .unwrap();
        let out = reconcile_sweep(&store, "p1", ws.path(), 0, 8, u64::MAX).unwrap();
        assert_eq!(out.rehashed, 1);
        assert_eq!(out.rehash_dirty, 0);
        assert!(!out.budget_exhausted);
    }

    #[test]
    fn sweep_respects_byte_budget_and_reports_exhaustion() {
        let (_h, store, ws) = setup();
        let f = ws.path().join("big.mov");
        std::fs::write(&f, vec![7u8; 4096]).unwrap();
        let cas = Cas::open(&store.root().join("blobs"), store.conn_handle()).unwrap();
        let mh = store_manifest_for(&cas, &f);
        let meta = std::fs::metadata(&f).unwrap();
        store
            .put_file(&FileRow {
                path: "big.mov".into(),
                project_id: "p1".into(),
                manifest_hash: Some(mh),
                size: meta.len(),
                mode: "file".into(),
                mtime: mtime_millis(&meta),
                local_state: "synced".into(),
            })
            .unwrap();
        let out = reconcile_sweep(&store, "p1", ws.path(), 0, 8, 1024).unwrap();
        assert!(out.budget_exhausted, "4KiB file vs 1KiB budget");
        assert_eq!(
            out.rehashed, 0,
            "file did not fit the budget — never rehashed"
        );
    }

    #[test]
    fn sweep_rotation_covers_all_files_over_counters() {
        let (_h, store, ws) = setup();
        for name in ["a", "b", "c"] {
            let f = ws.path().join(format!("{name}.bin"));
            std::fs::write(&f, format!("content-{name}-aaaaaaaaaa")).unwrap();
        }
        let cas = Cas::open(&store.root().join("blobs"), store.conn_handle()).unwrap();
        for name in ["a", "b", "c"] {
            let f = ws.path().join(format!("{name}.bin"));
            let mh = store_manifest_for(&cas, &f);
            let meta = std::fs::metadata(&f).unwrap();
            store
                .put_file(&FileRow {
                    path: format!("{name}.bin"),
                    project_id: "p1".into(),
                    manifest_hash: Some(mh),
                    size: meta.len(),
                    mode: "file".into(),
                    mtime: mtime_millis(&meta),
                    local_state: "synced".into(),
                })
                .unwrap();
        }
        // corrupt ONE file in place (byte flip, same size), restore its mtime — the
        // rotating window must find it within three sweeps (window = 1 file per sweep)
        let f = ws.path().join("b.bin");
        let mut corrupted = std::fs::read(&f).unwrap();
        corrupted[0] ^= 0xFF;
        std::fs::write(&f, &corrupted).unwrap();
        // restore b.bin's journaled mtime (it was re-written → mtime bumped)
        let row = store.get_file("p1", "b.bin").unwrap();
        filetime::set_file_mtime(
            &f,
            filetime::FileTime::from_unix_time(
                row.mtime.div_euclid(1000),
                u32::try_from(row.mtime.rem_euclid(1000)).unwrap_or(0) * 1_000_000,
            ),
        )
        .unwrap();
        let mut caught = 0u64;
        for counter in 0..3u64 {
            let out = reconcile_sweep(&store, "p1", ws.path(), counter, 1, u64::MAX).unwrap();
            caught += out.rehash_dirty;
        }
        assert_eq!(caught, 1, "rotating window must eventually sample b.bin");
    }
}
