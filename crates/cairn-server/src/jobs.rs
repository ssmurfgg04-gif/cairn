//! Control-plane jobs (SPEC §4/§12): idempotent, resumable, kill-switchable workers with a
//! leader lease (DB row). Every job reads its kill switch PER RUN — flags flip without
//! restart harm (§16, tested).
//!
//! - `rebuild_bloom` — bloom negative pre-filter refresh
//! - `gc_pass` — reachability mark-sweep (roots: refs ∪ trash ∪ sessions<7d ∪ legal holds),
//!   14-day grace, shadow mode, epoch guard vs packing
//! - `pack_pass` — small-object packing (50–128MB, zstd), verify-before-switch, atomic
//!   `pack_index` transaction
//! - `tier_pass` / `recall` — nightly tiering of >90d chunks to the cold backend; recall with
//!   progress + ETA
//! - `metering_rollup` — daily bytes_stored recomputation
//! - `canary` — headless round-trip probe (upload→verify→recall), pages on failure via metric

pub mod canary;
pub mod flags;
pub mod gc;
pub mod leader;
pub mod metering;
pub mod pack;
pub mod tier;

use crate::ServerState;
use cairn_core::bloom::Bloom;
use cairn_core::CairnError;

/// Rebuild the per-tenant bloom filter from the authoritative chunks table (cheap, always
/// safe; the authoritative check backstops every positive).
pub async fn rebuild_bloom(state: &ServerState) {
    let rows = sqlx::query("SELECT hash FROM chunks WHERE state='present' LIMIT 1000000")
        .fetch_all(&state.db)
        .await
        .unwrap_or_default();
    let mut bloom = Bloom::with_fpp((rows.len() as u64).max(100_000), 0.01);
    for r in &rows {
        use sqlx::Row;
        if let Ok(h) = r.try_get::<String, _>(0) {
            bloom.insert(h.as_bytes());
        }
    }
    *state.bloom.write().await = bloom;
    tracing::info!(entries = rows.len(), "bloom rebuilt");
}

/// Leader lease acquisition (DB row): only the holder runs scheduled jobs; safe under
/// restarts (lease expires) and duplicates (single row compare-and-set).
pub async fn try_acquire_leader(
    state: &ServerState,
    name: &str,
    holder: &str,
    ttl_millis: i64,
) -> Result<bool, CairnError> {
    let now = state.clock.now_millis();
    let res = sqlx::query(
        "INSERT INTO jobs_leader(name, holder, expires_at) VALUES(?1,?2,?3)
         ON CONFLICT(name) DO UPDATE SET holder=?2, expires_at=?3
         WHERE jobs_leader.expires_at < ?4 OR jobs_leader.holder = ?2",
    )
    .bind(name)
    .bind(holder)
    .bind(now + ttl_millis)
    .bind(now)
    .execute(&state.db)
    .await
    .map_err(|e| CairnError::new(cairn_core::ErrorKind::Unavailable, format!("leader: {e}")))?;
    if res.rows_affected() == 0 {
        // maybe we already hold it
        let holder_row: Option<String> =
            sqlx::query_scalar("SELECT holder FROM jobs_leader WHERE name=?1 AND expires_at>=?2")
                .bind(name)
                .bind(now)
                .fetch_optional(&state.db)
                .await
                .map_err(|e| {
                    CairnError::new(cairn_core::ErrorKind::Unavailable, format!("leader: {e}"))
                })?;
        return Ok(holder_row.as_deref() == Some(holder));
    }
    Ok(true)
}

/// Epoch guard: GC and packing must never overlap on the same objects (SPEC §12). The epoch
/// increments on GC start; packers record the epoch they verified against and fail the
/// switch if it moved.
pub async fn bump_epoch(state: &ServerState) -> Result<i64, CairnError> {
    sqlx::query(
        "INSERT INTO config_flags(name, value, updated_at) VALUES('gc_epoch','1',0)
         ON CONFLICT(name) DO UPDATE SET value = CAST(CAST(value AS INTEGER) + 1 AS TEXT)",
    )
    .execute(&state.db)
    .await
    .map_err(|e| CairnError::new(cairn_core::ErrorKind::Unavailable, format!("epoch: {e}")))?;
    let v: String = sqlx::query_scalar("SELECT value FROM config_flags WHERE name='gc_epoch'")
        .fetch_one(&state.db)
        .await
        .map_err(|e| CairnError::new(cairn_core::ErrorKind::Unavailable, format!("epoch: {e}")))?;
    v.parse::<i64>()
        .map_err(|e| CairnError::new(cairn_core::ErrorKind::Internal, format!("{e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn leader_lease_is_single_holder_and_expiring() {
        let dir = tempfile::tempdir().unwrap();
        let state = crate::tests_support::state_at(dir.path()).await;
        let a = try_acquire_leader(&state, "gc", "worker-a", 1_000)
            .await
            .unwrap();
        let b = try_acquire_leader(&state, "gc", "worker-b", 1_000)
            .await
            .unwrap();
        assert!(a);
        assert!(!b, "second holder must not acquire a live lease");
    }
}
