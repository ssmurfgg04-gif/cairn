//! Small-object packing (SPEC §12): objects <8MB only; packs immutable 50–128MB, zstd 3–7;
//! verify the finished pack BEFORE atomically switching pack_index rows in one transaction;
//! epoch guard prevents GC/pack overlap on the same objects (port of git packfile+idx
//! concepts — format versioned byte; see THIRD_PARTY.md).

use crate::ServerState;
use cairn_core::{CairnError, ErrorKind};
use sqlx::Row;

const PACK_TARGET: usize = 64 * 1024 * 1024;
const SMALL_OBJECT: usize = 8 * 1024 * 1024;

pub use cairn_core::pack::{build_pack, parse_pack};

/// Pack one tenant's small (<8MB) objects that are not yet packed.
/// Verify finished pack BEFORE switching pack_index rows in ONE transaction (SPEC §12).
pub async fn pack_pass(state: &ServerState, tenant_id: &str) -> Result<(String, u64), CairnError> {
    // epoch guard: record the GC epoch; if it moves before the switch, abort this pass
    let epoch: String = sqlx::query_scalar("SELECT value FROM config_flags WHERE name='gc_epoch'")
        .fetch_optional(&state.db)
        .await
        .map_err(|e| CairnError::new(ErrorKind::Unavailable, format!("{e}")))?
        .unwrap_or_else(|| "0".into());

    // candidates: manifest objects (trees/commits live in o/) not yet in pack_index
    let rows: Vec<(String, i64)> = sqlx::query(
        "SELECT m.hash, m.size FROM manifests m
         WHERE m.tenant_id=?1 AND m.size < ?2
           AND NOT EXISTS (SELECT 1 FROM pack_index p WHERE p.tenant_id=m.tenant_id AND p.object_hash=m.hash)",
    )
    .bind(tenant_id)
    .bind(SMALL_OBJECT as i64)
    .fetch_all(&state.db)
    .await
    .map_err(|e| CairnError::new(ErrorKind::Unavailable, format!("candidates: {e}")))?
    .into_iter()
    .map(|r| (r.get(0), r.get(1)))
    .collect();

    if rows.is_empty() {
        return Ok((String::new(), 0));
    }

    // gather objects, building ONE pack up to the target size
    let day = super::metering::day_string(state.clock.now_millis());
    let mut objects: Vec<(String, Vec<u8>)> = Vec::new();
    let mut pack_size = 9usize;
    for (hash, _) in &rows {
        let bytes = match state
            .store
            .get(&crate::storage::LocalFsStore::object_key(tenant_id, hash))
            .await
        {
            Ok(b) => b,
            Err(_) => continue,
        };
        if pack_size + bytes.len() + 36 > PACK_TARGET {
            break;
        }
        pack_size += bytes.len() + 36;
        objects.push((hash.clone(), bytes));
    }
    if objects.is_empty() {
        return Ok((String::new(), 0));
    }

    // compress the object stream (zstd 3..7; 3 for determinism here)
    let raw = build_pack(&objects);
    let compressed = zstd_encode(&raw)?;
    let pack_key = format!("packs/{day}-{epoch}.pack");
    let pack_path = crate::storage::LocalFsStore::pack_key(tenant_id, &pack_key);
    state.store.put(&pack_path, &compressed).await?;

    // VERIFY the finished pack (read back, parse, compare every object) BEFORE the switch
    let readback = state.store.get(&pack_path).await?;
    let decompressed = zstd_decode(&readback, raw.len())?;
    let parsed = parse_pack(&decompressed)?;
    if parsed.len() != objects.len() {
        return Err(CairnError::new(
            ErrorKind::ChecksumMismatch,
            "pack verify: count mismatch",
        ));
    }
    for ((h, b), (ph, pb)) in parsed.iter().zip(objects.iter()) {
        if h != ph || b != pb {
            return Err(CairnError::new(
                ErrorKind::ChecksumMismatch,
                "pack verify: content mismatch",
            ));
        }
    }

    // ATOMIC switch: all pack_index rows in one transaction; epoch guard re-checked
    let epoch_now: String =
        sqlx::query_scalar("SELECT value FROM config_flags WHERE name='gc_epoch'")
            .fetch_optional(&state.db)
            .await
            .map_err(|e| CairnError::new(ErrorKind::Unavailable, format!("{e}")))?
            .unwrap_or_else(|| "0".into());
    if epoch_now != epoch {
        return Err(CairnError::new(
            ErrorKind::Unavailable,
            "gc moved during packing; retry",
        ));
    }
    let mut conn = crate::db::begin_immediate(&state.db).await?;
    let mut offset = 9i64;
    for (hash, bytes) in &objects {
        sqlx::query(
            "INSERT OR REPLACE INTO pack_index(tenant_id, object_hash, pack_key, offset, len)
             VALUES(?1,?2,?3,?4,?5)",
        )
        .bind(tenant_id)
        .bind(hash)
        .bind(&pack_key)
        .bind(offset)
        .bind(bytes.len() as i64)
        .execute(&mut *conn)
        .await
        .map_err(|e| CairnError::new(ErrorKind::Unavailable, format!("pack_index: {e}")))?;
        offset += (bytes.len() + 36) as i64;
    }
    sqlx::query(
        "INSERT INTO packs(tenant_id, pack_key, size, state, created_at) VALUES(?1,?2,?3,'active',?4)
         ON CONFLICT(tenant_id, pack_key) DO UPDATE SET size=?3",
    )
    .bind(tenant_id)
    .bind(&pack_key)
    .bind(compressed.len() as i64)
    .bind(state.clock.now_millis())
    .execute(&mut *conn)
    .await
    .map_err(|e| CairnError::new(ErrorKind::Unavailable, format!("packs row: {e}")))?;
    crate::db::commit(&mut conn).await?;
    Ok((pack_key, objects.len() as u64))
}

fn zstd_encode(bytes: &[u8]) -> Result<Vec<u8>, CairnError> {
    zstd::bulk::compress(bytes, 3).map_err(|e| CairnError::new(ErrorKind::Io, format!("zstd: {e}")))
}

fn zstd_decode(bytes: &[u8], orig_hint: usize) -> Result<Vec<u8>, CairnError> {
    zstd::bulk::decompress(bytes, orig_hint.max(SMALL_OBJECT) * 2 + 1024)
        .map_err(|e| CairnError::new(ErrorKind::Io, format!("zstd: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SPEC §12: pack switch atomic under kill -9 → covered by SQLite transaction semantics;
    /// the epoch guard test lives in gc (bump_epoch moves → pack_pass aborts before switch).
    #[tokio::test]
    async fn pack_pass_verifies_before_switch() {
        let dir = tempfile::tempdir().unwrap();
        let state = crate::tests_support::state_at(dir.path()).await;
        sqlx::query("INSERT OR IGNORE INTO tenants(id, created_at) VALUES('t1',0)")
            .execute(&state.db)
            .await
            .unwrap();
        // register two small manifests with real object bytes
        for i in 0..2 {
            let body = format!("manifest-body-{i}").into_bytes();
            let h = cairn_core::hash::Hash::of(&body);
            state
                .store
                .put(
                    &crate::storage::LocalFsStore::object_key("t1", &h.hex()),
                    &body,
                )
                .await
                .unwrap();
            sqlx::query("INSERT OR IGNORE INTO manifests(tenant_id, hash, size, entry_count) VALUES('t1',?1,?2,0)")
                .bind(h.hex())
                .bind(body.len() as i64)
                .execute(&state.db).await.unwrap();
        }
        let (key, n) = pack_pass(&state, "t1").await.unwrap();
        assert_eq!(n, 2);
        assert!(key.starts_with("packs/"));
        // pack_index rows switched atomically
        let cnt: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pack_index WHERE tenant_id='t1'")
            .fetch_one(&state.db)
            .await
            .unwrap();
        assert_eq!(cnt, 2);
    }
}
