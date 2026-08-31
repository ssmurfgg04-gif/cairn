//! Server assembly + entry (SPEC §12): stateless tonic services, object-store HTTP endpoint,
//! (jobs attach at M6). Restart-safe by construction: all state in the SQLite-compatible DB
//! and the object store.

use std::sync::Arc;

use cairn_core::bloom::Bloom;
use cairn_core::clock::SystemClock;
use cairn_core::{CairnError, ErrorKind};

use crate::storage::LocalFsStore;
use crate::ServerState;

/// Server configuration.
pub struct ServerConfig {
    /// Data dir (meta.db, objects/, keys/).
    pub data_dir: std::path::PathBuf,
    /// gRPC listen address.
    pub grpc_addr: String,
    /// Object-store HTTP address (dev backend; production uses real buckets).
    pub objects_addr: String,
    /// Dev bootstrap (enroll codes without an admin token).
    pub dev_insecure: bool,
    /// TLS server cert (PEM) for the gRPC endpoint (port 7443). When set, the metadata
    /// plane is served over TLS — remote plaintext gRPC is a beta blocker.
    pub tls_cert: Option<std::path::PathBuf>,
    /// TLS server key (PEM).
    pub tls_key: Option<std::path::PathBuf>,
}

/// Build the server state from config.
pub async fn build_state(
    cfg: &ServerConfig,
    clock: Arc<dyn SystemClock>,
) -> Result<Arc<ServerState>, CairnError> {
    let db = crate::db::open(&cfg.data_dir.join("meta.db")).await?;
    let auth =
        crate::auth::Authenticator::load_or_create(&cfg.data_dir.join("keys"), clock.clone())?;
    let signing_key = read_or_create_object_key(&cfg.data_dir)?;
    // Backend selection (ADR-0005): a complete `CAIRN_S3_*` environment wires the
    // real SigV4 bucket backend; otherwise the dev LocalFs store serves. No
    // half-wired states — an incomplete S3 env must not silently mix backends.
    let store: Arc<dyn crate::storage::ObjectStore> =
        match crate::storage::S3ObjectStore::from_env() {
            Some(s3) => {
                tracing::info!(backend = "s3", "object store: real SigV4 bucket backend");
                Arc::new(s3)
            }
            None => {
                tracing::info!(backend = "local-fs", "object store: dev local filesystem");
                Arc::new(LocalFsStore::open(
                    &cfg.data_dir.join("objects"),
                    &signing_key,
                    &format!("http://{}/", cfg.objects_addr),
                )?)
            }
        };
    let state = ServerState {
        db,
        auth,
        store,
        bloom: tokio::sync::RwLock::new(Bloom::with_fpp(100_000, 0.01)),
        clock,
        dev_insecure: cfg.dev_insecure,
    };
    state.migrate().await?;
    crate::jobs::rebuild_bloom(&state).await;
    Ok(Arc::new(state))
}

fn read_or_create_object_key(data_dir: &std::path::Path) -> Result<Vec<u8>, CairnError> {
    let path = data_dir.join("keys").join("object-signing.key");
    if let Ok(s) = std::fs::read_to_string(&path) {
        return cairn_core::hash::hex_decode(s.trim())
            .ok_or_else(|| CairnError::new(ErrorKind::Io, "bad object key hex"));
    }
    let key: [u8; 32] = rand_key();
    std::fs::write(&path, cairn_core::hash::hex_encode(&key))
        .map_err(|e| CairnError::new(ErrorKind::Io, format!("write object key: {e}")))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(key.to_vec())
}

fn rand_key() -> [u8; 32] {
    // entropy: uuid v7 (has random bits) + clock nanos, stretched through BLAKE3 via
    // cairn_core. Dev-signing quality; production provisions keys via env/KMS (runbook).
    let mut seed = Vec::new();
    seed.extend_from_slice(uuid::Uuid::now_v7().as_bytes());
    seed.extend_from_slice(
        &std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0)
            .to_le_bytes(),
    );
    cairn_core::hash::Hash::of(&seed).0
}

/// Run the server (blocks). Kill-switch flags, jobs and canary attach at M6–M7.
pub async fn run(cfg: ServerConfig) -> Result<(), CairnError> {
    let clock: Arc<dyn SystemClock> = Arc::new(cairn_core::clock::WallClock);
    let state = build_state(&cfg, clock).await?;
    tracing::info!(
        grpc = %cfg.grpc_addr,
        objects = %cfg.objects_addr,
        backend = state.store.name(),
        dev_insecure = cfg.dev_insecure,
        "cairn server starting"
    );

    // object-store HTTP endpoint (dev backend; production = real bucket presigning)
    let objects_router = {
        let concrete = LocalFsStore::open(
            &cfg.data_dir.join("objects"),
            &read_or_create_object_key(&cfg.data_dir)?,
            &format!("http://{}/", cfg.objects_addr),
        )?;
        Arc::new(concrete).router()
    };
    let obj_listener = tokio::net::TcpListener::bind(&cfg.objects_addr)
        .await
        .map_err(|e| CairnError::new(ErrorKind::Io, format!("bind objects: {e}")))?;
    let objects = tokio::spawn(async move { axum::serve(obj_listener, objects_router).await });

    // TLS on the metadata plane (7443): self-signed dev certs via `just tls-dev-cert`;
    // production uses real certs. Objects HTTP stays dev-only (real buckets are HTTPS).
    let mut grpc = tonic::transport::Server::builder();
    if let (Some(cert), Some(key)) = (&cfg.tls_cert, &cfg.tls_key) {
        let identity = tonic::transport::Identity::from_pem(
            std::fs::read(cert)
                .map_err(|e| CairnError::new(ErrorKind::Io, format!("tls cert: {e}")))?,
            std::fs::read(key)
                .map_err(|e| CairnError::new(ErrorKind::Io, format!("tls key: {e}")))?,
        );
        grpc = grpc
            .tls_config(tonic::transport::ServerTlsConfig::new().identity(identity))
            .map_err(|e| CairnError::new(ErrorKind::Io, format!("tls config: {e}")))?;
        tracing::info!("metadata plane TLS: ENABLED");
    } else {
        tracing::warn!("metadata plane TLS: DISABLED (plaintext gRPC — dev only)");
    }
    let grpc = grpc
        .add_service(cairn_proto::pb::journal_server::JournalServer::new(
            crate::services::JournalSvc {
                state: Arc::clone(&state),
            },
        ))
        .add_service(cairn_proto::pb::lease_server::LeaseServer::new(
            crate::services::LeaseSvc {
                state: Arc::clone(&state),
            },
        ))
        .add_service(cairn_proto::pb::upload_server::UploadServer::new(
            crate::services::UploadSvc {
                state: Arc::clone(&state),
            },
        ))
        .add_service(cairn_proto::pb::download_server::DownloadServer::new(
            crate::services::DownloadSvc {
                state: Arc::clone(&state),
            },
        ))
        .add_service(cairn_proto::pb::auth_server::AuthServer::new(
            crate::services::AuthSvc {
                state: Arc::clone(&state),
            },
        ))
        .add_service(cairn_proto::pb::project_server::ProjectServer::new(
            crate::services::ProjectSvc {
                state: Arc::clone(&state),
            },
        ))
        .serve(
            cfg.grpc_addr
                .parse()
                .map_err(|e| CairnError::new(ErrorKind::Io, format!("addr: {e}")))?,
        );

    fn as_cairn<E: std::fmt::Display>(r: Result<(), E>, what: &str) -> Result<(), CairnError> {
        r.map_err(|e| CairnError::new(ErrorKind::Io, format!("{what}: {e}")))
    }

    tokio::select! {
        r = grpc => as_cairn(r, "grpc"),
        r = objects => match r {
            Ok(inner) => as_cairn(inner, "objects"),
            Err(e) => Err(CairnError::new(ErrorKind::Io, format!("objects task: {e}"))),
        },
        _ = tokio::signal::ctrl_c() => Ok(()),
    }
}
