//! Cairn storage server (SPEC §12): metadata plane services, control-plane jobs, data-plane
//! presigning. Stateless services; jobs are idempotent workers with a leader lease.
//!
//! I3 tenancy: every lookup carries tenant_id — enforced structurally (composite keys, scoped
//! keys) and tested (cross-tenant isolation tests).

pub mod auth;
pub mod db;
pub mod error_map;
pub mod fold;
pub mod journal;
pub mod leases;
pub mod run;
pub mod services;
pub mod storage;
pub mod upload;
pub mod jobs;

use std::sync::Arc;

use cairn_core::clock::SystemClock;
use cairn_core::CairnError;

/// Shared server state handed to every service.
pub struct ServerState {
    /// SQLite-compatible pool (dev: SQLite; prod: libsql — DDL is dialect-portable).
    pub db: sqlx::SqlitePool,
    /// Device-token authenticator (PASETO v4.public, ADR-0011).
    pub auth: auth::Authenticator,
    /// Object store backend (ADR-0005).
    pub store: Arc<dyn storage::ObjectStore>,
    /// Optional production SigV4 presigner (S3/R2/B2-compatible endpoints).
    pub s3: Option<storage::SigV4Presigner>,
    /// Per-tenant bloom negative pre-filter (rebuilt by the control-plane job).
    pub bloom: tokio::sync::RwLock<cairn_core::bloom::Bloom>,
    /// Clock (server-authoritative, I4).
    pub clock: Arc<dyn SystemClock>,
    /// Dev bootstrap mode: enrollment codes without an admin token (dev only, logged loudly).
    pub dev_insecure: bool,
}

impl ServerState {
    /// Run the DDL migrations (idempotent; safe at every boot — restart loses nothing).
    pub async fn migrate(&self) -> Result<(), CairnError> {
        db::migrate(&self.db).await
    }

    /// Authenticate a metadata-plane call from its `authorization` header (audited on denial).
    pub async fn authenticate_metadata<T>(
        &self,
        request: &tonic::Request<T>,
    ) -> Result<auth::DeviceIdentity, CairnError> {
        let bearer = request
            .metadata()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        match self.auth.authenticate(&self.db, &bearer).await {
            Ok(id) => Ok(id),
            Err(e) => {
                db::audit(&self.db, &self.clock, "", "unknown", "authz.denial", "metadata", &e.message).await;
                Err(e)
            }
        }
    }

    /// Audit an explicit authorization denial with actor context.
    pub async fn audit_denial(&self, identity: &auth::DeviceIdentity, action: &str) {
        db::audit(
            &self.db,
            &self.clock,
            &identity.tenant_id,
            &identity.device_id,
            "authz.denial",
            action,
            "denied",
        )
        .await;
    }
}
