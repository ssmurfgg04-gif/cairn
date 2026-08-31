//! Small-object packing (SPEC §12): objects <8MB only; packs immutable 50–128MB, zstd 3–7;
//! verify the finished pack BEFORE atomically switching pack_index rows in one transaction;
//! epoch guard prevents GC/pack overlap on the same objects (port of git packfile+idx
//! concepts — format versioned byte; see THIRD_PARTY.md).

use cairn_core::hash::Hash;
use cairn_core::{CairnError, ErrorKind};
use crate::ServerState;
use sqlx::Row;

pub const PACK_MAGIC: &[u8; 4] = b"CPCK";
pub const PACK_VERSION: u8 = 1;
const PACK_TARGET: usize = 64 * 1024 * 1024;
const PACK_MAX: usize = 128 * 1024 * 1024;
const SMALL_OBJECT: usize = 8 * 1024 * 1024;

/// Serialize a pack from `(hash, bytes)` pairs: magic | ver | u32 n | (u32 len, hash32, data)*
#[must_use]
pub fn build_pack(objects: &[(String, Vec<u8>)]) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(PACK_MAGIC);
    buf.push(PACK_VERSION);
    buf.extend_from_slice(&(objects.len() as u32).to_le_bytes());
    for (hash, bytes) in objects {
        buf.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        if let Some(h) = Hash::from_hex(hash) {
            buf.extend_from_slice(&h.0);
        } else {
            buf.extend_from_slice(&[0u8; 32]);
        }
        buf.extend_from_slice(bytes);
    }
    buf
}

/// Parse a pack into (hash → bytes). Fuzz target sibling (`pack_index_parse`).
pub fn parse_pack(bytes: &[u8]) -> Result<Vec<(String, Vec<u8>)>, CairnError> {
    let err = || CairnError::new(ErrorKind::ManifestFormat, "pack parse failed");
    if bytes.len() < 9 || &bytes[0..4] != PACK_MAGIC || bytes[4] != PACK_VERSION {
        return Err(err());
    }
    let n = u32::from_le_bytes([bytes[5], bytes[6], bytes[7], bytes[8]]) as usize;
    let mut pos = 9;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        if pos + 36 > bytes.len() {
            return Err(err());
        }
        let len = u32::from_le_bytes([bytes[pos], bytes[pos + 1], bytes[pos + 2], bytes[pos + 3]])
            as usize;
        let hash = Hash::from_slice(&bytes[pos + 4..pos + 36]).ok_or_else(err)?;
        pos += 36;
        if pos + len > bytes.len() {
            return Err(err());
        }
        out.push((hash.hex(), bytes[pos..pos + len].to_vec()));
        pos += len;
    }
    Ok(out)
}

/// Pack one tenant's small (<8MB) objects that are not yet packed.
/// Verify finished pack BEFORE switching pack_index rows in ONE transaction (SPEC §12).
pub async fn pack_pass(state: &ServerState, tenant_id: &str) -> Result<(String, u64), CairnError> {
    // epoch guard: record the GC epoch; if it moves before the switch, abort this pass
    let epoch: String =
        sqlx::query_scalar("SELECT value FROM config_flags WHERE name='gc_epoch'")
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
        return Err(CairnError::new(ErrorKind::ChecksumMismatch, "pack verify: count mismatch"));
    }
    for ((h, b), (ph, pb)) in parsed.iter().zip(objects.iter()) {
        if h != ph || b != pb {
            return Err(CairnError::new(ErrorKind::ChecksumMismatch, "pack verify: content mismatch"));
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
        return Err(CairnError::new(ErrorKind::Unavailable, "gc moved during packing; retry"));
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

    #[test]
    fn pack_roundtrip_and_garbage() {
        let objs = vec![
            (Hash::of(b"a").hex(), b"alpha-body".to_vec()),
            (Hash::of(b"b").hex(), b"beta-body-longer".to_vec()),
        ];
        let pack = build_pack(&objs);
        let parsed = parse_pack(&pack).unwrap();
        assert_eq!(parsed, objs);
        assert!(parse_pack(b"garbage").is_err());
        let mut corrupted = pack.clone();
        let last = corrupted.len() - 1;
        corrupted[last] ^= 0xFF;
        let parsed2 = parse_pack(&corrupted).unwrap();
        // body corruption: hash entry still matches → content mismatch detectable upstream
        assert_eq!(parsed2.len(), 2);
    }

    /// SPEC §12: pack switch atomic under kill -9 → covered by SQLite transaction semantics;
    /// the epoch guard test lives in gc (bump_epoch moves → pack_pass aborts before switch).
    #[tokio::test]
    async fn pack_pass_verifies_before_switch() {
        let dir = tempfile::tempdir().unwrap();
        let state = crate::tests_support::state_at(dir.path()).await;
        sqlx::query("INSERT OR IGNORE INTO tenants(id, created_at) VALUES('t1',0)")
            .execute(&state.db).await.unwrap();
        // register two small manifests with real object bytes
        for i in 0..2 {
            let body = format!("manifest-body-{i}").into_bytes();
            let h = cairn_core::hash::Hash::of(&body);
            state.store.put(&crate::storage::LocalFsStore::object_key("t1", &h.hex()), &body).await.unwrap();
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
            .fetch_one(&state.db).await.unwrap();
        assert_eq!(cnt, 2);
    }
}
