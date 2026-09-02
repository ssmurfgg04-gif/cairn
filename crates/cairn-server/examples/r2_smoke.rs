//! Live object-store smoke (R2/S3/B2): exercises the production backend end-to-end —
//! `from_env` → `ensure_bucket` → put/get/head roundtrip → presigned GET → delete.
//!
//! Run with a complete `CAIRN_S3_*` environment (see `S3ObjectStore` docs):
//! ```text
//! cargo run -p cairn-server --example r2_smoke
//! ```
//! Exits 0 only when every op round-trips byte-exact. Incomplete env exits 0 with an
//! "idle" note (the example is a verification tool, not a test gate).

use cairn_server::storage::{ObjectStore, S3ObjectStore};

#[tokio::main]
async fn main() {
    let Some(store) = S3ObjectStore::from_env() else {
        eprintln!(
            "r2_smoke: incomplete CAIRN_S3_* env — nothing to verify (this is not a failure)"
        );
        std::process::exit(0);
    };
    eprintln!("backend: {}", store.name());

    store
        .ensure_bucket()
        .await
        .expect("ensure_bucket (create-or-verify)");

    let key = "smoke/r2-roundtrip/manifest.bin";
    let payload: Vec<u8> = (0..64 * 1024u32).map(|i| (i % 253) as u8).collect();

    store.put(key, &payload).await.expect("put");
    let got = store.get(key).await.expect("get");
    assert_eq!(got, payload, "roundtrip mismatch");

    let size = store.head(key).await.expect("head");
    assert_eq!(size, payload.len() as u64, "head size mismatch");

    // presigned GET must serve the same bytes (the client-side chunk path)
    let url = store.presign_get(key, 600).await.expect("presign_get");
    let resp = reqwest::get(&url).await.expect("presigned fetch");
    assert!(
        resp.status().is_success(),
        "presigned GET status {}",
        resp.status()
    );
    let bytes = resp.bytes().await.expect("presigned body");
    assert_eq!(&bytes[..], &payload[..], "presigned roundtrip mismatch");

    store.delete(key).await.expect("delete");
    assert!(
        store.head(key).await.is_err(),
        "head after delete must fail"
    );

    eprintln!("r2_smoke: ALL OK (ensure_bucket, put, get, head, presigned GET, delete)");
}
