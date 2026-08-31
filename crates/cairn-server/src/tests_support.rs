//! Test support: a server state rooted at a directory (test-only helper, kept pub for
//! workspace integration tests).

use std::sync::Arc;

/// Build a ready state (DB migrated) rooted at `dir`.
pub async fn state_at(dir: &std::path::Path) -> Arc<crate::ServerState> {
    let db = crate::db::open(&dir.join("meta.db")).await.unwrap();
    crate::db::migrate(&db).await.unwrap();
    let auth = crate::auth::Authenticator::load_or_create(
        &dir.join("keys"),
        Arc::new(cairn_core::clock::WallClock),
    )
    .unwrap();
    let store = crate::storage::LocalFsStore::open(
        &dir.join("objects"),
        b"test-object-key",
        "http://127.0.0.1:1/",
    )
    .unwrap();
    let state = crate::ServerState {
        db,
        auth,
        store: Arc::new(store),
        bloom: tokio::sync::RwLock::new(cairn_core::bloom::Bloom::empty()),
        clock: Arc::new(cairn_core::clock::WallClock),
        dev_insecure: true,
    };
    Arc::new(state)
}
