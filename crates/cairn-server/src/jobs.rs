//! Control-plane jobs (SPEC §4): idempotent, resumable, kill-switchable workers with a
//! leader lease (DB row). M2/M3 scope: bloom rebuild; GC/pack/tier/fold/canary arrive at M6.

use cairn_core::bloom::Bloom;
use crate::ServerState;

/// Rebuild the per-tenant bloom filter from the authoritative chunks table.
/// Kill switch: none needed (cheap, always-safe); consistency is irrelevant because the
/// authoritative check backstops every positive.
pub async fn rebuild_bloom(state: &ServerState) {
    let rows = sqlx::query("SELECT hash FROM chunks WHERE state='present' LIMIT 1_000_000")
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
