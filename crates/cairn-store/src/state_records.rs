//! Project-scoped collaboration records (ADR-0031 Phase 1): the durable local
//! materialization of roster / audit / review state carried by journal
//! `state_record` ops.
//!
//! This module is the DUMB STORAGE layer — pure CRUD over the `state_records`
//! table (migration v4). The per-family MERGE semantics (LWW vs append-only
//! union, tombstones) live in ONE place: `cairn_sync::state_records::
//! apply_record_locally`, used by both the publish path (own-op suppression
//! means the journal never replays our own records back to us) and the apply
//! path (cursor replay). The store never decides merges.

use cairn_core::{CairnError, ErrorKind};
use rusqlite::OptionalExtension;

use crate::db::Store;

/// One materialized state record (mirrors `state_records`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateRecordRow {
    /// Record family ("member" | "audit" | "review_version" | "review_link" |
    /// "review_comment" — the authoritative set lives in
    /// `cairn_sync::state_records::FAMILIES`).
    pub family: String,
    /// Family-scoped record identity (LWW families: the key; append families:
    /// the content id — see cairn_sync::state_records module doc).
    pub record_id: String,
    /// Family-scoped key (device id / content id / token / version number).
    pub key: String,
    /// Writer clock (LWW timestamp).
    pub ts_ms: i64,
    /// Journal authorship of the write (LWW tie-break: higher device_id wins).
    pub device_id: String,
    /// Canonical JSON of the record.
    pub payload: Vec<u8>,
    /// True = delete/revoke marker (link revoke, member removal).
    pub tombstone: bool,
}

fn db_err(e: rusqlite::Error) -> CairnError {
    CairnError::new(ErrorKind::Io, format!("state_records: {e}"))
}

impl Store {
    /// INSERT OR REPLACE a record row verbatim (no merge). Callers that need
    /// merge semantics MUST go through `cairn_sync::state_records::
    /// apply_record_locally` — this helper exists for that module and for
    /// tests/views, not for feature code.
    pub fn upsert_state_record(
        &self,
        project_id: &str,
        r: &StateRecordRow,
    ) -> Result<(), CairnError> {
        let handle = self.conn_handle();
        let conn = handle.lock().expect("store poisoned");
        conn.execute(
            "INSERT OR REPLACE INTO state_records(project_id, family, record_id, key, ts_ms, device_id, payload, tombstone)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            rusqlite::params![
                project_id,
                r.family,
                r.record_id,
                r.key,
                r.ts_ms,
                r.device_id,
                r.payload,
                i64::from(r.tombstone),
            ],
        )
        .map_err(db_err)?;
        Ok(())
    }

    /// INSERT OR IGNORE a record row (append families: the first content id
    /// wins; replays are no-ops). Same discipline as [`Self::
    /// upsert_state_record`]: merge decisions live in cairn-sync.
    pub fn insert_state_record(
        &self,
        project_id: &str,
        r: &StateRecordRow,
    ) -> Result<(), CairnError> {
        let handle = self.conn_handle();
        let conn = handle.lock().expect("store poisoned");
        conn.execute(
            "INSERT OR IGNORE INTO state_records(project_id, family, record_id, key, ts_ms, device_id, payload, tombstone)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            rusqlite::params![
                project_id,
                r.family,
                r.record_id,
                r.key,
                r.ts_ms,
                r.device_id,
                r.payload,
                i64::from(r.tombstone),
            ],
        )
        .map_err(db_err)?;
        Ok(())
    }

    /// Fetch one record.
    #[must_use]
    pub fn get_state_record(
        &self,
        project_id: &str,
        family: &str,
        record_id: &str,
    ) -> Option<StateRecordRow> {
        let handle = self.conn_handle();
        let conn = handle.lock().expect("store poisoned");
        conn.query_row(
            "SELECT family, record_id, key, ts_ms, device_id, payload, tombstone
             FROM state_records WHERE project_id=?1 AND family=?2 AND record_id=?3",
            rusqlite::params![project_id, family, record_id],
            row_of,
        )
        .optional()
        .ok()
        .flatten()
    }

    /// All records of one family for a project, oldest-write first (ts_ms,
    /// then record_id for stability). The dashboard's merged read surfaces
    /// re-sort per their display contract (members by device_id).
    #[must_use]
    pub fn list_state_records(&self, project_id: &str, family: &str) -> Vec<StateRecordRow> {
        let handle = self.conn_handle();
        let conn = handle.lock().expect("store poisoned");
        let mut stmt = match conn.prepare(
            "SELECT family, record_id, key, ts_ms, device_id, payload, tombstone
             FROM state_records WHERE project_id=?1 AND family=?2
             ORDER BY ts_ms ASC, record_id ASC",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        stmt.query_map(rusqlite::params![project_id, family], row_of)
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default()
    }

    /// Distinct families that have records for a project (the
    /// state-records view's family picker / per-family counts).
    #[must_use]
    pub fn list_state_families(&self, project_id: &str) -> Vec<String> {
        let handle = self.conn_handle();
        let conn = handle.lock().expect("store poisoned");
        let mut stmt = match conn.prepare(
            "SELECT DISTINCT family FROM state_records WHERE project_id=?1 ORDER BY family ASC",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        stmt.query_map(rusqlite::params![project_id], |r| r.get::<_, String>(0))
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default()
    }
}

fn row_of(r: &rusqlite::Row<'_>) -> rusqlite::Result<StateRecordRow> {
    Ok(StateRecordRow {
        family: r.get(0)?,
        record_id: r.get(1)?,
        key: r.get(2)?,
        ts_ms: r.get(3)?,
        device_id: r.get(4)?,
        payload: r.get(5)?,
        tombstone: r.get::<_, i64>(6)? != 0,
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

    fn row(
        family: &str,
        record_id: &str,
        key: &str,
        ts: i64,
        dev: &str,
        tomb: bool,
    ) -> StateRecordRow {
        StateRecordRow {
            family: family.into(),
            record_id: record_id.into(),
            key: key.into(),
            ts_ms: ts,
            device_id: dev.into(),
            payload: format!(r#"{{"k":"{key}","ts":{ts}}}"#).into_bytes(),
            tombstone: tomb,
        }
    }

    #[test]
    fn upsert_get_and_replace() {
        let (_d, s) = open();
        s.upsert_state_record("p1", &row("member", "dev-1", "dev-1", 100, "dev-A", false))
            .unwrap();
        let g = s.get_state_record("p1", "member", "dev-1").unwrap();
        assert_eq!(g.key, "dev-1");
        assert_eq!(g.ts_ms, 100);
        assert!(!g.tombstone);

        // REPLACE: the LWW apply path overwrites wholesale
        s.upsert_state_record("p1", &row("member", "dev-1", "dev-1", 200, "dev-B", true))
            .unwrap();
        let g = s.get_state_record("p1", "member", "dev-1").unwrap();
        assert_eq!((g.ts_ms, g.device_id.as_str()), (200, "dev-B"));
        assert!(g.tombstone);
        assert!(String::from_utf8(g.payload).unwrap().contains("200"));
    }

    #[test]
    fn insert_ignore_keeps_first_content() {
        let (_d, s) = open();
        s.insert_state_record("p1", &row("audit", "cid-1", "cid-1", 100, "dev-A", false))
            .unwrap();
        // same record_id again (replay / union merge): the FIRST row stays
        s.insert_state_record("p1", &row("audit", "cid-1", "cid-1", 999, "dev-B", false))
            .unwrap();
        let g = s.get_state_record("p1", "audit", "cid-1").unwrap();
        assert_eq!(
            g.ts_ms, 100,
            "append families are insert-or-ignore by content id"
        );
        assert_eq!(g.device_id, "dev-A");
    }

    #[test]
    fn list_is_scoped_by_project_and_family() {
        let (_d, s) = open();
        s.upsert_state_record("p1", &row("member", "dev-2", "dev-2", 200, "dev-A", false))
            .unwrap();
        s.upsert_state_record("p1", &row("member", "dev-1", "dev-1", 100, "dev-A", false))
            .unwrap();
        s.upsert_state_record("p1", &row("audit", "cid-1", "cid-1", 300, "dev-A", false))
            .unwrap();
        s.upsert_state_record("p2", &row("member", "dev-9", "dev-9", 400, "dev-A", false))
            .unwrap();

        let members = s.list_state_records("p1", "member");
        assert_eq!(members.len(), 2, "project+family scope");
        assert_eq!(members[0].key, "dev-1", "oldest ts first");
        assert_eq!(members[1].key, "dev-2");
        assert_eq!(s.list_state_records("p1", "review_link").len(), 0);
        assert_eq!(s.list_state_records("p2", "member").len(), 1);

        assert_eq!(
            s.list_state_families("p1"),
            vec!["audit".to_string(), "member".to_string()]
        );
        assert!(s.list_state_families("p3").is_empty());
    }

    #[test]
    fn get_missing_is_none_and_cross_family_never_leaks() {
        let (_d, s) = open();
        s.upsert_state_record("p1", &row("review_link", "tok", "tok", 1, "dev-A", false))
            .unwrap();
        assert!(
            s.get_state_record("p1", "member", "tok").is_none(),
            "family-scoped PK"
        );
        assert!(s.get_state_record("p1", "review_link", "other").is_none());
        assert!(s.get_state_record("p2", "review_link", "tok").is_none());
    }
}
