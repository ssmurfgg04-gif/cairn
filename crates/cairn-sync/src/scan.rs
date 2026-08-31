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
use cairn_store::{FileRow, Store};

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

fn mtime_millis(meta: &std::fs::Metadata) -> i64 {
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
}
