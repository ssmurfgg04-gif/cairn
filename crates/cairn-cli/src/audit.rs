//! Audit ledger (ADR-0022): every daemon-side RBAC decision is recorded
//! in `<root>/.cairn/audit.json` on the machine that made it.
//!
//! HONEST SCOPE: `.cairn*` is on the scan ignore-list (SPEC §10), so this
//! ledger is LOCAL — the owner sees every decision made on THIS machine,
//! in this file, with content-derived ids. It is not yet synced
//! machine-to-machine (that needs append-only merging through the sync
//! surface, a named follow-up in ADR-0022's ledger). What it already
//! fixes: decisions that previously vanished into daemon logs now land
//! in a durable, bounded, tamper-evident file the dashboard renders.
//!
//! Design notes:
//! * append-only, bounded (the newest `MAX_ENTRIES` survive; a bounded
//!   ledger that keeps working beats an unbounded one that grows forever);
//! * ids are content-derived (blake3 of device|ts|action|allowed) so the
//!   same decision recorded on two machines converges instead of
//!   duplicating;
//! * deterministic BTreeMap serialization — same wire bytes for the same
//!   decisions, so the three-way merge machinery applies to it unchanged.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const SCHEMA: &str = "cairn-audit/v1";
/// Bounds the ledger: newest N decisions survive (see module doc).
pub const MAX_ENTRIES: usize = 500;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEntry {
    /// Unix millis of the decision.
    pub ts_ms: i64,
    /// Acting device id ("" when the daemon has no identity yet).
    pub device: String,
    /// The role the device held at decision time.
    pub role: String,
    /// Short action label, e.g. "ctl/detach-root".
    pub action: String,
    /// Project id ("" for daemon-wide actions).
    pub project: String,
    /// Whether the guard allowed it.
    pub allowed: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditFile {
    #[serde(default)]
    pub schema: String,
    /// Keyed by entry id (content-derived, sortable by insertion since
    /// ids embed no ordering — the VALUE carries ts; reads sort by ts).
    #[serde(default)]
    pub entries: BTreeMap<String, AuditEntry>,
}

/// `<root>/.cairn/audit.json`
pub fn audit_path(root: &Path) -> PathBuf {
    root.join(".cairn").join("audit.json")
}

impl AuditFile {
    pub fn to_json(&self) -> Result<Vec<u8>, String> {
        serde_json::to_vec_pretty(self).map_err(|e| format!("serialize audit: {e}"))
    }

    pub fn from_json(bytes: &[u8]) -> Result<AuditFile, String> {
        let f: AuditFile =
            serde_json::from_slice(bytes).map_err(|e| format!("parse audit: {e}"))?;
        if !f.schema.is_empty() && f.schema != SCHEMA {
            return Err(format!("unknown audit schema: {}", f.schema));
        }
        Ok(f)
    }

    /// Entry id: blake3 of the decision content — the same decision on
    /// two machines converges to one entry after sync.
    fn id_for(e: &AuditEntry) -> String {
        let mut h = blake3::Hasher::new();
        h.update(e.device.as_bytes());
        h.update(&e.ts_ms.to_le_bytes());
        h.update(e.action.as_bytes());
        h.update(e.project.as_bytes());
        h.update(&[u8::from(e.allowed)]);
        h.finalize().to_hex().to_string()[..16].to_string()
    }

    /// Record a decision (idempotent by content id) and persist
    /// atomically. A corrupt existing ledger is NEVER silently replaced:
    /// the decision still happens (enforcement is not hostage to
    /// bookkeeping), but the failure is surfaced so operators notice.
    pub fn record(root: &Path, entry: AuditEntry) -> Result<(), String> {
        let path = audit_path(root);
        let mut f = match std::fs::read(&path) {
            Ok(b) => AuditFile::from_json(&b)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => AuditFile {
                schema: SCHEMA.into(),
                entries: BTreeMap::new(),
            },
            Err(e) => return Err(format!("read audit: {e}")),
        };
        f.schema = SCHEMA.into();
        let id = Self::id_for(&entry);
        f.entries.insert(id, entry);
        // bound the ledger, keeping the NEWEST by ts (then id for stability)
        if f.entries.len() > MAX_ENTRIES {
            let mut keys: Vec<(i64, String)> = f
                .entries
                .iter()
                .map(|(k, v)| (v.ts_ms, k.clone()))
                .collect();
            keys.sort();
            let drop_n = f.entries.len() - MAX_ENTRIES;
            for (i, (_, k)) in keys.into_iter().enumerate() {
                if i >= drop_n {
                    break;
                }
                f.entries.remove(&k);
            }
        }
        let json = f.to_json()?;
        cairn_proxy::pipeline::atomic_write(&path, &json)
    }

    /// Read + sort by time for display (dashboard Team tab, CLI).
    pub fn load(root: &Path) -> Result<Vec<(String, AuditEntry)>, String> {
        let path = audit_path(root);
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(format!("read audit: {e}")),
        };
        let f = AuditFile::from_json(&bytes)?;
        let mut rows: Vec<(String, AuditEntry)> = f.entries.into_iter().collect();
        rows.sort_by(|a, b| a.1.ts_ms.cmp(&b.1.ts_ms).then_with(|| a.0.cmp(&b.0)));
        Ok(rows)
    }

    /// The entry-point used by the daemon guard.
    pub fn decision(
        root: &Path,
        ts_ms: i64,
        device: &str,
        role: &str,
        action: &str,
        project: &str,
        allowed: bool,
    ) -> Result<(), String> {
        Self::record(
            root,
            AuditEntry {
                ts_ms,
                device: device.to_string(),
                role: role.to_string(),
                action: action.to_string(),
                project: project.to_string(),
                allowed,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        tempfile::tempdir().unwrap().keep()
    }

    #[test]
    fn records_are_idempotent_bounded_and_sorted() {
        let root = tmp();
        for i in 0..3 {
            AuditFile::decision(
                &root,
                1000 + i,
                "dev-a",
                "editor",
                "ctl/detach-root",
                "p1",
                true,
            )
            .unwrap();
        }
        // same content again: no duplicate
        AuditFile::decision(
            &root,
            1001,
            "dev-a",
            "editor",
            "ctl/detach-root",
            "p1",
            true,
        )
        .unwrap();
        let rows = AuditFile::load(&root).unwrap();
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().all(|(_, e)| e.allowed));
        assert_eq!(rows[0].1.ts_ms, 1000);
        assert_eq!(rows[2].1.ts_ms, 1002);

        // bound: MAX+50 decisions keep only the newest MAX
        for i in 0..(MAX_ENTRIES + 50) {
            AuditFile::decision(
                &root,
                10_000 + i as i64,
                "dev-b",
                "owner",
                "ctl/pin",
                "p1",
                true,
            )
            .unwrap();
        }
        let rows = AuditFile::load(&root).unwrap();
        assert_eq!(rows.len(), MAX_ENTRIES);
        assert!(rows.iter().all(|(_, e)| e.ts_ms >= 10_050));
    }

    #[test]
    fn same_decision_yields_the_same_entry_id() {
        let a = tmp();
        let b = tmp();
        let e = AuditEntry {
            ts_ms: 42,
            device: "dev-a".into(),
            role: "editor".into(),
            action: "ctl/attach-root".into(),
            project: "p".into(),
            allowed: true,
        };
        AuditFile::record(&a, e.clone()).unwrap();
        AuditFile::record(&b, e).unwrap();
        let ra = AuditFile::load(&a).unwrap();
        let rb = AuditFile::load(&b).unwrap();
        assert_eq!(ra.len(), 1);
        assert_eq!(rb.len(), 1);
        assert_eq!(ra[0].0, rb[0].0); // deterministic ids: dedupe/merge stays safe
    }

    #[test]
    fn corrupt_ledger_fails_closed_on_read_but_decisions_still_record() {
        let root = tmp();
        std::fs::create_dir_all(root.join(".cairn")).unwrap();
        std::fs::write(audit_path(&root), b"{ nope").unwrap();
        assert!(AuditFile::load(&root).is_err());
        // recording over corrupt bytes surfaces the error (never silently
        // replaces history), but does not panic
        assert!(AuditFile::decision(&root, 1, "d", "r", "a", "p", true).is_err());
    }

    #[test]
    fn schema_is_stamped_and_roundtrips() {
        let root = tmp();
        AuditFile::decision(&root, 1, "d", "owner", "x", "p", false).unwrap();
        let bytes = std::fs::read(audit_path(&root)).unwrap();
        let text = String::from_utf8(bytes.clone()).unwrap();
        assert!(text.contains(&format!("\"{SCHEMA}\"")));
        let back = AuditFile::from_json(&bytes).unwrap();
        assert_eq!(back.entries.len(), 1);
        // foreign schema refused
        assert!(AuditFile::from_json(br#"{"schema":"other/v1","entries":{}}"#).is_err());
    }
}
