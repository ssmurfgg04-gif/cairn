//! State records (ADR-0031 Phase 1): publish + merge for project-scoped
//! collaboration state (roster / audit / review) carried by journal
//! `state_record` ops.
//!
//! # Record id scheme (the one place it is defined)
//!
//! * **LWW families** — `member`, `review_link`, `review_comment`:
//!   `record_id = key`. One register per key; concurrent writers converge by
//!   LWW (higher `ts_ms` wins, tie-break higher `device_id`); a tombstone is
//!   just another LWW value (the Phase-2 cross-machine revoke substrate).
//! * **Append families** — `audit`, `review_version`:
//!   `record_id = blake3(family | key | payload)[..32]` hex — a CONTENT id.
//!   Distinct content is a distinct record; re-publishing identical content
//!   collapses (union dedupe) both at the request_id layer (same content →
//!   same request id, see `cairn_core::ids::state_record_request_id`) and at
//!   the store layer (INSERT OR IGNORE by record_id). `ts_ms`/`device_id` are
//!   deliberately NOT in the id: the same fact written twice must stay one
//!   fact, and tombstones are never sent for append families.
//!
//! # Publish discipline
//!
//! The local record table is written FIRST (step 3 of
//! [`publish_state_record`]): the pull phase skips own-device journal entries
//! (engine.rs own-op suppression), so the local table is ONLY updated at
//! publish time for our own records — replay will never re-deliver them. The
//! op is then durable in the outbox BEFORE the send (I2: a crash between
//! enqueue and append resumes with the same request_id; the server dedupes).
//!
//! # Where merges happen
//!
//! [`apply_record_locally`] is THE single merge-semantics function — used by
//! the publish path, the replay path (apply.rs `StateRecord` arm) and the
//! tests. The store layer (cairn_store::state_records) is dumb CRUD.

use prost::Message as _;

use cairn_core::pathutil::{state_record_path, validate_state_key};
use cairn_core::{CairnError, ErrorKind};
use cairn_proto::pb::JournalOp;
use cairn_store::{Outbox, OutboxEntry, Store};

use crate::plane::state_record_op;

/// The known record families (ADR-0031 Phase 1). Unknown families are
/// rejected at publish and gracefully skipped at replay (the same contract as
/// an old client skipping an unknown oneof arm — forward compatibility).
pub const FAMILIES: &[&str] = &[
    "member",
    "audit",
    "review_version",
    "review_link",
    "review_comment",
];

/// Families whose records are LWW-registers keyed by the record key.
const LWW_FAMILIES: &[&str] = &["member", "review_link", "review_comment"];

/// Validate a family name against the known set.
///
/// # Errors
/// `InvalidPath`-kind error naming the contract (unknown family).
pub fn validate_family(family: &str) -> Result<(), CairnError> {
    if FAMILIES.contains(&family) {
        Ok(())
    } else {
        Err(CairnError::new(
            ErrorKind::InvalidPath,
            format!(
                "unknown state record family '{family}' (known: {})",
                FAMILIES.join("|")
            ),
        ))
    }
}

/// The record id for a record (see the module doc for the scheme).
#[must_use]
pub fn record_id_for(family: &str, key: &str, payload: &[u8]) -> String {
    if LWW_FAMILIES.contains(&family) {
        return key.to_string();
    }
    let mut h = blake3::Hasher::new();
    h.update(family.as_bytes());
    h.update(b"|");
    h.update(key.as_bytes());
    h.update(b"|");
    h.update(payload);
    h.finalize().to_hex().to_string()[..32].to_string()
}

/// THE merge-semantics function (module doc): one record, one project.
///
/// * LWW families (`member`, `review_link`, `review_comment`): replace the
///   existing row iff `(ts_ms, device_id)` sorts strictly higher than the
///   existing row's — a tombstone competes with the same rule (it is just
///   another register value).
/// * Append families (`audit`, `review_version`): INSERT OR IGNORE by record
///   id (append-only union; tombstones never sent — a content id has no
///   "delete", and the audit ledger's honesty is exactly that it only grows).
pub fn apply_record_locally(
    store: &Store,
    project_id: &str,
    family: &str,
    record_id: &str,
    key: &str,
    ts_ms: i64,
    device_id: &str,
    payload: &[u8],
    tombstone: bool,
) -> Result<(), CairnError> {
    let row = cairn_store::StateRecordRow {
        family: family.to_string(),
        record_id: record_id.to_string(),
        key: key.to_string(),
        ts_ms,
        device_id: device_id.to_string(),
        payload: payload.to_vec(),
        tombstone,
    };
    if LWW_FAMILIES.contains(&family) {
        if let Some(existing) = store.get_state_record(project_id, family, record_id) {
            let new_rank = (ts_ms, device_id);
            let old_rank = (existing.ts_ms, existing.device_id.as_str());
            // TIE RULE: a tombstone beats a live value at the same rank
            // (mint + revoke inside one millisecond must not resurrect the
            // link on the next replay — fail-closed, the ADR-0031 Phase-2
            // security property). Every other equal-rank write is a replay:
            // the register keeps its current value.
            let tie_break_tombstone = tombstone && !existing.tombstone;
            if new_rank < old_rank || (new_rank == old_rank && !tie_break_tombstone) {
                return Ok(());
            }
            store.upsert_state_record(project_id, &row)?;
            return Ok(());
        }
        store.upsert_state_record(project_id, &row)?;
        return Ok(());
    }
    // append families: union by content id — first writer wins the row,
    // replays and re-writes of the same content are no-ops
    store.insert_state_record(project_id, &row)
}

/// Everything the publish and enqueue paths derive from the raw record input:
/// `(record_id, request_id, op, journal_path)`. Validation happens HERE so
/// both paths reject the same inputs identically.
fn build(
    tenant_id: &str,
    project_id: &str,
    device_id: &str,
    family: &str,
    key: &str,
    payload: &[u8],
    ts_ms: i64,
    tombstone: bool,
) -> Result<(String, String, JournalOp, String), CairnError> {
    validate_family(family)?;
    validate_state_key(key)?;
    let record_id = record_id_for(family, key, payload);
    let request_id = cairn_core::ids::state_record_request_id(
        tenant_id, project_id, family, key, payload, ts_ms, device_id,
    );
    let op = state_record_op(family, key, payload, ts_ms, tombstone);
    let path = state_record_path(family, key);
    Ok((record_id, request_id, op, path))
}

fn outbox_enqueue(
    outbox: &Outbox,
    store: &Store,
    local_ns: &str,
    request_id: &str,
    op: &JournalOp,
) -> Result<(), CairnError> {
    let mut buf = Vec::new();
    op.encode(&mut buf)
        .map_err(|e| CairnError::new(ErrorKind::Internal, format!("state op encode: {e}")))?;
    outbox.enqueue(OutboxEntry {
        request_id: request_id.to_string(),
        project_id: local_ns.to_string(),
        op: buf,
        state: "pending".into(),
        attempts: 0,
        created_at: store.clock().now_millis(),
    })
}

/// Enqueue a record WITHOUT sending: durable-before-send only. The sync
/// loop's outbox drain sends it on the next pass. This is the daemon-surface
/// entry point (ADR-0030 actions run where no engine handle exists — the
/// engine owns no state a record publish needs besides the plane, and the
/// outbox IS the send queue).
///
/// Returns the record id (content id for append families, key for LWW).
///
/// # Errors
/// Unknown family, invalid key, or a store/outbox failure. Callers that
/// publish as a side effect of a primary action MUST treat this as
/// fire-and-forget (log + continue): a record publish failure must never fail
/// the action it rode in on.
pub fn enqueue_state_record(store: &Store, rec: PublishParams<'_>) -> Result<String, CairnError> {
    let (record_id, request_id, op, _path) = build(
        rec.tenant_id,
        rec.project_id,
        rec.device_id,
        rec.family,
        rec.key,
        rec.payload,
        rec.ts_ms,
        rec.tombstone,
    )?;
    // own-op suppression: the pull phase never replays our own entries back —
    // the local table MUST be updated here, before the op is even durable
    apply_record_locally(
        store,
        rec.local_ns,
        rec.family,
        &record_id,
        rec.key,
        rec.ts_ms,
        rec.device_id,
        rec.payload,
        rec.tombstone,
    )?;
    let outbox = Outbox::new(store.conn_handle());
    outbox_enqueue(&outbox, store, rec.local_ns, &request_id, &op)?;
    Ok(record_id)
}

/// Inputs for [`enqueue_state_record`]. `project_id` (server journal scope)
/// feeds the request-id material — request ids dedupe per (tenant, project) on
/// the server — while `local_ns` scopes the LOCAL rows and outbox (they differ
/// only for additional roots, ADR-0019 §2; for the default root they are the
/// same string). `device_id` is the journal authorship (`author_id`) of the
/// publishing root: LWW tie-breaks and apply-time authorship both see this
/// exact string.
pub struct PublishParams<'a> {
    pub tenant_id: &'a str,
    pub project_id: &'a str,
    pub local_ns: &'a str,
    pub device_id: &'a str,
    pub family: &'a str,
    pub key: &'a str,
    pub payload: &'a [u8],
    pub ts_ms: i64,
    pub tombstone: bool,
}

impl crate::engine::Engine {
    /// Validate + apply locally + durable outbox + send immediately (the same
    /// retry/conflict path `send_outbox_entry` gives file ops).
    ///
    /// `Err` after the enqueue is honest and safe to ignore: the entry stays
    /// pending in the outbox and the next sync pass re-sends it (request_id
    /// dedup makes that idempotent). Records carry no file row — the send
    /// path never fabricates one and never runs a conflict copy for them
    /// (engine.rs guards the `StateRecord` arm).
    pub async fn publish_state_record(
        &self,
        family: &str,
        key: &str,
        payload: &[u8],
        ts_ms: i64,
        tombstone: bool,
    ) -> Result<(), CairnError> {
        let (record_id, request_id, op, path) = build(
            &self.tenant_id,
            &self.project_id,
            &self.author_id,
            family,
            key,
            payload,
            ts_ms,
            tombstone,
        )?;
        // local state FIRST (own-op suppression — see module doc)
        apply_record_locally(
            &self.store,
            &self.local_ns,
            family,
            &record_id,
            key,
            ts_ms,
            &self.author_id,
            payload,
            tombstone,
        )?;
        outbox_enqueue(&self.outbox, &self.store, &self.local_ns, &request_id, &op)?;
        // No lease can exist on a synthetic state path; resolve it anyway so
        // this stays correct if that ever changes.
        let lease_token = self.store.get_lease(&path).map_or(0, |(t, _)| t);
        let mut stats = crate::engine::PassStats::default();
        self.send_outbox_entry(&request_id, op, lease_token, &path, &mut stats)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cairn_core::clock::WallClock;
    use cairn_proto::pb::journal_op::Op as OpKind;
    use std::sync::Arc;

    fn store() -> Store {
        let dir = tempfile::tempdir().unwrap();
        Store::open(dir.path(), Arc::new(WallClock)).unwrap()
    }

    fn payload(role: &str) -> Vec<u8> {
        format!(r#"{{"role":"{role}"}}"#).into_bytes()
    }

    // ---------- LWW ordering ----------

    #[test]
    fn lww_higher_ts_wins() {
        let s = store();
        apply_record_locally(
            &s,
            "p1",
            "member",
            "dev-1",
            "dev-1",
            100,
            "dev-A",
            &payload("editor"),
            false,
        )
        .unwrap();
        // older ts loses (offline clock behind)
        apply_record_locally(
            &s,
            "p1",
            "member",
            "dev-1",
            "dev-1",
            50,
            "dev-B",
            &payload("viewer"),
            false,
        )
        .unwrap();
        let g = s.get_state_record("p1", "member", "dev-1").unwrap();
        assert_eq!((g.ts_ms, g.device_id.as_str()), (100, "dev-A"));
        assert!(String::from_utf8(g.payload).unwrap().contains("editor"));

        // newer ts wins
        apply_record_locally(
            &s,
            "p1",
            "member",
            "dev-1",
            "dev-1",
            200,
            "dev-B",
            &payload("owner"),
            false,
        )
        .unwrap();
        let g = s.get_state_record("p1", "member", "dev-1").unwrap();
        assert_eq!((g.ts_ms, g.device_id.as_str()), (200, "dev-B"));
        assert!(String::from_utf8(g.payload).unwrap().contains("owner"));
    }

    #[test]
    fn lww_tie_breaks_on_higher_device_id() {
        let s = store();
        apply_record_locally(
            &s,
            "p1",
            "member",
            "dev-1",
            "dev-1",
            100,
            "dev-Z",
            &payload("editor"),
            false,
        )
        .unwrap();
        // same ts, LOWER device id: loses
        apply_record_locally(
            &s,
            "p1",
            "member",
            "dev-1",
            "dev-1",
            100,
            "dev-A",
            &payload("viewer"),
            false,
        )
        .unwrap();
        let g = s.get_state_record("p1", "member", "dev-1").unwrap();
        assert_eq!(g.device_id, "dev-Z");
        assert!(String::from_utf8(g.payload).unwrap().contains("editor"));
        // same ts, HIGHER device id: wins
        apply_record_locally(
            &s,
            "p1",
            "member",
            "dev-1",
            "dev-1",
            100,
            "dev-Z9",
            &payload("reviewer"),
            false,
        )
        .unwrap();
        let g = s.get_state_record("p1", "member", "dev-1").unwrap();
        assert_eq!(g.device_id, "dev-Z9");
        assert!(String::from_utf8(g.payload).unwrap().contains("reviewer"));
    }

    #[test]
    fn lww_tombstone_is_just_another_value() {
        let s = store();
        apply_record_locally(
            &s,
            "p1",
            "review_link",
            "tok",
            "tok",
            100,
            "dev-A",
            &payload("live"),
            false,
        )
        .unwrap();
        // a tombstone with a newer ts revokes
        apply_record_locally(
            &s,
            "p1",
            "review_link",
            "tok",
            "tok",
            200,
            "dev-A",
            &[],
            true,
        )
        .unwrap();
        let g = s.get_state_record("p1", "review_link", "tok").unwrap();
        assert!(
            g.tombstone,
            "newer tombstone wins (Phase-2 revoke substrate)"
        );
        // an OLD write arriving late does NOT resurrect the revoked link
        apply_record_locally(
            &s,
            "p1",
            "review_link",
            "tok",
            "tok",
            150,
            "dev-B",
            &payload("live"),
            false,
        )
        .unwrap();
        assert!(
            s.get_state_record("p1", "review_link", "tok")
                .unwrap()
                .tombstone
        );
    }

    // ---------- append families ----------

    #[test]
    fn append_union_is_idempotent_by_content() {
        let s = store();
        let p = payload("editor");
        let id = record_id_for("audit", "cid", &p);
        apply_record_locally(&s, "p1", "audit", &id, "cid", 100, "dev-A", &p, false).unwrap();
        // same content re-applied at a different ts/device: still one row
        apply_record_locally(&s, "p1", "audit", &id, "cid", 999, "dev-B", &p, false).unwrap();
        assert_eq!(s.list_state_records("p1", "audit").len(), 1);
        assert_eq!(s.get_state_record("p1", "audit", &id).unwrap().ts_ms, 100);

        // distinct content = distinct id = a second row (union grows)
        let p2 = payload("owner");
        let id2 = record_id_for("audit", "cid2", &p2);
        apply_record_locally(&s, "p1", "audit", &id2, "cid2", 101, "dev-B", &p2, false).unwrap();
        assert_eq!(s.list_state_records("p1", "audit").len(), 2);
    }

    #[test]
    fn record_id_scheme_matches_the_module_doc() {
        let p = payload("editor");
        // LWW families: record id IS the key
        for fam in ["member", "review_link", "review_comment"] {
            assert_eq!(record_id_for(fam, "key-1", &p), "key-1");
        }
        // append families: content id — distinct content distinct id, same
        // content same id (across ts/device, which are NOT id material)
        let a = record_id_for("audit", "k", &p);
        let b = record_id_for("audit", "k", &p);
        assert_eq!(a, b);
        assert_eq!(a.len(), 32);
        assert_ne!(a, record_id_for("audit", "k2", &p));
        assert_ne!(a, record_id_for("review_version", "k", &p));
        assert_ne!(a, record_id_for("audit", "k", &payload("owner")));
    }

    #[test]
    fn validate_family_and_key_gates() {
        for fam in FAMILIES {
            assert!(validate_family(fam).is_ok());
        }
        assert!(validate_family("roster").is_err());
        assert!(validate_family("").is_err());

        assert!(validate_state_key("dev-1").is_ok());
        assert!(validate_state_key("").is_err());
        assert!(validate_state_key("a/b").is_err());
    }

    #[test]
    fn enqueue_applies_locally_and_lands_in_the_outbox() {
        let s = store();
        let id = enqueue_state_record(
            &s,
            PublishParams {
                tenant_id: "t1",
                project_id: "p1",
                local_ns: "p1",
                device_id: "dev-A",
                family: "member",
                key: "dev-1",
                payload: &payload("editor"),
                ts_ms: 100,
                tombstone: false,
            },
        )
        .unwrap();
        assert_eq!(id, "dev-1", "LWW families: record id = key");
        // local table updated at publish time (own-op suppression contract)
        let g = s.get_state_record("p1", "member", "dev-1").unwrap();
        assert_eq!(g.device_id, "dev-A");
        // durable op in the outbox, decodable back to the same record
        let outbox = Outbox::new(s.conn_handle());
        let pending = outbox.pending("p1", 10);
        assert_eq!(pending.len(), 1);
        let op = JournalOp::decode(pending[0].op.as_slice()).unwrap();
        match op.op.unwrap() {
            OpKind::StateRecord(sr) => {
                assert_eq!(sr.family, "member");
                assert_eq!(sr.key, "dev-1");
                assert_eq!(sr.ts_ms, 100);
                assert!(!sr.tombstone);
            }
            other => panic!("expected StateRecord, got {other:?}"),
        }
        assert!(pending[0].request_id.starts_with("req-state-"));
    }

    #[test]
    fn enqueue_rejects_unknown_family_before_any_write() {
        let s = store();
        let r = enqueue_state_record(
            &s,
            PublishParams {
                tenant_id: "t1",
                project_id: "p1",
                local_ns: "p1",
                device_id: "dev-A",
                family: "roster",
                key: "dev-1",
                payload: &payload("editor"),
                ts_ms: 100,
                tombstone: false,
            },
        );
        assert!(r.is_err());
        assert!(s.get_state_record("p1", "member", "dev-1").is_none());
        assert!(Outbox::new(s.conn_handle()).pending("p1", 10).is_empty());
    }

    #[test]
    fn enqueue_is_idempotent_by_request_id_and_content() {
        fn params(payload: &[u8]) -> PublishParams<'_> {
            PublishParams {
                tenant_id: "t1",
                project_id: "p1",
                local_ns: "p1",
                device_id: "dev-A",
                family: "audit",
                key: "cid-1",
                payload,
                ts_ms: 100,
                tombstone: false,
            }
        }
        let s = store();
        enqueue_state_record(&s, params(&payload("editor"))).unwrap();
        // a retry / racing caller re-enqueues the SAME content: same request
        // id (server dedupe) and still exactly one row (union by content id)
        enqueue_state_record(&s, params(&payload("editor"))).unwrap();
        let outbox = Outbox::new(s.conn_handle());
        let ids: std::collections::HashSet<String> = outbox
            .pending("p1", 10)
            .into_iter()
            .map(|e| e.request_id)
            .collect();
        assert_eq!(ids.len(), 1, "same content must derive the same request id");
        assert_eq!(s.list_state_records("p1", "audit").len(), 1);
    }
}
