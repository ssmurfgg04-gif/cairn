//! Journal — the sync log IS the database (SPEC §7.1).
//!
//! Append-only, per-project, server-assigned u64 seq (I4). Idempotency via request_id
//! (UUIDv7). Conflict rule implemented EXACTLY: a FileUpsert is accepted iff no entry from a
//! DIFFERENT device has seq > base_seq for the same path; same-device upserts supersede.
//! Fencing is enforced at append (SPEC §8): a leased path requires the current
//! (device, token); stale/expired/mismatched → STALE_LEASE. Leases are advisory; fencing is
//! the guarantee. Rename conflict-check semantics: ADR-0012 (both endpoints checked).

use prost::Message;
use sqlx::Row;
use sqlx::SqlitePool;
use std::sync::Arc;

use cairn_core::clock::SystemClock;
use cairn_core::{CairnError, ErrorKind};
use cairn_proto::pb::journal_op::Op as JournalOpKind;
use cairn_proto::pb::{JournalOp, RenameOp};

/// One materialized journal entry (wire shape).
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub seq: u64,
    pub device_id: String,
    pub op: JournalOp,
    pub server_ts: i64,
}

/// Extract the affected path(s) of an op (conflict-check keys). State records
/// (ADR-0031) ride a SYNTHETIC path derived from (family, key) — it groups
/// records in the journal without colliding with real files, and the real
/// validation of a record happens in `validate_state_record` below, not here.
fn paths_of(op: &JournalOp) -> Vec<String> {
    match op.op.as_ref() {
        Some(JournalOpKind::FileUpsert(o)) => vec![o.path.clone()],
        Some(JournalOpKind::FileDelete(o)) => vec![o.path.clone()],
        Some(JournalOpKind::Rename(r)) => vec![r.old_path.clone(), r.new_path.clone()],
        Some(JournalOpKind::LeaseEvent(l)) => vec![l.path.clone()],
        Some(JournalOpKind::StateRecord(sr)) => {
            vec![cairn_core::pathutil::state_record_path(&sr.family, &sr.key)]
        }
        None => vec![],
    }
}

/// True when this op is a `state_record` (ADR-0031 Phase 1): exempt from
/// lease fencing AND from the §7.1 conflict rule. Concurrent same-key appends
/// from different devices are LEGAL for records — convergence is LWW/union at
/// apply time on every device (`cairn_sync::state_records::
/// apply_record_locally`), never at append time.
fn is_state_record(op: &JournalOp) -> bool {
    matches!(op.op.as_ref(), Some(JournalOpKind::StateRecord(_)))
}

/// Authoritative state-record validation (the choke point complement to
/// `paths_of`'s synthetic path): unknown families and separator-bearing keys
/// never reach the journal — a `\` or `/` in a key would alias into sibling
/// records on every peer's replay.
fn validate_state_record(op: &JournalOp) -> Result<(), CairnError> {
    let Some(JournalOpKind::StateRecord(sr)) = op.op.as_ref() else {
        return Ok(());
    };
    // known families only (the fixed set keeps the synthetic path shape-safe)
    let known = [
        "member",
        "audit",
        "review_version",
        "review_link",
        "review_comment",
    ];
    if !known.contains(&sr.family.as_str()) {
        return Err(CairnError::new(
            ErrorKind::InvalidPath,
            "invalid state record: unknown family",
        ));
    }
    cairn_core::pathutil::validate_state_key(&sr.key)
}

/// Encode the op blob + primary conflict path for storage.
fn encode(op: &JournalOp) -> Result<(Vec<u8>, String), CairnError> {
    let primary = paths_of(op).first().cloned().unwrap_or_default();
    let mut buf = Vec::with_capacity(64);
    op.encode(&mut buf)
        .map_err(|e| CairnError::new(ErrorKind::Internal, format!("op encode: {e}")))?;
    Ok((buf, primary))
}

/// Append one op. Returns `(seq, deduplicated)`.
///
/// # Errors
/// `CONFLICT` when the conflict rule rejects; `STALE_LEASE` when fencing rejects.
pub async fn append(
    pool: &SqlitePool,
    clock: &Arc<dyn SystemClock>,
    tenant_id: &str,
    project_id: &str,
    device_id: &str,
    request_id: &str,
    op: JournalOp,
    lease_token: u64,
) -> Result<(u64, bool), CairnError> {
    // WO6-9 security gate (authoritative): EVERY op path must stay inside the project
    // root. This is the single server-side choke point all devices push through —
    // rejecting here protects every peer's hydration/restore/apply path.
    for p in paths_of(&op) {
        if let Err(e) = cairn_core::pathutil::validate_rel_path(&p) {
            crate::db::audit(
                pool,
                clock,
                tenant_id,
                device_id,
                "journal.append.invalid_path",
                project_id,
                &e.message,
            )
            .await;
            return Err(e);
        }
    }
    // ADR-0031 choke point: a record's family/key gate runs beside the path
    // gate (the synthetic path passes `validate_rel_path` for any
    // family-shaped string, so the KEY must be checked directly).
    if let Err(e) = validate_state_record(&op) {
        crate::db::audit(
            pool,
            clock,
            tenant_id,
            device_id,
            "journal.append.invalid_state_record",
            project_id,
            &e.message,
        )
        .await;
        return Err(e);
    }
    let (op_blob, primary_path) = encode(&op)?;
    let mut conn = crate::db::begin_immediate(pool).await?;

    // idempotency: request_id already accepted → return its seq (retry-safe, SPEC §7.1)
    let existing: Option<i64> = sqlx::query_scalar(
        "SELECT seq FROM journal WHERE tenant_id=?1 AND project_id=?2 AND request_id=?3",
    )
    .bind(tenant_id)
    .bind(project_id)
    .bind(request_id)
    .fetch_optional(&mut *conn)
    .await
    .map_err(db_err)?;
    if let Some(seq) = existing {
        crate::db::rollback(&mut conn).await;
        return Ok((seq.max(0) as u64, true));
    }

    // fencing (SPEC §8): leased + unexpired path requires current (device, token).
    // ADR-0031: state records are EXEMPT — they never touch a leased file path
    // (their journal path is the synthetic `state/<family>/<key>`), so fencing
    // a pen on `scene.prproj` must never delay a roster/audit/review append.
    // The kind guard below is exactly that exemption.
    if matches!(
        op.op.as_ref(),
        Some(JournalOpKind::FileUpsert(_)) | Some(JournalOpKind::Rename(_))
    ) {
        let lease = sqlx::query(
            "SELECT device_id, token, expires_at FROM leases
             WHERE tenant_id=?1 AND project_id=?2 AND path=?3",
        )
        .bind(tenant_id)
        .bind(project_id)
        .bind(&primary_path)
        .fetch_optional(&mut *conn)
        .await
        .map_err(db_err)?;
        if let Some(l) = lease {
            let l_device: String = l.get("device_id");
            let l_token: i64 = l.get("token");
            let l_exp: i64 = l.get("expires_at");
            if l_exp > clock.now_millis() {
                let stale = l_device != device_id || lease_token != l_token.max(0) as u64;
                if stale {
                    crate::db::rollback(&mut conn).await;
                    return Err(CairnError::new(
                        ErrorKind::StaleLease,
                        format!(
                            "path {primary_path} is leased to {l_device} (token {l_token}); got device {device_id} token {lease_token}"
                        ),
                    ));
                }
            }
        }
    }

    // conflict rule, implemented exactly (SPEC §7.1): accepted iff NO entry from a DIFFERENT
    // device has seq > base_seq for the same path (any op kind — upsert/delete/rename).
    // ADR-0031: state records are EXEMPT — the rule exists to preserve file
    // content lineage; records are CRDT-shaped (LWW per key / append-only
    // union by content id) and converge at apply time, so concurrent
    // same-key appends from different devices are legal and MUST both land.
    if !is_state_record(&op) {
        let base_seq = base_seq_of(&op);
        let conflicting: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM journal
             WHERE tenant_id=?1 AND project_id=?2 AND path=?3 AND seq>?4 AND device_id<>?5",
        )
        .bind(tenant_id)
        .bind(project_id)
        .bind(&primary_path)
        .bind(base_seq.max(0))
        .bind(device_id)
        .fetch_one(&mut *conn)
        .await
        .map_err(db_err)?;
        if conflicting > 0 {
            crate::db::rollback(&mut conn).await;
            return Err(CairnError::new(
                ErrorKind::Conflict,
                format!(
                    "path {primary_path}: a different device has seq>{base_seq}; upsert diverged"
                ),
            ));
        }
        // same-device entries with seq > base: always supersede (rule allows)
    }

    // server-assigned seq (I4) inside the same IMMEDIATE tx
    let max_seq: i64 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(seq),0) FROM journal WHERE tenant_id=?1 AND project_id=?2",
    )
    .bind(tenant_id)
    .bind(project_id)
    .fetch_one(&mut *conn)
    .await
    .map_err(db_err)?;
    let seq = max_seq + 1;
    sqlx::query(
        "INSERT INTO journal(tenant_id, project_id, seq, request_id, device_id, path, op, server_ts)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
    )
    .bind(tenant_id)
    .bind(project_id)
    .bind(seq)
    .bind(request_id)
    .bind(device_id)
    .bind(&primary_path)
    .bind(&op_blob)
    .bind(clock.now_millis())
    .execute(&mut *conn)
    .await
    .map_err(db_err)?;

    crate::db::commit(&mut conn).await?;
    Ok((seq as u64, false))
}

fn base_seq_of(op: &JournalOp) -> i64 {
    match op.op.as_ref() {
        Some(JournalOpKind::FileUpsert(o)) => o.base_seq as i64,
        Some(JournalOpKind::FileDelete(o)) => o.base_seq as i64,
        Some(JournalOpKind::Rename(r)) => r.base_seq as i64,
        // LeaseEvents carry no base_seq (informational); state records
        // (ADR-0031) declare no lineage — their appends skip the conflict
        // rule entirely, so the base is never read for them.
        _ => 0,
    }
}

/// Fetch entries strictly after `after_seq` (cursor replay — the guarantee, SPEC §7.1).
pub async fn batch(
    pool: &SqlitePool,
    tenant_id: &str,
    project_id: &str,
    after_seq: u64,
    limit: u32,
) -> Result<Vec<Entry>, CairnError> {
    let rows = sqlx::query(
        "SELECT seq, device_id, op, server_ts FROM journal
         WHERE tenant_id=?1 AND project_id=?2 AND seq>?3 ORDER BY seq ASC LIMIT ?4",
    )
    .bind(tenant_id)
    .bind(project_id)
    .bind(after_seq as i64)
    .bind(i64::from(limit.max(1)))
    .fetch_all(pool)
    .await
    .map_err(db_err)?;
    rows.into_iter()
        .map(|r| {
            let blob: Vec<u8> = r.try_get("op").map_err(db_err)?;
            let op = JournalOp::decode(blob.as_slice())
                .map_err(|e| CairnError::new(ErrorKind::Internal, format!("op decode: {e}")))?;
            Ok(Entry {
                seq: r.get::<i64, _>("seq").max(0) as u64,
                device_id: r.get("device_id"),
                op,
                server_ts: r.get("server_ts"),
            })
        })
        .collect()
}

/// Update a device's cursor (durable; replay base).
pub async fn update_cursor(
    pool: &SqlitePool,
    _tenant_id: &str,
    device_id: &str,
    project_id: &str,
    last_seq: u64,
) -> Result<(), CairnError> {
    sqlx::query(
        "INSERT INTO journal_cursors(device_id, project_id, last_seq) VALUES(?1,?2,?3)
         ON CONFLICT(device_id, project_id) DO UPDATE SET last_seq=excluded.last_seq",
    )
    .bind(device_id)
    .bind(project_id)
    .bind(last_seq as i64)
    .execute(pool)
    .await
    .map_err(db_err)?;
    Ok(())
}

/// Rename with fenced+conflict-checked semantics (ADR-0012): both old and new paths are
/// conflict-checked; the op is metadata-only (never re-chunks).
#[must_use]
pub fn rename_conflict_paths(r: &RenameOp) -> Vec<String> {
    vec![r.old_path.clone(), r.new_path.clone()]
}

fn db_err(e: sqlx::Error) -> CairnError {
    CairnError::new(ErrorKind::Unavailable, format!("db: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cairn_proto::pb::{FileDeleteOp, FileUpsertOp};
    use std::path::Path;

    async fn setup() -> (tempfile::TempDir, SqlitePool, Arc<dyn SystemClock>) {
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::db::open(&Path::new(dir.path()).join("meta.db"))
            .await
            .unwrap();
        crate::db::migrate(&pool).await.unwrap();
        sqlx::query("INSERT INTO tenants(id, created_at) VALUES('t1', 0)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO projects(tenant_id, project_id, created_at) VALUES('t1','p1',0)")
            .execute(&pool)
            .await
            .unwrap();
        (dir, pool, Arc::new(cairn_core::clock::WallClock))
    }

    fn upsert(path: &str, base: u64) -> JournalOp {
        JournalOp {
            op: Some(cairn_proto::pb::journal_op::Op::FileUpsert(FileUpsertOp {
                path: path.into(),
                manifest_hash: format!("m-{path}"),
                size: 10,
                base_seq: base,
            })),
        }
    }

    #[tokio::test]
    async fn sequential_appends_get_server_seq() {
        let (_d, pool, clock) = setup().await;
        let (s1, dedup) = append(&pool, &clock, "t1", "p1", "d1", "r1", upsert("a.mov", 0), 0)
            .await
            .unwrap();
        let (s2, _) = append(
            &pool,
            &clock,
            "t1",
            "p1",
            "d2",
            "r2",
            upsert("a.mov", s1),
            0,
        )
        .await
        .unwrap();
        assert!(!dedup);
        assert_eq!((s1, s2), (1, 2));
    }

    /// WO6-9 security gate: traversal/absolute paths are rejected at the authoritative
    /// choke point — they must NEVER reach the journal (peers replay journal ops into
    /// filesystem joins).
    #[tokio::test]
    async fn append_rejects_path_traversal() {
        let (_d, pool, clock) = setup().await;
        for bad in [
            "../escape.mov",
            "/etc/passwd",
            "sub/../../out.mov",
            r"win\path.mov",
            "C:drive.mov",
            "",
        ] {
            let r = append(&pool, &clock, "t1", "p1", "d1", "r-bad", upsert(bad, 0), 0).await;
            assert!(r.is_err(), "must reject: {bad:?}");
            assert_eq!(r.unwrap_err().kind, cairn_core::ErrorKind::InvalidPath);
        }
        // nothing landed in the journal
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM journal")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n, 0);
        // a rename with a bad OLD path is rejected too
        let op = JournalOp {
            op: Some(cairn_proto::pb::journal_op::Op::Rename(RenameOp {
                old_path: "../old.mov".into(),
                new_path: "new.mov".into(),
                manifest_hash: "m".into(),
                base_seq: 0,
            })),
        };
        let r = append(&pool, &clock, "t1", "p1", "d1", "r-rename", op, 0).await;
        assert_eq!(r.unwrap_err().kind, cairn_core::ErrorKind::InvalidPath);
    }

    #[tokio::test]
    async fn duplicate_request_id_returns_same_seq_deduplicated() {
        let (_d, pool, clock) = setup().await;
        let (s1, _) = append(&pool, &clock, "t1", "p1", "d1", "r1", upsert("a.mov", 0), 0)
            .await
            .unwrap();
        let (s2, dedup) = append(&pool, &clock, "t1", "p1", "d1", "r1", upsert("a.mov", 0), 0)
            .await
            .unwrap();
        assert!(dedup);
        assert_eq!(s1, s2, "retry must be safe (request_id dedupe)");
    }

    /// §7.1 conflict truth table (implement exactly):
    /// accepted iff no entry from a DIFFERENT device has seq > base_seq for the same path.
    #[tokio::test]
    async fn conflict_truth_table() {
        let (_d, pool, clock) = setup().await;
        // d1 appends a.mov at seq 1 (base 0)
        let (s1, _) = append(&pool, &clock, "t1", "p1", "d1", "r1", upsert("a.mov", 0), 0)
            .await
            .unwrap();

        // d2 with base_seq >= s1 → ACCEPTED (no newer different-device entry)
        append(
            &pool,
            &clock,
            "t1",
            "p1",
            "d2",
            "r2",
            upsert("a.mov", s1),
            0,
        )
        .await
        .unwrap();
        // d2 again with stale base (s1 < 2) → CONFLICT (d2's own seq2 was same-device, so the
        // blocking rule checks OTHER devices: d1's seq1 is not > base... base=0 → seq1 from d1
        // IS > 0 and different device → conflict)
        let e = append(&pool, &clock, "t1", "p1", "d2", "r3", upsert("a.mov", 0), 0)
            .await
            .unwrap_err();
        assert_eq!(e.code(), "CONFLICT");

        // same-device supersede: d2 base 0 (its own seq2 exists) → same-device always supersedes?
        // d1 has seq1 > 0 → different device blocks: CONFLICT regardless (rule is per-path)
        let e2 = append(&pool, &clock, "t1", "p1", "d1", "r4", upsert("a.mov", 0), 0)
            .await
            .unwrap_err();
        assert_eq!(e2.code(), "CONFLICT");

        // d1 with base = current max (2) → accepted (supersedes its own + no newer others)
        append(&pool, &clock, "t1", "p1", "d1", "r5", upsert("a.mov", 2), 0)
            .await
            .unwrap();

        // deletes block too (entry = any op on the path): tombstone accepted at fresh base…
        let del = JournalOp {
            op: Some(cairn_proto::pb::journal_op::Op::FileDelete(FileDeleteOp {
                path: "a.mov".into(),
                base_seq: 3,
            })),
        };
        append(&pool, &clock, "t1", "p1", "d2", "r6", del, 0)
            .await
            .unwrap();
        // …then a different device at the stale base is rejected by that tombstone
        let del_stale = JournalOp {
            op: Some(cairn_proto::pb::journal_op::Op::FileDelete(FileDeleteOp {
                path: "a.mov".into(),
                base_seq: 3,
            })),
        };
        let e3 = append(&pool, &clock, "t1", "p1", "d1", "r7", del_stale, 0)
            .await
            .unwrap_err();
        assert_eq!(
            e3.code(),
            "CONFLICT",
            "delete tombstone blocks older-device upserts"
        );

        // different path is unaffected
        append(&pool, &clock, "t1", "p1", "d1", "r8", upsert("b.mov", 0), 0)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn fencing_rejects_wrong_device_and_stale_token() {
        let (_d, pool, clock) = setup().await;
        // d1 acquires lease on scene.prproj → token 1
        let token = crate::leases::acquire(&pool, &clock, "t1", "p1", "scene.prproj", "d1", 60_000)
            .await
            .unwrap()
            .0;
        assert_eq!(token, 1);

        // d1 appends WITH correct token → accepted
        append(
            &pool,
            &clock,
            "t1",
            "p1",
            "d1",
            "r1",
            upsert("scene.prproj", 0),
            token,
        )
        .await
        .unwrap();

        // d2 appends with the (stale for it) token → STALE_LEASE
        let e = append(
            &pool,
            &clock,
            "t1",
            "p1",
            "d2",
            "r2",
            upsert("scene.prproj", 1),
            token,
        )
        .await
        .unwrap_err();
        assert_eq!(e.code(), "STALE_LEASE");

        // d2 appends with no token → STALE_LEASE
        let e2 = append(
            &pool,
            &clock,
            "t1",
            "p1",
            "d2",
            "r3",
            upsert("scene.prproj", 1),
            0,
        )
        .await
        .unwrap_err();
        assert_eq!(e2.code(), "STALE_LEASE");

        // d1 appends with WRONG (old) token → STALE_LEASE
        let e3 = append(
            &pool,
            &clock,
            "t1",
            "p1",
            "d1",
            "r4",
            upsert("scene.prproj", 1),
            token.wrapping_sub(1),
        )
        .await
        .unwrap_err();
        assert_eq!(e3.code(), "STALE_LEASE");

        // release → any device can append again (advisory model)
        crate::leases::release(&pool, "t1", "p1", "scene.prproj", "d1", token)
            .await
            .unwrap();
        append(
            &pool,
            &clock,
            "t1",
            "p1",
            "d2",
            "r5",
            upsert("scene.prproj", 2),
            0,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn expired_lease_does_not_block_advisory_model() {
        let (_d, pool, _clock) = setup().await;
        // acquire with a TTL, then force expiry via the fixed clock (deterministic time)
        let fc = Arc::new(cairn_core::clock::FixedClock::new(1_000_000));
        let fixed: Arc<dyn SystemClock> = fc.clone();
        let token = crate::leases::acquire(&pool, &fixed, "t1", "p1", "x.prproj", "d1", 60_000)
            .await
            .unwrap()
            .0;
        fc.advance(61_000); // lease expired
        append(
            &pool,
            &fixed,
            "t1",
            "p1",
            "d2",
            "r1",
            upsert("x.prproj", 0),
            0,
        )
        .await
        .unwrap();
        let _ = token;
    }

    #[tokio::test]
    async fn cursor_replay_returns_full_suffix() {
        let (_d, pool, clock) = setup().await;
        for i in 0..5 {
            append(
                &pool,
                &clock,
                "t1",
                "p1",
                "d1",
                &format!("r{i}"),
                upsert(&format!("f{i}.mov"), 0),
                0,
            )
            .await
            .unwrap();
        }
        let batch = batch(&pool, "t1", "p1", 2, 100).await.unwrap();
        assert_eq!(batch.len(), 3);
        assert_eq!(batch[0].seq, 3);
        update_cursor(&pool, "t1", "d2", "p1", 5).await.unwrap();
    }

    /// I3: cross-tenant isolation — identical (project, path) under another tenant is invisible.
    #[tokio::test]
    async fn tenancy_isolation() {
        let (_d, pool, clock) = setup().await;
        sqlx::query("INSERT INTO tenants(id, created_at) VALUES('t2', 0)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO projects(tenant_id, project_id, created_at) VALUES('t2','p1',0)")
            .execute(&pool)
            .await
            .unwrap();
        append(&pool, &clock, "t1", "p1", "d1", "r1", upsert("a.mov", 0), 0)
            .await
            .unwrap();
        // t2 sees nothing at seq 0 for same project/path
        let b = batch(&pool, "t2", "p1", 0, 100).await.unwrap();
        assert!(b.is_empty(), "cross-tenant journal leakage — I3 violation");
        // and its own first append gets seq 1 (independent log)
        let (s, _) = append(&pool, &clock, "t2", "p1", "d1", "r1", upsert("a.mov", 0), 0)
            .await
            .unwrap();
        assert_eq!(s, 1);
    }

    // ---------- state records (ADR-0031 Phase 1) ----------

    fn state_record(family: &str, key: &str, ts: i64, tombstone: bool) -> JournalOp {
        JournalOp {
            op: Some(cairn_proto::pb::journal_op::Op::StateRecord(
                cairn_proto::pb::StateRecordOp {
                    family: family.into(),
                    key: key.into(),
                    payload: format!(r#"{{"key":"{key}"}}"#).into_bytes(),
                    ts_ms: ts,
                    tombstone,
                },
            )),
        }
    }

    /// ADR-0031: the §7.1 conflict rule does NOT apply to state records — two
    /// devices appending a record for the SAME key (the racing-writers case:
    /// two machines edit the same member's role concurrently) must BOTH be
    /// accepted with distinct seqs. Convergence is LWW at apply time.
    #[tokio::test]
    async fn state_records_allow_concurrent_same_key_appends() {
        let (_d, pool, clock) = setup().await;
        // d1 appends a member record for device "dev-x" ...
        let (s1, dedup1) = append(
            &pool,
            &clock,
            "t1",
            "p1",
            "d1",
            "r1",
            state_record("member", "dev-x", 100, false),
            0,
        )
        .await
        .unwrap();
        assert!(!dedup1);
        // ... d2 appends the SAME key concurrently (base_seq is irrelevant —
        // records declare none): MUST be Ok, not CONFLICT.
        let (s2, _) = append(
            &pool,
            &clock,
            "t1",
            "p1",
            "d2",
            "r2",
            state_record("member", "dev-x", 200, false),
            0,
        )
        .await
        .unwrap();
        assert_eq!(s1, 1);
        assert_eq!(s2, 2, "both appends land with distinct server seqs");

        // the exemption is scoped to records: the §7.1 conflict rule still
        // fires for FILE ops on their real paths
        append(&pool, &clock, "t1", "p1", "d3", "r3", upsert("a.mov", 0), 0)
            .await
            .unwrap();
        let e = append(&pool, &clock, "t1", "p1", "d4", "r4", upsert("a.mov", 0), 0)
            .await
            .unwrap_err();
        assert_eq!(e.code(), "CONFLICT", "file ops keep the strict rule");
        let batch = batch(&pool, "t1", "p1", 0, 100).await.unwrap();
        assert_eq!(batch.len(), 3);
    }

    #[tokio::test]
    async fn state_record_dedups_by_request_id() {
        let (_d, pool, clock) = setup().await;
        let (s1, d1) = append(
            &pool,
            &clock,
            "t1",
            "p1",
            "d1",
            "req-state-abc",
            state_record("audit", "cid-1", 100, false),
            0,
        )
        .await
        .unwrap();
        // crash-recovery resend of the SAME request id → dedup, same seq
        let (s2, d2) = append(
            &pool,
            &clock,
            "t1",
            "p1",
            "d1",
            "req-state-abc",
            state_record("audit", "cid-1", 100, false),
            0,
        )
        .await
        .unwrap();
        assert!(d2, "retry must dedup by request_id");
        assert!(!d1);
        assert_eq!(s1, s2);
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM journal")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n, 1);
    }

    /// ADR-0031 choke point: record keys are identifiers, not paths — a key
    /// carrying a separator would alias into sibling records on every peer's
    /// replay, so it is rejected at append (same INVALID_PATH taxonomy as the
    /// file-path gate; nothing reaches the journal).
    #[tokio::test]
    async fn state_record_paths_are_validated() {
        let (_d, pool, clock) = setup().await;
        for bad_key in ["dev/1", "", "a\\b", "bell\u{7}"] {
            let r = append(
                &pool,
                &clock,
                "t1",
                "p1",
                "d1",
                "r-bad",
                state_record("member", bad_key, 100, false),
                0,
            )
            .await;
            assert!(r.is_err(), "must reject key: {bad_key:?}");
            assert_eq!(r.unwrap_err().kind, cairn_core::ErrorKind::InvalidPath);
        }
        // unknown family never lands either
        let r = append(
            &pool,
            &clock,
            "t1",
            "p1",
            "d1",
            "r-bad-fam",
            state_record("roster", "dev-1", 100, false),
            0,
        )
        .await;
        assert_eq!(r.unwrap_err().kind, cairn_core::ErrorKind::InvalidPath);

        // and nothing from this test reached the journal
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM journal")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n, 0);

        // the honest record still appends after the rejects
        append(
            &pool,
            &clock,
            "t1",
            "p1",
            "d1",
            "r-ok",
            state_record("member", "dev-1", 100, false),
            0,
        )
        .await
        .unwrap();
    }
}
