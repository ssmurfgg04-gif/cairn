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
            // W5 guard (round 13, caught LIVE by the two-device matrix on the
            // plain-file path): a locally-CLEAN row whose DISK stat has drifted
            // from the row's recorded stat is an UNDISCOVERED local edit.
            // Edit-discovery lag is the window: plain-file workspaces discover
            // local edits only at the periodic scan (CfAPI/FUSE surfaces get
            // close/flush notifications and close it). Taking the remote
            // manifest as authoritative here would let the materializer
            // OVERWRITE those bytes -- silent local data loss, no conflict
            // copy. Instead apply the scan's EXACT predicate at pull time:
            // re-dirty the row, let the push side hash the real bytes, and let
            // the server's append-time conflict rule (SPEC §7.1) produce the
            // conflict copy that preserves BOTH versions.
            if let Some(existing) = store.get_file(project_id, &u.path) {
                let locally_clean_and_stale = existing.mode == "file"
                    && existing.manifest_hash.as_deref() != Some(u.manifest_hash.as_str())
                    && matches!(
                        LocalState::parse(&existing.local_state),
                        Some(LocalState::Synced)
                            | Some(LocalState::Clean)
                            | Some(LocalState::Pinned)
                    );
                if locally_clean_and_stale {
                    let target = crate::workspace::workspace_dir(store, project_id).join(&u.path);
                    if let Ok(st) = std::fs::metadata(&target) {
                        let drifted = st.len() != existing.size
                            || crate::scan::mtime_millis(&st) != existing.mtime;
                        if drifted {
                            store.set_file_state(
                                project_id,
                                &u.path,
                                LocalState::Dirty.as_str(),
                            )?;
                            mark_fork(store, project_id, &u.path, entry.seq)?;
                            tracing::warn!(
                                path = %u.path,
                                "remote update for a locally-clean row whose disk stat \
                                 drifted: undiscovered local edit re-dirtied (conflict \
                                 path, SPEC 7.1); remote overwrite refused"
                            );
                            return Ok(());
                        }
                    }
                }
            }
            // Local dirty rows keep their state: local edits win locally and the server's
            // conflict rule resolves at append time (§7.1). Otherwise the remote manifest
            // becomes authoritative: a fresh row or a stale local copy is a PLACEHOLDER
            // that the hydrator materializes (missing or overwritten on disk).
            // Row-stat provenance (round 18, the W4 catch): `mtime` is
            // informational for rows that have never touched this disk
            // (fresh placeholders, renames), but the round-13 §7.1 guard
            // COMPARES it against the live disk stat -- so any arm that
            // describes content this device already has on disk MUST carry
            // the row's own recorded stat, not the server's ts/size. The
            // shared-put below used to clobber `mtime` with `server_ts` on
            // every apply: replaying our OWN upsert (same manifest, no-op)
            // replaced the row's push-time disk stat with the server
            // timestamp, and the next remote-differing upsert then read as
            // "stat drifted" -- a spurious undiscovered-local-edit that
            // refused the remote and diverged forever (Windows-matrix W4,
            // timing-dependent: fires only when the remote lands before the
            // next scan re-records the stat).
            let mut takes_remote = true;
            let (local_state, row_size, row_mtime) = match store.get_file(project_id, &u.path) {
                Some(e)
                    if matches!(
                        LocalState::parse(&e.local_state),
                        Some(LocalState::Dirty) | Some(LocalState::Conflict)
                    ) =>
                {
                    // Fork bookkeeping: the local bytes fork from the PRE-refusal
                    // lineage, so the eventual append must claim THAT base, not
                    // the read cursor (see mark_fork / engine process_file).
                    // Identical content is NOT a fork (dedup: another device
                    // upserted the same bytes -- no divergence to declare).
                    if e.manifest_hash.as_deref() != Some(u.manifest_hash.as_str()) {
                        mark_fork(store, project_id, &u.path, entry.seq)?;
                        takes_remote = false;
                    }
                    (e.local_state.clone(), e.size, e.mtime)
                }
                Some(e) if e.manifest_hash.as_deref() == Some(u.manifest_hash.as_str()) => {
                    // no-op replay (own entry, or identical bytes from a peer):
                    // keep the row's OWN stat -- it is the local truth the §7.1
                    // guard compares; a server_ts here fakes a drift
                    (e.local_state.clone(), e.size, e.mtime)
                }
                // fresh or stale-local row: the remote manifest is authoritative;
                // size/mtime stay informational until materialization records
                // the real disk stat (I4)
                _ => (
                    LocalState::Placeholder.as_str().into(),
                    u.size,
                    entry.server_ts,
                ),
            };
            if takes_remote {
                // the row (and, after materialization, the disk) now descends from
                // the remote -- any earlier fork is consumed/resolved
                clear_fork(store, project_id, &u.path)?;
            }
            store.put_file(&FileRow {
                path: u.path.clone(),
                project_id: project_id.into(),
                manifest_hash: Some(u.manifest_hash.clone()),
                size: row_size,
                mode: "file".into(),
                mtime: row_mtime,
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

// ---------- content-lineage fork markers (round 13, the W5 catch) ----------
//
// base_seq for an append must declare what the local BYTES descend from, not
// what the device has READ. When apply refuses a remote upsert (dirty-keep or
// the undiscovered-local-edit guard), the local content forks at the
// PRE-refusal head -- without a marker, the later append carries the read
// cursor (past the refused entry), the server accepts it linearly, and the
// other device's version silently loses head status: NO conflict copy, the
// H9 contract broken. The marker pins the fork seq; engine::process_file
// claims base = min(cursor, fork-1); the server's seq>base conflict rule then
// fires exactly as designed (SPEC 7.1).

fn fork_key(project_id: &str, path: &str) -> String {
    format!("fork:{project_id}:{path}")
}

/// Pin (or keep the earliest) fork point for a path whose local content does
/// not descend from the remote upsert at `seq`.
fn mark_fork(store: &Store, project_id: &str, path: &str, seq: u64) -> Result<(), CairnError> {
    let key = fork_key(project_id, path);
    if let Some(prev) = store.meta_get(&key) {
        if let Ok(p) = prev.parse::<u64>() {
            if p <= seq {
                return Ok(()); // an earlier fork point already pins the lineage
            }
        }
    }
    store.meta_set(&key, &seq.to_string())
}

/// The fork point for a path, if the local content is forked (None = the
/// cursor is the honest base).
pub fn fork_seq(store: &Store, project_id: &str, path: &str) -> Option<u64> {
    store.meta_get(&fork_key(project_id, path))?.parse().ok()
}

/// Consume a fork: the path's local state now descends from the remote again.
pub fn clear_fork(store: &Store, project_id: &str, path: &str) -> Result<(), CairnError> {
    store.meta_clear(&fork_key(project_id, path))
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

    // ---------- W5 guard (round 13): undiscovered-local-edit at pull time ----------
    //
    // The two-device matrix caught this live: on plain-file workspaces local
    // edits are discovered by the periodic scan, so a remote update can arrive
    // while the row still says clean -- the materializer would then overwrite
    // the local bytes with NO conflict copy (silent data loss). The guard
    // re-dirties the row instead, handing the divergence to the conflict rule.

    fn upsert(path: &str, manifest: &str, size: u64) -> cairn_proto::pb::JournalOp {
        cairn_proto::pb::JournalOp {
            op: Some(OpKind::FileUpsert(FileUpsertOp {
                path: path.into(),
                manifest_hash: manifest.into(),
                size,
                base_seq: 0,
            })),
        }
    }

    #[test]
    fn remote_update_over_undiscovered_local_edit_redirties_not_clobbers() {
        let ws = tempfile::tempdir().unwrap();
        let store = Store::open(ws.path(), Arc::new(WallClock)).unwrap();
        crate::workspace::set_workspace(&store, "p1", ws.path()).unwrap();
        let f = ws.path().join("probe.txt");
        std::fs::write(&f, b"seed").unwrap();
        let st = std::fs::metadata(&f).unwrap();
        store
            .put_file(&FileRow {
                path: "probe.txt".into(),
                project_id: "p1".into(),
                manifest_hash: Some("v1".into()),
                size: st.len(),
                mode: "file".into(),
                mtime: crate::scan::mtime_millis(&st),
                local_state: LocalState::Synced.as_str().into(),
            })
            .unwrap();
        // the UNDISCOVERED local edit: disk changes, no scan runs
        std::fs::write(&f, b"seed+local-edit").unwrap();
        // push the mtime past the row's recorded value (a same-second write
        // could collide at millisecond granularity)
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(2);
        std::fs::File::options()
            .append(true)
            .open(&f)
            .unwrap()
            .set_modified(later)
            .unwrap();

        apply_entry(&store, "p1", "me", &entry(upsert("probe.txt", "v2", 15), 5)).unwrap();

        let row = store.get_file("p1", "probe.txt").unwrap();
        assert_eq!(row.local_state, LocalState::Dirty.as_str());
        // identity preserved at the LOCAL version: the push side re-hashes the
        // real bytes; the server's append-time conflict rule does the rest
        assert_eq!(row.manifest_hash.as_deref(), Some("v1"));
        // and the bytes on disk were NOT clobbered by the pull
        assert_eq!(std::fs::read(&f).unwrap(), b"seed+local-edit");
    }

    #[test]
    fn remote_update_over_genuinely_stale_copy_stays_placeholder() {
        let ws = tempfile::tempdir().unwrap();
        let store = Store::open(ws.path(), Arc::new(WallClock)).unwrap();
        crate::workspace::set_workspace(&store, "p1", ws.path()).unwrap();
        let f = ws.path().join("probe.txt");
        std::fs::write(&f, b"seed").unwrap();
        let st = std::fs::metadata(&f).unwrap();
        store
            .put_file(&FileRow {
                path: "probe.txt".into(),
                project_id: "p1".into(),
                manifest_hash: Some("v1".into()),
                size: st.len(),
                mode: "file".into(),
                mtime: crate::scan::mtime_millis(&st),
                local_state: LocalState::Synced.as_str().into(),
            })
            .unwrap();
        // NO local edit: disk stat matches the row exactly
        apply_entry(&store, "p1", "me", &entry(upsert("probe.txt", "v2", 10), 5)).unwrap();
        let row = store.get_file("p1", "probe.txt").unwrap();
        assert_eq!(row.local_state, LocalState::Placeholder.as_str());
        assert_eq!(row.manifest_hash.as_deref(), Some("v2"));
    }

    #[test]
    fn remote_update_over_missing_file_stays_placeholder() {
        let ws = tempfile::tempdir().unwrap();
        let store = Store::open(ws.path(), Arc::new(WallClock)).unwrap();
        crate::workspace::set_workspace(&store, "p1", ws.path()).unwrap();
        // row exists, file deleted locally but scan has not tombstoned it yet
        store
            .put_file(&FileRow {
                path: "gone.txt".into(),
                project_id: "p1".into(),
                manifest_hash: Some("v1".into()),
                size: 4,
                mode: "file".into(),
                mtime: 1,
                local_state: LocalState::Synced.as_str().into(),
            })
            .unwrap();
        apply_entry(&store, "p1", "me", &entry(upsert("gone.txt", "v2", 8), 5)).unwrap();
        let row = store.get_file("p1", "gone.txt").unwrap();
        assert_eq!(row.local_state, LocalState::Placeholder.as_str());
        assert_eq!(row.manifest_hash.as_deref(), Some("v2"));
    }

    // ---------- row-stat provenance (round 18, the Windows-matrix W4 catch) ----------
    //
    // The pull phase skips OWN-device ops (see own_op_livelock), but a REMOTE
    // same-manifest upsert -- identical bytes re-asserted by a peer (the
    // re-materialize + re-append shape a cold re-attach produces) -- still
    // goes through apply_entry. The shared put_file used to write
    // mtime=server_ts there, clobbering the row's push-time DISK stat. The
    // very next remote-differing upsert then read as "stat drifted" at the
    // §7.1 guard: a spurious undiscovered-local-edit that refused the remote
    // and diverged silently (A held v1 forever, B held v2, no conflict copy).

    #[test]
    fn remote_same_manifest_replay_preserves_row_stat() {
        let ws = tempfile::tempdir().unwrap();
        let store = Store::open(ws.path(), Arc::new(WallClock)).unwrap();
        crate::workspace::set_workspace(&store, "p1", ws.path()).unwrap();
        let f = ws.path().join("probe.txt");
        std::fs::write(&f, b"seed").unwrap();
        let st = std::fs::metadata(&f).unwrap();
        let disk_mtime = crate::scan::mtime_millis(&st);
        store
            .put_file(&FileRow {
                path: "probe.txt".into(),
                project_id: "p1".into(),
                manifest_hash: Some("v1".into()),
                size: st.len(),
                mode: "file".into(),
                mtime: disk_mtime,
                local_state: LocalState::Synced.as_str().into(),
            })
            .unwrap();

        // a peer re-asserts the SAME bytes (same manifest): a no-op content-wise,
        // carrying a fresh server_ts that must NOT be written into the row
        apply_entry(&store, "p1", "me", &entry(upsert("probe.txt", "v1", 4), 5)).unwrap();

        let row = store.get_file("p1", "probe.txt").unwrap();
        assert_eq!(
            row.mtime, disk_mtime,
            "no-op replay must keep the disk stat"
        );
        assert_eq!(row.size, st.len());
        assert_eq!(row.local_state, LocalState::Synced.as_str());
        assert_eq!(row.manifest_hash.as_deref(), Some("v1"));

        // THE W4 SHAPE: the next remote-differing upsert must apply cleanly
        // (no spurious drift -> no refusal -> the row takes the remote head)
        apply_entry(&store, "p1", "me", &entry(upsert("probe.txt", "v2", 9), 6)).unwrap();
        let row = store.get_file("p1", "probe.txt").unwrap();
        assert_eq!(
            row.local_state,
            LocalState::Placeholder.as_str(),
            "the guard must NOT fire on a stat the replay preserved"
        );
        assert_eq!(row.manifest_hash.as_deref(), Some("v2"));
    }
}
