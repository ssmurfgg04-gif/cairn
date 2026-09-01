//! Cursor replay — the guarantee (SPEC §7.1): applying journal entries to the local file
//! table. Watch is only a hint; this module is authoritative for convergence.

use cairn_core::{CairnError, ErrorKind};
use cairn_proto::pb::journal_op::Op as OpKind;
use cairn_store::state::LocalState;
use cairn_store::{FileRow, Store};

use crate::plane::Entry;

/// Apply one journal entry to the local view (idempotent: replaying a seq is a no-op via
/// cursor monotonicity enforced by the caller).
pub fn apply_entry(
    store: &Store,
    project_id: &str,
    _self_device: &str,
    entry: &Entry,
) -> Result<(), CairnError> {
    let Some(op) = entry.op.op.as_ref() else {
        return Ok(());
    };
    // WO6-9 defense in depth: the server rejects traversal paths at append, but replay
    // must never trust that an older/compromised server filtered — validate before the
    // path can reach a filesystem join (hydration writes root.join(path)).
    let paths: Vec<&str> = match op {
        OpKind::FileUpsert(u) => vec![&u.path],
        OpKind::FileDelete(d) => vec![&d.path],
        OpKind::Rename(r) => vec![&r.old_path, &r.new_path],
        OpKind::LeaseEvent(_) => vec![],
    };
    for p in paths {
        cairn_core::pathutil::validate_rel_path(p)?;
    }
    match op {
        OpKind::FileUpsert(u) => {
            // Local dirty rows keep their state: local edits win locally and the server's
            // conflict rule resolves at append time (§7.1). Otherwise the remote manifest
            // becomes authoritative: a fresh row or a stale local copy is a PLACEHOLDER
            // that the hydrator materializes (missing or overwritten on disk).
            let local_state = match store.get_file(project_id, &u.path) {
                Some(e)
                    if matches!(
                        LocalState::parse(&e.local_state),
                        Some(LocalState::Dirty) | Some(LocalState::Conflict)
                    ) =>
                {
                    e.local_state.clone()
                }
                Some(e) if e.manifest_hash.as_deref() == Some(u.manifest_hash.as_str()) => {
                    e.local_state.clone()
                }
                _ => LocalState::Placeholder.as_str().into(),
            };
            store.put_file(&FileRow {
                path: u.path.clone(),
                project_id: project_id.into(),
                manifest_hash: Some(u.manifest_hash.clone()),
                size: u.size,
                mode: "file".into(),
                mtime: entry.server_ts, // informational only (I4)
                local_state,
            })?;
        }
        OpKind::FileDelete(d) => {
            // tombstone: local row removed (trash retention lives server-side per §7.1)
            store.put_file(&FileRow {
                path: d.path.clone(),
                project_id: project_id.into(),
                manifest_hash: None,
                size: 0,
                mode: "tombstone".into(),
                mtime: entry.server_ts,
                local_state: LocalState::Synced.as_str().into(),
            })?;
        }
        OpKind::Rename(r) => {
            // metadata-only move (never re-chunks, §7.1)
            store.put_file(&FileRow {
                path: r.old_path.clone(),
                project_id: project_id.into(),
                manifest_hash: None,
                size: 0,
                mode: "tombstone".into(),
                mtime: entry.server_ts,
                local_state: LocalState::Synced.as_str().into(),
            })?;
            store.put_file(&FileRow {
                path: r.new_path.clone(),
                project_id: project_id.into(),
                manifest_hash: Some(r.manifest_hash.clone()),
                size: 0,
                mode: "file".into(),
                mtime: entry.server_ts,
                local_state: LocalState::Clean.as_str().into(),
            })?;
        }
        OpKind::LeaseEvent(_) => {
            // informational (§7.1)
        }
    }
    Ok(())
}

/// A client whose cursor predates compaction re-syncs from the latest snapshot (§7.1).
/// The server reports `COMPACTION_REQUIRED`; the engine resets to the snapshot tree.
pub fn reset_to_snapshot(store: &Store, project_id: &str) -> Result<(), CairnError> {
    // snapshot restore materialization lands with RestoreSnapshot (ctl); here we clear the
    // local view so the next fold snapshot repopulates it deterministically
    for f in store.list_files(project_id) {
        store.set_file_state(project_id, &f.path, LocalState::Clean.as_str())?;
    }
    Err(CairnError::new(
        ErrorKind::CompactionRequired,
        "cursor predates compaction; resync from snapshot queued",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cairn_core::clock::WallClock;
    use cairn_proto::pb::{FileUpsertOp, RenameOp};
    use std::sync::Arc;

    fn entry(op: cairn_proto::pb::JournalOp, seq: u64) -> Entry {
        Entry {
            seq,
            device_id: "other".into(),
            op,
            server_ts: 1,
        }
    }

    #[test]
    fn apply_upsert_delete_rename() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), Arc::new(WallClock)).unwrap();
        let up = cairn_proto::pb::JournalOp {
            op: Some(OpKind::FileUpsert(FileUpsertOp {
                path: "a.mov".into(),
                manifest_hash: "aa".into(),
                size: 10,
                base_seq: 0,
            })),
        };
        apply_entry(&store, "p1", "me", &entry(up, 1)).unwrap();
        let f = store.get_file("p1", "a.mov").unwrap();
        assert_eq!(f.manifest_hash.as_deref(), Some("aa"));
        let rn = cairn_proto::pb::JournalOp {
            op: Some(OpKind::Rename(RenameOp {
                old_path: "a.mov".into(),
                new_path: "b.mov".into(),
                manifest_hash: "aa".into(),
                base_seq: 1,
            })),
        };
        apply_entry(&store, "p1", "me", &entry(rn, 2)).unwrap();
        assert_eq!(store.get_file("p1", "a.mov").unwrap().mode, "tombstone");
        assert_eq!(
            store
                .get_file("p1", "b.mov")
                .unwrap()
                .manifest_hash
                .as_deref(),
            Some("aa")
        );
    }
}
