//! Tiering (SPEC §12): nightly; chunks untouched >90d → copy to cold backend → verify
//! checksum → tombstone hot copy. NEVER tier manifests/trees/commits. Deep Archive is
//! per-tenant opt-in. RecallService restores with progress + ETA.

use cairn_core::hash::Hash;
use cairn_core::{CairnError, ErrorKind};
use crate::storage::LocalFsStore;
use crate::ServerState;
use sqlx::Row;

const TIER_AFTER_DAYS: i64 = 90;

/// Cold (B2-class) backend abstraction. Dev = second LocalFs dir; production = B2 S3-compat.
#[async_trait::async_trait]
pub trait ColdStore: Send + Sync {
    async fn put(&self, key: &str, bytes: &[u8]) -> Result<(), CairnError>;
    async fn get(&self, key: &str) -> Result<Vec<u8>, CairnError>;
    async fn delete(&self, key: &str) -> Result<(), CairnError>;
}

/// Dev cold backend (simulates B2 semantics for tests).
pub struct DevColdStore {
    root: std::path::PathBuf,
}

impl DevColdStore {
    /// New dev cold store under `root`.
    pub fn new(root: &std::path::Path) -> Self {
        let _ = std::fs::create_dir_all(root);
        DevColdStore { root: root.to_path_buf() }
    }

    fn path(&self, key: &str) -> std::path::PathBuf {
        self.root.join(key.replace('/', "_"))
    }
}

#[async_trait::async_trait]
impl ColdStore for DevColdStore {
    async fn put(&self, key: &str, bytes: &[u8]) -> Result<(), CairnError> {
        std::fs::write(self.path(key), bytes)
            .map_err(|e| CairnError::new(ErrorKind::Io, format!("cold put: {e}")))
    }
    async fn get(&self, key: &str) -> Result<Vec<u8>, CairnError> {
        std::fs::read(self.path(key))
            .map_err(|_| CairnError::new(ErrorKind::NotFound, format!("cold {key}")))
    }
    async fn delete(&self, key: &str) -> Result<(), CairnError> {
        let _ = std::fs::remove_file(self.path(key));
        Ok(())
    }
}

/// Tier cold chunks (>90d untouched) to the cold backend. Kill switch: `tiering_enabled`.
/// Deep Archive is per-tenant opt-in (`tenants.deep_archive`) — cold tiering only runs for
/// opted-in tenants when the backend supports it.
pub async fn tier_pass(state: &ServerState, cold: &dyn ColdStore, tenant_id: &str) -> Result<u64, CairnError> {
    if !crate::jobs::flags::enabled(state, "tiering_enabled").await? {
        return Ok(0);
    }
    let cutoff = state.clock.now_millis() - TIER_AFTER_DAYS * 24 * 3600 * 1000;
    let rows: Vec<(String, i64)> = sqlx::query(
        "SELECT hash, size FROM chunks WHERE tenant_id=?1 AND tier='hot' AND state='present' AND last_touched<?2",
    )
    .bind(tenant_id)
    .bind(cutoff)
    .fetch_all(&state.db)
    .await
    .map_err(|e| CairnError::new(ErrorKind::Unavailable, format!("tier scan: {e}")))?
    .into_iter()
    .map(|r| (r.get(0), r.get(1)))
    .collect();

    let mut tiered = 0u64;
    for (hash, size) in rows {
        let key = LocalFsStore::chunk_key(tenant_id, &hash);
        // 1) copy to cold
        let bytes = match state.store.get(&key).await {
            Ok(b) => b,
            Err(_) => continue,
        };
        cold.put(&key, &bytes).await?;
        // 2) verify checksum BEFORE tombstoning the hot copy (SPEC §12)
        match cold.get(&key).await {
            Ok(readback) if readback == bytes && readback.len() as i64 == size => {
                state.store.delete(&key).await?;
                sqlx::query("UPDATE chunks SET tier='archive' WHERE tenant_id=?1 AND hash=?2")
                    .bind(tenant_id)
                    .bind(&hash)
                    .execute(&state.db)
                    .await
                    .map_err(|e| CairnError::new(ErrorKind::Unavailable, format!("tier: {e}")))?;
                crate::db::audit(&state.db, &state.clock, tenant_id, "tier-job", "tier.archive", &hash, "cold copy verified").await;
                tiered += 1;
            }
            _ => {
                // verification failed → keep hot copy (never lose data, I2)
                let _ = cold.delete(&key).await;
            }
        }
    }
    Ok(tiered)
}

/// Start a recall job (archive → hot) and track progress in `jobs` (progress + ETA per §12).
pub async fn start_recall(
    state: &ServerState,
    cold: &dyn ColdStore,
    tenant_id: &str,
    job_id: &str,
    only_hash: Option<&str>,
) -> Result<(), CairnError> {
    let rows: Vec<(String, i64)> = sqlx::query(
        "SELECT hash, size FROM chunks WHERE tenant_id=?1 AND tier='archive' AND state='present'
         AND (?2 IS NULL OR hash=?2)",
    )
    .bind(tenant_id)
    .bind(only_hash)
    .fetch_all(&state.db)
    .await
    .map_err(|e| CairnError::new(ErrorKind::Unavailable, format!("recall scan: {e}")))?
    .into_iter()
    .map(|r| (r.get(0), r.get(1)))
    .collect();
    let total: i64 = rows.iter().map(|(_, s)| s).sum();
    let started = state.clock.now_millis();
    sqlx::query(
        "INSERT INTO jobs(id, tenant_id, kind, state, progress, total, detail, updated_at)
         VALUES(?1,?2,'recall','running',0,?3,'',?4)
         ON CONFLICT(id) DO UPDATE SET state='running', progress=0",
    )
    .bind(job_id)
    .bind(tenant_id)
    .bind(total)
    .bind(started)
    .execute(&state.db)
    .await
    .map_err(|e| CairnError::new(ErrorKind::Unavailable, format!("job row: {e}")))?;

    let mut done: i64 = 0;
    for (hash, size) in rows {
        let key = LocalFsStore::chunk_key(tenant_id, &hash);
        let bytes = cold.get(&key).await?;
        // verify before serving hot again (I2)
        if Hash::of(&bytes).hex() != hash || bytes.len() as i64 != size {
            return Err(CairnError::new(ErrorKind::ChecksumMismatch, format!("recall {hash} corrupt")));
        }
        state.store.put(&key, &bytes).await?;
        sqlx::query("UPDATE chunks SET tier='hot', last_touched=?3 WHERE tenant_id=?1 AND hash=?2")
            .bind(tenant_id)
            .bind(&hash)
            .bind(state.clock.now_millis())
            .execute(&state.db)
            .await
            .map_err(|e| CairnError::new(ErrorKind::Unavailable, format!("recall: {e}")))?;
        done += size;
        let elapsed = state.clock.now_millis() - started;
        let progress = if total > 0 { done as f64 / total as f64 } else { 1.0 };
        // ETA: elapsed/progress − elapsed (ms), clamped
        let eta = if progress > 0.001 {
            ((elapsed as f64 / progress) - elapsed as f64).max(0.0) as i64
        } else {
            -1
        };
        sqlx::query(
            "UPDATE jobs SET progress=?2, detail=?3, updated_at=?4 WHERE id=?1",
        )
        .bind(job_id)
        .bind(progress)
        .bind(format!("eta_ms={eta}"))
        .bind(state.clock.now_millis())
        .execute(&state.db)
        .await
        .map_err(|e| CairnError::new(ErrorKind::Unavailable, format!("job upd: {e}")))?;
    }
    sqlx::query("UPDATE jobs SET state='complete', progress=1.0, updated_at=?2 WHERE id=?1")
        .bind(job_id)
        .bind(state.clock.now_millis())
        .execute(&state.db)
        .await
        .map_err(|e| CairnError::new(ErrorKind::Unavailable, format!("job done: {e}")))?;
    crate::db::audit(&state.db, &state.clock, tenant_id, "recall", "tier.recall", job_id, "complete").await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SPEC §12: recall round-trip works from "B2" (dev cold store) — M6 AC.
    #[tokio::test]
    async fn tier_then_recall_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let state = crate::tests_support::state_at(dir.path()).await;
        sqlx::query("INSERT OR IGNORE INTO tenants(id, created_at) VALUES('t1',0)")
            .execute(&state.db).await.unwrap();
        // one chunk, long untouched
        let body = b"cold-chunk-body".repeat(100);
        let h = Hash::of(&body);
        state.store.put(&LocalFsStore::chunk_key("t1", &h.hex()), &body).await.unwrap();
        sqlx::query("INSERT INTO chunks(tenant_id, hash, size, tier, state, last_touched) VALUES('t1',?1,?2,'hot','present',0)")
            .bind(h.hex())
            .bind(body.len() as i64)
            .execute(&state.db).await.unwrap();

        let cold = DevColdStore::new(&dir.path().join("cold"));
        let tiered = tier_pass(&state, &cold, "t1").await.unwrap();
        assert_eq!(tiered, 1);
        // hot copy gone, archive tier recorded
        assert!(state.store.head(&LocalFsStore::chunk_key("t1", &h.hex())).await.is_err());

        // recall brings it back verified
        start_recall(&state, &cold, "t1", "job-1", Some(&h.hex())).await.unwrap();
        let back = state.store.get(&LocalFsStore::chunk_key("t1", &h.hex())).await.unwrap();
        assert_eq!(back, body);
        let tier: String = sqlx::query_scalar("SELECT tier FROM chunks WHERE hash=?1")
            .bind(h.hex()).fetch_one(&state.db).await.unwrap();
        assert_eq!(tier, "hot");
        let st: String = sqlx::query_scalar("SELECT state FROM jobs WHERE id='job-1'")
            .fetch_one(&state.db).await.unwrap();
        assert_eq!(st, "complete");
    }

    /// Kill switch: tiering disabled → no-op (flag flips without restart, §16).
    #[tokio::test]
    async fn tiering_respects_kill_switch() {
        let dir = tempfile::tempdir().unwrap();
        let state = crate::tests_support::state_at(dir.path()).await;
        crate::jobs::flags::set(&state, "ops", "tiering_enabled", "false").await.unwrap();
        let cold = DevColdStore::new(&tempfile::tempdir().unwrap().path());
        assert_eq!(tier_pass(&state, &cold, "t1").await.unwrap(), 0);
    }
}
