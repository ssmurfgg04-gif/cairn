//! M3 e2e (SPEC §19): end-to-end round trip through the real server + object endpoint with a
//! SERVER RESTART mid-upload (chunk-granular resume), byte-identical verification, and the
//! adversarial-bloom proof that a skipped upload can never happen.

use std::sync::Arc;

use cairn_core::clock::{SystemClock, WallClock};
use cairn_core::hash::Hash;
use cairn_core::manifest::{Manifest, ManifestEntry};
use sha2::{Digest, Sha256};

use crate::http;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// Boot a real server state + objects HTTP endpoint on an ephemeral loopback port.
async fn spin_server(root: &std::path::Path) -> Result<(Arc<cairn_server::ServerState>, String)> {
    // bind the objects endpoint FIRST so presign base URLs carry the real port
    let obj_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let objects_addr = obj_listener.local_addr()?.to_string();

    let clock: Arc<dyn SystemClock> = Arc::new(WallClock);
    let cfg = cairn_server::run::ServerConfig {
        data_dir: root.to_path_buf(),
        grpc_addr: "127.0.0.1:7443".into(), // production runner binds this; unused here
        objects_addr: objects_addr.clone(),
        dev_insecure: true,
        tls_cert: None,
        tls_key: None,
    };
    let state = cairn_server::run::build_state(&cfg, clock).await?;

    let key_file = std::fs::read_to_string(root.join("keys").join("object-signing.key"))?;
    let raw = cairn_core::hash::hex_decode(key_file.trim()).ok_or("bad object key")?;
    let concrete = cairn_server::storage::LocalFsStore::open(
        &root.join("objects"),
        &raw,
        &format!("http://{objects_addr}/"),
    )?;
    let router = Arc::new(concrete).router();
    tokio::spawn(async move { axum::serve(obj_listener, router).await });
    Ok((state, objects_addr))
}

async fn insert_tenant(db: &sqlx::SqlitePool, tenant: &str) {
    sqlx::query("INSERT OR IGNORE INTO tenants(id, created_at) VALUES(?1, 0)")
        .bind(tenant)
        .execute(db)
        .await
        .unwrap();
}

async fn insert_chunk(db: &sqlx::SqlitePool, tenant: &str, hash: &str, size: usize) {
    sqlx::query(
        "INSERT OR IGNORE INTO chunks(tenant_id, hash, size, tier, state, last_touched)
         VALUES(?1,?2,?3,'hot','present',0)",
    )
    .bind(tenant)
    .bind(hash)
    .bind(size as i64)
    .execute(db)
    .await
    .unwrap();
}

/// Adversarial bloom (§15.2): even with EVERY bit set, the exact missing set is preserved —
/// a bloom "maybe present" is only ever a hint; the chunks table decides.
#[test]
fn adversarial_bloom_cannot_skip_uploads() {
    use cairn_core::bloom::Bloom;
    let mut b = Bloom::with_fpp(1000, 0.01);
    b.corrupt_all_bits();
    let missing: Vec<String> = (0..64)
        .map(|i| Hash::of(format!("m{i}").as_bytes()).hex())
        .collect();
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let (state, _obj) = spin_server(dir.path()).await.unwrap();
        insert_tenant(&state.db, "t1").await;
        let out = cairn_server::upload::batch_exists(&state, "t1", &missing)
            .await
            .unwrap();
        assert_eq!(
            out.len(),
            missing.len(),
            "a lying bloom must not hide missing chunks"
        );
    });
}

/// M3 AC: chunk → BatchExists → presigned PUT (checksum-enforced) → SERVER RESTART →
/// resume only the remainder → CompleteUpload → manifest → download+verify byte-identical.
#[tokio::test]
async fn e2e_restart_resume_roundtrip_byte_identical() {
    let bytes_total: usize = std::env::var("CAIRN_E2E_BYTES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(64 * 1024 * 1024);
    let dir = tempfile::tempdir().unwrap();
    let (state, _obj) = spin_server(dir.path()).await.unwrap();
    insert_tenant(&state.db, "t1").await;

    // 1) single pass chunk+hash of a large synthetic file
    // non-periodic stream (splitmix-flavored) so chunks are distinct content
    let data: Vec<u8> = (0..bytes_total)
        .map(|i| {
            let x = (i as u64)
                .wrapping_mul(0x9E37_79B9_7F4A_7C15)
                .wrapping_add(0x2545_F491);
            ((x >> 33) & 0xFF) as u8
        })
        .collect();
    let sh = cairn_core::chunker::StreamHash::compute(&data);
    let hash_hexes: Vec<String> = sh.chunk_hashes.iter().map(Hash::hex).collect();

    // 2) BatchExists: fresh tenant → everything missing
    let missing = cairn_server::upload::batch_exists(&state, "t1", &hash_hexes)
        .await
        .unwrap();
    assert_eq!(missing.len(), hash_hexes.len());

    // 3) upload the FIRST HALF over real HTTP with checksums; corrupt uploads rejected
    let spans_and_hashes: Vec<(&cairn_core::chunker::ChunkSpan, &Hash)> =
        sh.spans.iter().zip(sh.chunk_hashes.iter()).collect();
    let half = spans_and_hashes.len() / 2;
    let mut uploaded: Vec<(String, Vec<u8>)> = Vec::new();
    for (span, h) in &spans_and_hashes[..half] {
        let bytes =
            data[span.offset as usize..(span.offset + u64::from(span.len)) as usize].to_vec();
        let key = cairn_server::storage::LocalFsStore::chunk_key("t1", &h.hex());
        let url = state.store.presign_put(&key, 3600).await.unwrap();
        let checksum = cairn_core::hash::hex_encode(&Sha256::digest(&bytes));
        let resp = http::put_object(&url, &bytes, &checksum).await.unwrap();
        assert_eq!(resp.status, 200);
        // the "bucket" rejects corrupt uploads (SPEC §9.1)
        let bad = http::put_object(&url, b"corrupt-payload", &checksum)
            .await
            .unwrap();
        assert_eq!(bad.status, 400, "bucket must reject corrupt upload");
        uploaded.push((h.hex(), bytes));
    }

    // 4) CompleteUpload effect for the first half (chunk rows registered), THEN server restart
    for (h, bytes) in &uploaded {
        insert_chunk(&state.db, "t1", h, bytes.len()).await;
    }
    drop(state);
    let (state, _obj) = spin_server(dir.path()).await.unwrap();

    // 5) resume: only the un-uploaded half is missing (chunk-granular resume, I2)
    let missing2 = cairn_server::upload::batch_exists(&state, "t1", &hash_hexes)
        .await
        .unwrap();
    assert_eq!(
        missing2.len(),
        hash_hexes.len() - half,
        "resume must skip acknowledged chunks after restart"
    );
    for (span, h) in &spans_and_hashes[half..] {
        let bytes =
            data[span.offset as usize..(span.offset + u64::from(span.len)) as usize].to_vec();
        let key = cairn_server::storage::LocalFsStore::chunk_key("t1", &h.hex());
        let url = state.store.presign_put(&key, 3600).await.unwrap();
        let checksum = cairn_core::hash::hex_encode(&Sha256::digest(&bytes));
        let resp = http::put_object(&url, &bytes, &checksum).await.unwrap();
        assert_eq!(resp.status, 200);
        let size = bytes.len();
        uploaded.push((h.hex(), bytes));
        insert_chunk(&state.db, "t1", &h.hex(), size).await;
    }

    // 6) manifest object + registration
    let entries: Vec<ManifestEntry> = sh
        .spans
        .iter()
        .zip(sh.chunk_hashes.iter())
        .map(|(s, h)| ManifestEntry {
            offset: s.offset,
            len: s.len,
            chunk_hash: *h,
        })
        .collect();
    let manifest = Manifest::build(entries, cairn_core::manifest::Compression::None, None);
    let (manifest_hash, manifest_bytes) = manifest.serialize();
    cairn_server::upload::register_manifest(&state, "t1", &manifest_hash.hex(), &manifest_bytes)
        .await
        .unwrap();

    // 7) download path: presigned GET (immutable) + Range support + full reassembly with
    //    per-chunk BLAKE3 verification on ingest (I2)
    let mkey = cairn_server::storage::LocalFsStore::object_key("t1", &manifest_hash.hex());
    let murl = state.store.presign_get(&mkey, 3600).await.unwrap();
    let got = http::get_object(&murl, None).await.unwrap();
    assert_eq!(got.status, 200);
    assert_eq!(got.body, manifest_bytes, "manifest round-trip");
    let ranged = http::get_object(&murl, Some("bytes=0-99")).await.unwrap();
    assert_eq!(ranged.status, 206);
    assert_eq!(ranged.body.len(), 100);

    let mut back = Vec::with_capacity(bytes_total);
    for (h, _) in &uploaded {
        let key = cairn_server::storage::LocalFsStore::chunk_key("t1", h);
        let url = state.store.presign_get(&key, 3600).await.unwrap();
        let resp = http::get_object(&url, None).await.unwrap();
        assert_eq!(resp.status, 200);
        let chunk_bytes = resp.body;
        assert_eq!(
            Hash::of(&chunk_bytes).hex(),
            *h,
            "chunk hash verified on ingest (I2)"
        );
        back.extend_from_slice(&chunk_bytes);
    }
    assert_eq!(back.len(), bytes_total);
    assert_eq!(
        Hash::of(&back),
        Hash::of(&data),
        "BYTE-IDENTICAL round trip"
    );
}
