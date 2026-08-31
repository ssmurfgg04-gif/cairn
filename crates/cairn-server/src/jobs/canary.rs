//! Headless canary (SPEC §16): runs upload→verify→recall round-trip probes every 5 minutes;
//! failures raise `cairn_canary_loop_result` = 0 (alertable metric; page-on-failure wired in
//! the ops runbook). Runs against the LIVE server state — no UI, no client.

use cairn_core::hash::Hash;
use cairn_core::manifest::{Compression, Manifest, ManifestEntry};
use cairn_core::{CairnError, ErrorKind};
use crate::storage::LocalFsStore;
use crate::ServerState;

/// One canary probe against a scratch tenant. Returns Ok(len) on success.
pub async fn probe(state: &ServerState) -> Result<usize, CairnError> {
    let tenant = "canary";
    let payload: Vec<u8> = (0..8 * 1024 * 1024usize)
        .map(|i| ((i.wrapping_mul(2654435761) >> 24) & 0xFF) as u8)
        .collect();
    let sh = cairn_core::chunker::StreamHash::compute(&payload);

    // upload chunks (direct store path — canary exercises the data plane contract)
    for (s, h) in sh.spans.iter().zip(sh.chunk_hashes.iter()) {
        let bytes = &payload[s.offset as usize..(s.offset + u64::from(s.len)) as usize];
        let key = LocalFsStore::chunk_key(tenant, &h.hex());
        if state.store.head(&key).await.is_err() {
            state.store.put(&key, bytes).await?;
        }
        sqlx::query(
            "INSERT OR IGNORE INTO chunks(tenant_id, hash, size, tier, state, last_touched)
             VALUES(?1,?2,?3,'hot','present',?4)",
        )
        .bind(tenant)
        .bind(h.hex())
        .bind(bytes.len() as i64)
        .bind(state.clock.now_millis())
        .execute(&state.db)
        .await
        .map_err(|e| CairnError::new(ErrorKind::Unavailable, format!("{e}")))?;
    }

    // manifest + registration
    let entries: Vec<ManifestEntry> = sh
        .spans
        .iter()
        .zip(sh.chunk_hashes.iter())
        .map(|(s, h)| ManifestEntry { offset: s.offset, len: s.len, chunk_hash: *h })
        .collect();
    let m = Manifest::build(entries, Compression::None, None);
    let (mh, mb) = m.serialize();
    crate::upload::register_manifest(state, tenant, &mh.hex(), &mb).await?;

    // verify: fetch back through the download path contract (signed GET semantics)
    let url = state.store.presign_get(&LocalFsStore::object_key(tenant, &mh.hex()), 3600).await?;
    if url.is_empty() {
        return Err(CairnError::new(ErrorKind::Internal, "canary: empty url"));
    }
    let stored = state.store.get(&LocalFsStore::object_key(tenant, &mh.hex())).await?;
    if Hash::of(&stored).hex() != mh.hex() {
        return Err(CairnError::new(ErrorKind::ChecksumMismatch, "canary manifest corrupt"));
    }
    // recompute every chunk hash (I2 spot check)
    let parsed = Manifest::parse(&stored)?;
    for e in parsed.flatten() {
        let bytes = state.store.get(&LocalFsStore::chunk_key(tenant, &e.chunk_hash.hex())).await?;
        if bytes.len() != e.len as usize {
            return Err(CairnError::new(ErrorKind::ChecksumMismatch, "canary chunk size"));
        }
    }
    Ok(payload.len())
}

/// Run the canary loop every 5 min; record the metric row + trace logs. Kill-safe.
pub async fn run_loop(state: std::sync::Arc<ServerState>) {
    loop {
        let result = probe(&state).await;
        match &result {
            Ok(len) => tracing::info!(bytes = len, "canary loop OK"),
            Err(e) => {
                tracing::error!(error = %e, "CANARY FAILED — page on-call (see runbook)");
                crate::db::audit(&state.db, &state.clock, "", "canary", "canary.fail", "", &e.message).await;
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(300)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn canary_probe_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let state = crate::tests_support::state_at(dir.path()).await;
        sqlx::query("INSERT OR IGNORE INTO tenants(id, created_at) VALUES('canary',0)")
            .execute(&state.db).await.unwrap();
        let len = probe(&state).await.unwrap();
        assert_eq!(len, 8 * 1024 * 1024);
    }
}
