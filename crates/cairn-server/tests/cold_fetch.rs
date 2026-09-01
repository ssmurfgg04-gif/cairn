//! WO6-4 COLD-FETCH instrumentation test: the REAL download path —
//! `GetDownloadUrl` (presign RPC) → presigned GET against the live objects
//! endpoint — measured by `GrpcPlane::measure_cold_fetch`, the exact fn the
//! soak's gate S4 uses. First byte must land; the body must be byte-identical
//! and complete; presign must be a real, fetchable URL (no mock planes).

use std::sync::Arc;

use cairn_core::clock::{SystemClock, WallClock};
use cairn_core::hash::Hash;
use cairn_server::storage::{LocalFsStore, ObjectStore};

#[tokio::test]
async fn cold_fetch_first_byte_through_the_real_plane() {
    // ---- objects endpoint on an ephemeral port; the store's presigned URLs
    //      must point at the ACTUAL bound port (production passes a fixed addr) ----
    let obj_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let obj_port = obj_listener.local_addr().unwrap().port();
    let base = format!("http://127.0.0.1:{obj_port}/");
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(
        LocalFsStore::open(&dir.path().join("objects"), b"test-object-key", &base).unwrap(),
    );

    // ---- server state mirroring tests_support::state_at, but with the real base URL ----
    let db = cairn_server::db::open(&dir.path().join("meta.db"))
        .await
        .unwrap();
    cairn_server::db::migrate(&db).await.unwrap();
    let auth = cairn_server::auth::Authenticator::load_or_create(
        &dir.path().join("keys"),
        Arc::new(WallClock),
    )
    .unwrap();
    let state = Arc::new(cairn_server::ServerState {
        db,
        auth,
        store: Arc::clone(&store) as Arc<dyn cairn_server::storage::ObjectStore>,
        bloom: tokio::sync::RwLock::new(cairn_core::bloom::Bloom::empty()),
        clock: Arc::new(WallClock) as Arc<dyn SystemClock>,
        dev_insecure: true,
    });

    // ---- serve objects HTTP + the full gRPC stack (both ephemeral) ----
    let objects_jh = tokio::spawn({
        let router = Arc::clone(&store).router();
        async move { axum::serve(obj_listener, router).await }
    });
    let grpc_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let grpc_port = grpc_listener.local_addr().unwrap().port();
    // per-service clones; the test keeps its own `state` for the DB inserts below
    let st_journal = Arc::clone(&state);
    let st_lease = Arc::clone(&state);
    let st_upload = Arc::clone(&state);
    let st_download = Arc::clone(&state);
    let st_auth = Arc::clone(&state);
    let st_project = Arc::clone(&state);
    let grpc_jh = tokio::spawn(async move {
        let _ = tonic::transport::Server::builder()
            .add_service(cairn_proto::pb::journal_server::JournalServer::new(
                cairn_server::services::JournalSvc { state: st_journal },
            ))
            .add_service(cairn_proto::pb::lease_server::LeaseServer::new(
                cairn_server::services::LeaseSvc { state: st_lease },
            ))
            .add_service(cairn_proto::pb::upload_server::UploadServer::new(
                cairn_server::services::UploadSvc { state: st_upload },
            ))
            .add_service(cairn_proto::pb::download_server::DownloadServer::new(
                cairn_server::services::DownloadSvc { state: st_download },
            ))
            .add_service(cairn_proto::pb::auth_server::AuthServer::new(
                cairn_server::services::AuthSvc { state: st_auth },
            ))
            .add_service(cairn_proto::pb::project_server::ProjectServer::new(
                cairn_server::services::ProjectSvc { state: st_project },
            ))
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(
                grpc_listener,
            ))
            .await;
    });

    // ---- tenant + device token (the real PASETO the plane presents) ----
    sqlx::query("INSERT OR IGNORE INTO tenants(id, created_at) VALUES('t1',0)")
        .execute(&state.db)
        .await
        .unwrap();
    let code = state
        .auth
        .enroll_code("t1", "probe@studio.tv", "sync", 60_000)
        .await;
    let (token, identity) = state
        .auth
        .enroll(&state.db, &code, "pk-test", "cold-fetch-probe")
        .await
        .expect("enroll");

    // ---- one stored 1 MiB chunk (the soak picks the largest; here it IS the only) ----
    let chunk: Vec<u8> = (0..1024 * 1024).map(|i| (i % 251) as u8).collect();
    let chunk_hash = Hash::of(&chunk);
    store
        .put(&LocalFsStore::chunk_key("t1", &chunk_hash.hex()), &chunk)
        .await
        .unwrap();
    sqlx::query(
        "INSERT OR IGNORE INTO chunks(tenant_id, hash, size, tier, state, last_touched)
         VALUES('t1',?1,?2,'hot','present',0)",
    )
    .bind(chunk_hash.hex())
    .bind(chunk.len() as i64)
    .execute(&state.db)
    .await
    .unwrap();

    // ---- the measurement (same fn the soak's gate S4 drives) ----
    let plane = cairn_sync::plane_grpc::GrpcPlane::connect(
        &format!("http://127.0.0.1:{grpc_port}"),
        &token,
        &identity.tenant_id,
        None,
    )
    .await
    .expect("plane connect");
    let sample = plane
        .measure_cold_fetch("t1", &chunk_hash.hex())
        .await
        .expect("cold fetch");

    assert_eq!(
        std::format!("{}", sample.bytes),
        chunk.len().to_string(),
        "body must be complete"
    );
    assert!(sample.first_byte_ms > 0.0, "first byte must be timed");
    assert!(
        sample.total_ms >= sample.first_byte_ms,
        "total >= first byte ({} vs {})",
        sample.total_ms,
        sample.first_byte_ms
    );
    assert!(
        sample.presign_ms > 0.0 && sample.presign_ms <= sample.total_ms,
        "presign is the first leg of the fetch"
    );
    println!(
        "cold fetch in-process: first_byte {:.2} ms | presign {:.2} ms | total {:.2} ms | {} bytes",
        sample.first_byte_ms, sample.presign_ms, sample.total_ms, sample.bytes
    );

    grpc_jh.abort();
    objects_jh.abort();
}
