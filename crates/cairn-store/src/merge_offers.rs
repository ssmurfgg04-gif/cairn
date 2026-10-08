//! Pending semantic-merge offers (CONTRACT-DEBT #1): the durable local
//! record that a timeline conflict has an AUTO-MERGEABLE resolution waiting
//! for a human decision.
//!
//! DUMB STORAGE layer, same discipline as `state_records`: pure CRUD over
//! the `merge_offers` table (migration v5). Every decision — whether an
//! offer is created, what accepting means — lives in ONE place:
//! `cairn_sync::merge_offer`. One offer per (project, path); the row keeps
//! the manifest hashes needed to RECOMPUTE the merge at accept time (the
//! three-way merge is byte-deterministic, so merged bytes are never stored)
//! plus the conflict-copy path whose row is removed on accept.

use cairn_core::{CairnError, ErrorKind};
use rusqlite::OptionalExtension;

use crate::db::Store;

/// One pending merge offer (mirrors `merge_offers`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeOfferRow {
    /// Local store namespace the conflicted file belongs to (same key the
    /// `files` rows use — the plain project id for the default root).
    pub project_id: String,
    /// The ORIGINAL journal path the conflict was raised on.
    pub path: String,
    /// Where this device's local content was moved (the conflict copy).
    pub copy_path: String,
    /// Manifest hash of the common ancestor (the base_seq entry's content).
    pub base_manifest: String,
    /// Manifest hash of the remote write that won the append race.
    pub theirs_manifest: String,
    /// Machine-readable merge report at offer time (`MergeReport::to_json`).
    pub report_json: String,
    /// Store clock at offer creation.
    pub created_ms: i64,
}

fn db_err(e: rusqlite::Error) -> CairnError {
    CairnError::new(ErrorKind::Io, format!("merge_offers: {e}"))
}

impl Store {
    /// INSERT OR REPLACE the offer for (project, path). One pending offer per
    /// path — a second conflict on the same path replaces the stale row.
    pub fn upsert_merge_offer(&self, row: &MergeOfferRow) -> Result<(), CairnError> {
        let handle = self.conn_handle();
        let conn = handle.lock().expect("store poisoned");
        conn.execute(
            "INSERT OR REPLACE INTO merge_offers(project_id, path, copy_path, base_manifest, theirs_manifest, report_json, created_ms)
             VALUES(?1,?2,?3,?4,?5,?6,?7)",
            rusqlite::params![
                row.project_id,
                row.path,
                row.copy_path,
                row.base_manifest,
                row.theirs_manifest,
                row.report_json,
                row.created_ms,
            ],
        )
        .map_err(db_err)?;
        Ok(())
    }

    /// Fetch the pending offer for one path.
    #[must_use]
    pub fn get_merge_offer(&self, project_id: &str, path: &str) -> Option<MergeOfferRow> {
        let handle = self.conn_handle();
        let conn = handle.lock().expect("store poisoned");
        conn.query_row(
            "SELECT project_id, path, copy_path, base_manifest, theirs_manifest, report_json, created_ms
             FROM merge_offers WHERE project_id=?1 AND path=?2",
            rusqlite::params![project_id, path],
            row_of,
        )
        .optional()
        .ok()
        .flatten()
    }

    /// All pending offers for a project, oldest first. The Files view joins
    /// this against the file rows for the `merge_available` badge.
    #[must_use]
    pub fn list_merge_offers(&self, project_id: &str) -> Vec<MergeOfferRow> {
        let handle = self.conn_handle();
        let conn = handle.lock().expect("store poisoned");
        let mut stmt = match conn.prepare(
            "SELECT project_id, path, copy_path, base_manifest, theirs_manifest, report_json, created_ms
             FROM merge_offers WHERE project_id=?1
             ORDER BY created_ms ASC, path ASC",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        stmt.query_map(rusqlite::params![project_id], row_of)
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default()
    }

    /// Remove the offer (accept: the merge landed; decline: the copy stays,
    /// only the affordance goes). Deleting an absent offer is a no-op.
    pub fn delete_merge_offer(&self, project_id: &str, path: &str) -> Result<(), CairnError> {
        let handle = self.conn_handle();
        let conn = handle.lock().expect("store poisoned");
        conn.execute(
            "DELETE FROM merge_offers WHERE project_id=?1 AND path=?2",
            rusqlite::params![project_id, path],
        )
        .map_err(db_err)?;
        Ok(())
    }
}

fn row_of(r: &rusqlite::Row<'_>) -> rusqlite::Result<MergeOfferRow> {
    Ok(MergeOfferRow {
        project_id: r.get(0)?,
        path: r.get(1)?,
        copy_path: r.get(2)?,
        base_manifest: r.get(3)?,
        theirs_manifest: r.get(4)?,
        report_json: r.get(5)?,
        created_ms: r.get(6)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use cairn_core::clock::WallClock;
    use std::sync::Arc;

    fn open() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path(), Arc::new(WallClock)).unwrap();
        (dir, s)
    }

    fn row(path: &str, copy: &str, base: &str, theirs: &str, created: i64) -> MergeOfferRow {
        MergeOfferRow {
            project_id: "p1".into(),
            path: path.into(),
            copy_path: copy.into(),
            base_manifest: base.into(),
            theirs_manifest: theirs.into(),
            report_json: format!(r#"{{"outcome":"clean","path":"{path}"}}"#),
            created_ms: created,
        }
    }

    #[test]
    fn upsert_get_and_replace_by_path() {
        let (_d, s) = open();
        s.upsert_merge_offer(&row("a.otio", "a (conflict).otio", "aa", "bb", 100))
            .unwrap();
        let g = s.get_merge_offer("p1", "a.otio").unwrap();
        assert_eq!(g.copy_path, "a (conflict).otio");
        assert_eq!(g.base_manifest, "aa");
        assert_eq!(g.theirs_manifest, "bb");
        assert_eq!(g.created_ms, 100);

        // a second conflict on the same path replaces the stale offer
        s.upsert_merge_offer(&row("a.otio", "a (conflict).otio", "cc", "dd", 200))
            .unwrap();
        let g = s.get_merge_offer("p1", "a.otio").unwrap();
        assert_eq!((g.base_manifest.as_str(), g.created_ms), ("cc", 200));
    }

    #[test]
    fn list_is_scoped_by_project_and_sorted() {
        let (_d, s) = open();
        s.upsert_merge_offer(&row("b.otio", "b (c).otio", "x", "y", 200))
            .unwrap();
        s.upsert_merge_offer(&row("a.otio", "a (c).otio", "x", "y", 100))
            .unwrap();
        s.upsert_merge_offer(&MergeOfferRow {
            project_id: "p2".into(),
            path: "c.otio".into(),
            copy_path: "c (c).otio".into(),
            base_manifest: "x".into(),
            theirs_manifest: "y".into(),
            report_json: "{}".into(),
            created_ms: 50,
        })
        .unwrap();

        let offers = s.list_merge_offers("p1");
        assert_eq!(offers.len(), 2, "project scope");
        assert_eq!(offers[0].path, "a.otio", "oldest first");
        assert_eq!(offers[1].path, "b.otio");
        assert_eq!(s.list_merge_offers("p2").len(), 1);
        assert_eq!(s.list_merge_offers("p3").len(), 0);
    }

    #[test]
    fn get_missing_is_none_and_delete_is_idempotent() {
        let (_d, s) = open();
        s.upsert_merge_offer(&row("a.otio", "a (c).otio", "aa", "bb", 1))
            .unwrap();
        assert!(s.get_merge_offer("p1", "other.otio").is_none());
        assert!(s.get_merge_offer("p9", "a.otio").is_none());
        s.delete_merge_offer("p1", "a.otio").unwrap();
        assert!(s.get_merge_offer("p1", "a.otio").is_none());
        // deleting an absent offer stays a no-op, never an error
        s.delete_merge_offer("p1", "a.otio").unwrap();
    }

    /// Migration v4 -> v5: a store that predates `merge_offers` reopens with
    /// the table created and usable (the exact upgrade an existing beta
    /// device takes when the daemon ships this feature).
    #[test]
    fn migration_v4_to_v5_reopens_and_serves_offers() {
        let dir = tempfile::tempdir().unwrap();
        {
            let s = Store::open(dir.path(), Arc::new(WallClock)).unwrap();
            assert_eq!(s.schema_version().unwrap(), 5);
            // rewind to a v4-shaped database: drop the table, pin the version
            let handle = s.conn_handle();
            let conn = handle.lock().expect("store poisoned");
            conn.execute("DROP TABLE merge_offers", []).unwrap();
            conn.pragma_update(None, "user_version", 4).unwrap();
            drop(conn);
        }
        let s = Store::open(dir.path(), Arc::new(WallClock)).unwrap();
        assert_eq!(s.schema_version().unwrap(), 5, "migration re-applied");
        assert!(s.get_merge_offer("p1", "a.otio").is_none());
        s.upsert_merge_offer(&row("a.otio", "a (c).otio", "aa", "bb", 7))
            .unwrap();
        assert_eq!(s.get_merge_offer("p1", "a.otio").unwrap().created_ms, 7);
    }
}
