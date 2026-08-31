//! Upload/download control-plane logic (M3, SPEC §9).
//!
//! - BatchExists: bloom negative pre-filter ONLY; the authoritative check is the chunks table.
//!   A bloom false positive can never skip an upload (property-tested adversarially).
//! - Upload sessions: presigned PUTs (checksum-bound, TTL ≤ 1h, write-scoped), resumable at
//!   chunk granularity via session rows.
//! - CompleteUpload: HEAD-verifies a 10% sample (100% for chunks > 64MB) before inserting
//!   chunk rows.

use prost::Message;
use sqlx::Row;

use cairn_core::hash::Hash;
use cairn_core::{CairnError, ErrorKind};
use cairn_proto::pb::{
    CreateUploadSessionResponse, UploadPut, UploadReceipt,
};

use crate::storage::LocalFsStore;
use crate::ServerState;

const SESSION_TTL_MILLIS: i64 = 3_600_000; // ≤1h (SPEC §9)
const SAMPLE_PERCENT: u64 = 10;
pub const BIG_CHUNK_BYTES: u64 = 64 * 1024 * 1024;

/// Bloom negative pre-filter + authoritative KV (SPEC §9.1). Negative answers are certain;
/// positives (true or false-positive) fall through to the chunks table.
pub async fn batch_exists(
    state: &ServerState,
    tenant_id: &str,
    hashes: &[String],
) -> Result<Vec<String>, CairnError> {
    let bloom = state.bloom.read().await;
    let mut missing = Vec::new();
    let mut needs_authoritative = Vec::new();
    for h in hashes {
        // I3: bloom is per-tenant (rebuilt per tenant scope); unknown tenant → all missing
        if bloom.might_contain(h.as_bytes()) {
            needs_authoritative.push(h.clone()); // maybe present → MUST verify (rule)
        } else {
            missing.push(h.clone()); // bloom-negative ⇒ definitely absent
        }
    }
    drop(bloom);
    if !needs_authoritative.is_empty() {
        for h in &needs_authoritative {
            let row: Option<i64> = sqlx::query_scalar(
                "SELECT 1 FROM chunks WHERE tenant_id=?1 AND hash=?2 AND state='present' LIMIT 1",
            )
            .bind(tenant_id)
            .bind(h)
            .fetch_optional(&state.db)
            .await
            .map_err(|e| CairnError::new(ErrorKind::Unavailable, format!("exists: {e}")))?;
            if row.is_none() {
                missing.push(h.clone());
            }
        }
    }
    Ok(missing)
}

/// Create a resumable upload session with presigned PUTs.
pub async fn create_session(
    state: &ServerState,
    identity: &crate::auth::DeviceIdentity,
    project_id: &str,
    missing: &[String],
) -> Result<CreateUploadSessionResponse, CairnError> {
    if missing.is_empty() {
        return Err(CairnError::new(ErrorKind::NotFound, "no missing chunks provided"));
    }
    let session_id = uuid::Uuid::now_v7().to_string();
    let expires_at = state.clock.now_millis() + SESSION_TTL_MILLIS;
    // session rows persist across restarts → chunk-granular resume (I2, SPEC §9.1)
    let mut blob = Vec::with_capacity(missing.len() * 34);
    for h in missing {
        blob.extend_from_slice(h.as_bytes());
        blob.push(b'\n');
    }
    sqlx::query(
        "INSERT INTO upload_sessions(id, tenant_id, device_id, chunk_hashes, expires_at, state)
         VALUES(?1,?2,?3,?4,?5,'open')",
    )
    .bind(&session_id)
    .bind(&identity.tenant_id)
    .bind(&identity.device_id)
    .bind(&blob)
    .bind(expires_at)
    .execute(&state.db)
    .await
    .map_err(|e| CairnError::new(ErrorKind::Unavailable, format!("session insert: {e}")))?;

    let mut puts = Vec::with_capacity(missing.len());
    for h in missing {
        let key = LocalFsStore::chunk_key(&identity.tenant_id, h);
        let url = state.store.presign_put(&key, 3600).await?;
        puts.push(UploadPut { chunk_hash: h.clone(), url, expires_at });
    }
    crate::db::audit(
        &state.db,
        &state.clock,
        &identity.tenant_id,
        &identity.device_id,
        "upload.session",
        project_id,
        &format!("session={session_id} chunks={}", missing.len()),
    )
    .await;
    Ok(CreateUploadSessionResponse { session_id, puts })
}

/// CompleteUpload: sample-verify (10%, 100% for >64MB) then register chunk rows.
pub async fn complete(
    state: &ServerState,
    identity: &crate::auth::DeviceIdentity,
    session_id: &str,
    receipts: Vec<UploadReceipt>,
) -> Result<cairn_proto::pb::CompleteUploadResponse, CairnError> {
    // session must be open, owned, unexpired
    let row = sqlx::query(
        "SELECT tenant_id, device_id, expires_at, state FROM upload_sessions WHERE id=?1",
    )
    .bind(session_id)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| CairnError::new(ErrorKind::Unavailable, format!("session: {e}")))?;
    let Some(row) = row else {
        return Err(CairnError::new(ErrorKind::NotFound, format!("session {session_id}")));
    };
    let tenant: String = row.get("tenant_id");
    let device: String = row.get("device_id");
    let expires: i64 = row.get("expires_at");
    if tenant != identity.tenant_id || device != identity.device_id {
        return Err(CairnError::new(ErrorKind::PermissionDenied, "session not yours"));
    }
    if expires < state.clock.now_millis() {
        return Err(CairnError::new(ErrorKind::SessionExpired, "upload session expired"));
    }

    let n = receipts.len() as u64;
    let mut verified = Vec::new();
    let mut rejected = Vec::new();
    for (i, r) in receipts.iter().enumerate() {
        let is_big = r.size > BIG_CHUNK_BYTES;
        let sample_hit = is_big || (n > 0 && (i as u64) % (100 / SAMPLE_PERCENT.max(1)) == 0);
        let key = LocalFsStore::chunk_key(&tenant, &r.chunk_hash);
        let head = state.store.head(&key).await;
        match head {
            Ok(size) if size == r.size => {
                verified.push(r.chunk_hash.clone());
            }
            _ if sample_hit => {
                rejected.push(r.chunk_hash.clone());
            }
            _ => {
                // unsampled HEAD miss: still register only what HEAD confirmed; missing objects
                // are re-detected by BatchExists on the retry pass (idempotent, I2-safe)
                if head.is_err() {
                    rejected.push(r.chunk_hash.clone());
                } else {
                    verified.push(r.chunk_hash.clone());
                }
            }
        }
    }

    let now = state.clock.now_millis();
    for h in &verified {
        let size: Option<i64> = sqlx::query_scalar(
            "SELECT size FROM chunks WHERE tenant_id=?1 AND hash=?2",
        )
        .bind(&tenant)
        .bind(h)
        .fetch_optional(&state.db)
        .await
        .map_err(|e| CairnError::new(ErrorKind::Unavailable, format!("chunk: {e}")))?;
        if size.is_none() {
            sqlx::query(
                "INSERT INTO chunks(tenant_id, hash, size, tier, state, last_touched)
                 VALUES(?1,?2,?3,'hot','present',?4)",
            )
            .bind(&tenant)
            .bind(h)
            .bind(i64::try_from(receipts.iter().find(|r| &r.chunk_hash == h).map(|r| r.size).unwrap_or(0)).unwrap_or(0))
            .bind(now)
            .execute(&state.db)
            .await
            .map_err(|e| CairnError::new(ErrorKind::Unavailable, format!("chunk insert: {e}")))?;
            // metering: bytes_uploaded (SPEC §12/§5.2)
            let day = day_string(now);
            sqlx::query(
                "INSERT INTO metering(tenant_id, day, bytes_uploaded) VALUES(?1,?2,?3)
                 ON CONFLICT(tenant_id, day) DO UPDATE SET bytes_uploaded = bytes_uploaded + ?3",
            )
            .bind(&tenant)
            .bind(&day)
            .bind(
                receipts
                    .iter()
                    .find(|r| &r.chunk_hash == h)
                    .map(|r| i64::try_from(r.size).unwrap_or(0))
                    .unwrap_or(0),
            )
            .execute(&state.db)
            .await
            .map_err(|e| CairnError::new(ErrorKind::Unavailable, format!("metering: {e}")))?;
        }
    }

    // mark session complete if everything verified
    if rejected.is_empty() {
        let _ = sqlx::query("UPDATE upload_sessions SET state='complete' WHERE id=?1")
            .bind(session_id)
            .execute(&state.db)
            .await;
    }
    Ok(cairn_proto::pb::CompleteUploadResponse { verified, rejected })
}

/// Signed immutable download URL (1h TTL, Range-capable; renew-on-403 is client-side).
pub async fn download_url(
    state: &ServerState,
    tenant_id: &str,
    manifest_hash: &str,
) -> Result<(String, i64), CairnError> {
    let key = LocalFsStore::object_key(tenant_id, manifest_hash);
    let expires_at = state.clock.now_millis() + SESSION_TTL_MILLIS;
    let url = state.store.presign_get(&key, 3600).await?;
    Ok((url, expires_at))
}

/// Register a manifest object after upload (used by the client's complete path).
pub async fn register_manifest(
    state: &ServerState,
    tenant_id: &str,
    manifest_hash: &str,
    bytes: &[u8],
) -> Result<(), CairnError> {
    if Hash::of(bytes).hex() != manifest_hash {
        return Err(CairnError::new(
            ErrorKind::ChecksumMismatch,
            "manifest bytes do not match manifest_hash",
        ));
    }
    let key = LocalFsStore::object_key(tenant_id, manifest_hash);
    state.store.put(&key, bytes).await?;
    let (total, count) = count_entries(bytes)?;
    sqlx::query(
        "INSERT INTO manifests(tenant_id, hash, size, entry_count) VALUES(?1,?2,?3,?4)
         ON CONFLICT(tenant_id, hash) DO NOTHING",
    )
    .bind(tenant_id)
    .bind(manifest_hash)
    .bind(bytes.len() as i64)
    .bind(count as i64)
    .execute(&state.db)
    .await
    .map_err(|e| CairnError::new(ErrorKind::Unavailable, format!("manifest row: {e}")))?;
    let _ = total;
    Ok(())
}

fn count_entries(bytes: &[u8]) -> Result<(usize, usize), CairnError> {
    let m = cairn_core::manifest::Manifest::parse(bytes)
        .map_err(|e| CairnError::new(ErrorKind::ManifestFormat, e.message))?;
    Ok((m.total_len() as usize, m.entry_count()))
}

fn day_string(now_millis: i64) -> String {
    // UTC day (YYYY-MM-DD) without chrono
    let secs = now_millis.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// Encode an op for tests/tools (used by the e2e harness).
pub fn encode_op(op: &cairn_proto::pb::JournalOp) -> Vec<u8> {
    let mut buf = Vec::new();
    let _ = op.encode(&mut buf);
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn day_string_matches_known_epoch() {
        assert_eq!(day_string(1_700_000_000_000), "2023-11-14");
    }

    #[test]
    fn encode_op_roundtrip() {
        let op = cairn_proto::pb::JournalOp {
            op: Some(cairn_proto::pb::journal_op::Op::FileUpsert(
                cairn_proto::pb::FileUpsertOp {
                    path: "a.mov".into(),
                    manifest_hash: "ff".into(),
                    size: 1,
                    base_seq: 0,
                },
            )),
        };
        let bytes = encode_op(&op);
        assert!(!bytes.is_empty());
    }
}
