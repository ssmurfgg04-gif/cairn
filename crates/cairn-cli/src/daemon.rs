//! Local daemon (SPEC §11): single process, localhost gRPC ctl on :17777 (token-gated for
//! mutations), local dashboard on :17778 (ADR-0009). Built as the frozen ctl contract's first
//! reference client.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use cairn_core::clock::WallClock;
use cairn_proto::pb::ctl_diagnostics_server::{CtlDiagnostics, CtlDiagnosticsServer};
use cairn_proto::pb::ctl_projects_server::{CtlProjects, CtlProjectsServer};
use cairn_proto::pb::ctl_status_server::{CtlStatus, CtlStatusServer};
use cairn_proto::pb::{
    Ack, AttachRootRequest, AttachRootResponse, DetachRootRequest, DoctorCheck, DoctorReport,
    DoctorRequest, FlagInfo, GetFlagsRequest, GetFlagsResponse, ListProjectsCtlRequest,
    ListProjectsCtlResponse, ProjectInfoCtl, ProjectStatus, SetFlagRequest, StatusRequest,
    StatusResponse,
};
use tokio::sync::RwLock;
use tonic::{Request, Response, Status};

use crate::doctor;
use crate::projects;

/// Shared daemon state.
pub struct DaemonState {
    /// Data directory.
    pub home: PathBuf,
    /// Start time.
    pub started: Instant,
    /// Kill switches (config_flags mirror; daemon-side copy, server-side authoritative).
    pub flags: RwLock<Vec<(String, String)>>,
}

impl DaemonState {
    fn new(home: PathBuf) -> Self {
        DaemonState {
            home,
            started: Instant::now(),
            flags: RwLock::new(default_flags()),
        }
    }
}

fn default_flags() -> Vec<(String, String)> {
    vec![
        ("packing_enabled".into(), "true".into()),
        ("tiering_enabled".into(), "true".into()),
        ("delta_fold_enabled".into(), "true".into()),
        ("compression_enabled".into(), "true".into()),
        ("placeholder_driver".into(), "native".into()),
        // chunk-input normalization: OFF until it soaks behind AttachRoot (flag-gated)
        ("normalize_containers".into(), "false".into()),
    ]
}

// ---------- ctl gRPC services ----------

pub struct CtlStatusSvc {
    pub state: Arc<DaemonState>,
}

#[tonic::async_trait]
impl CtlStatus for CtlStatusSvc {
    async fn status(
        &self,
        _request: Request<StatusRequest>,
    ) -> Result<Response<StatusResponse>, Status> {
        let rep = doctor::collect(&self.state.home);
        // attached projects: live runtimes first, then any durable binding (crash-resume
        // window where the loop hasn't spawned yet)
        let mut list: Vec<ProjectStatus> = Vec::new();
        {
            let map = projects::RUNTIMES.read().await;
            for rt in map.values() {
                let v = rt.view.read().await;
                list.push(ProjectStatus {
                    project_id: rt.project_id.clone(),
                    root_path: rt.workspace.to_string_lossy().into_owned(),
                    state: v.state.clone(),
                    pending_outbox: v.pending_outbox,
                    cursor: v.cursor,
                    files_synced: v.files_synced,
                });
            }
        }
        list.sort_by(|a, b| a.project_id.cmp(&b.project_id));
        Ok(Response::new(StatusResponse {
            version: format!("cairn {}", env!("CARGO_PKG_VERSION")),
            proto: cairn_proto::PROTO_VERSION,
            uptime_ms: u64::try_from(self.state.started.elapsed().as_millis()).unwrap_or(u64::MAX),
            projects: list,
            server_reachable: if rep.healthy() {
                "ok".into()
            } else {
                "degraded".into()
            },
        }))
    }
}

pub struct CtlDiagSvc {
    pub state: Arc<DaemonState>,
}

pub struct CtlProjectsSvc {
    pub state: Arc<DaemonState>,
}

#[tonic::async_trait]
impl CtlProjects for CtlProjectsSvc {
    async fn attach_root(
        &self,
        request: Request<AttachRootRequest>,
    ) -> Result<Response<AttachRootResponse>, Status> {
        let req = request.into_inner();
        let root = std::path::PathBuf::from(&req.root_path);
        let pid = projects::attach(
            &self.state.home,
            &root,
            if req.project_id.is_empty() {
                None
            } else {
                Some(req.project_id)
            },
            if req.server_addr.is_empty() {
                None
            } else {
                Some(req.server_addr)
            },
        )
        .await
        .map_err(|e| Status::failed_precondition(e.message))?;
        tracing::info!(project = %pid, root = %req.root_path, "attach_root accepted");
        Ok(Response::new(AttachRootResponse { project_id: pid }))
    }

    async fn detach_root(
        &self,
        request: Request<DetachRootRequest>,
    ) -> Result<Response<Ack>, Status> {
        let req = request.into_inner();
        projects::detach(&self.state.home, &req.project_id)
            .await
            .map_err(|e| Status::failed_precondition(e.message))?;
        Ok(Response::new(Ack { ok: true }))
    }

    async fn list_projects(
        &self,
        _request: Request<ListProjectsCtlRequest>,
    ) -> Result<Response<ListProjectsCtlResponse>, Status> {
        let mut list = Vec::new();
        {
            let map = projects::RUNTIMES.read().await;
            for rt in map.values() {
                let state = rt.view.read().await.state.clone();
                list.push(ProjectInfoCtl {
                    project_id: rt.project_id.clone(),
                    root_path: rt.workspace.to_string_lossy().into_owned(),
                    state,
                });
            }
        }
        list.sort_by(|a, b| a.project_id.cmp(&b.project_id));
        Ok(Response::new(ListProjectsCtlResponse { projects: list }))
    }
}

#[tonic::async_trait]
impl CtlDiagnostics for CtlDiagSvc {
    async fn doctor(
        &self,
        _request: Request<DoctorRequest>,
    ) -> Result<Response<DoctorReport>, Status> {
        let rep = doctor::collect(&self.state.home);
        Ok(Response::new(DoctorReport {
            checks: rep
                .checks
                .iter()
                .map(|c| DoctorCheck {
                    name: c.name.to_string(),
                    ok: c.ok,
                    detail: c.detail.clone(),
                    latency_ms: c.latency_ms,
                })
                .collect(),
            healthy: rep.healthy(),
        }))
    }

    async fn gc_shadow_report(
        &self,
        _request: Request<cairn_proto::pb::GcShadowReportRequest>,
    ) -> Result<Response<cairn_proto::pb::GcShadowReportResponse>, Status> {
        Err(Status::unimplemented(
            "GC shadow report runs against the storage server (M6); ctl forwards then",
        ))
    }

    async fn set_flag(
        &self,
        request: Request<SetFlagRequest>,
    ) -> Result<Response<cairn_proto::pb::Ack>, Status> {
        let req = request.into_inner();
        let mut flags = self.state.flags.write().await;
        let known = flags.iter_mut().find(|(n, _)| *n == req.name);
        match known {
            Some(slot) => {
                slot.1 = req.value.clone();
                // mirror into the store so the ENGINE sees it per pass (e.g.
                // normalize_containers is read on every process_file)
                if let Ok(store) = cairn_store::Store::open(&self.state.home, Arc::new(WallClock)) {
                    let _ = store.meta_set(&format!("flag:{}", req.name), &req.value);
                }
                tracing::info!(flag = %req.name, value = %req.value, "kill switch flipped (no restart)");
                Ok(Response::new(cairn_proto::pb::Ack { ok: true }))
            }
            None => Err(Status::not_found(format!("unknown flag {}", req.name))),
        }
    }

    async fn get_flags(
        &self,
        _request: Request<GetFlagsRequest>,
    ) -> Result<Response<GetFlagsResponse>, Status> {
        let flags = self.state.flags.read().await;
        Ok(Response::new(GetFlagsResponse {
            flags: flags
                .iter()
                .map(|(n, v)| FlagInfo {
                    name: n.clone(),
                    value: v.clone(),
                })
                .collect(),
        }))
    }
}

// ---------- daemon entry ----------

/// Run the daemon (M1 skeleton: ctl status/diagnostics live; project services attach at M4;
/// dashboard at UI phase; the process is SIGTERM/SIGKILL-safe by construction — all state is
/// durable in the client store before any ack).
pub async fn run(home: PathBuf, ctl_addr: String, ui_addr: String) -> anyhow::Result<()> {
    let state = Arc::new(DaemonState::new(home));
    // persist the ctl endpoint so CLI status/attach in THIS home find the right daemon
    // (multi-daemon machines run several ctl ports; 17777 is only the default)
    if let Ok(store) = cairn_store::Store::open(&state.home, Arc::new(WallClock)) {
        let _ = store.meta_set("ctl/addr", &format!("http://{ctl_addr}"));
    }
    tracing::info!(ctl_addr = %ctl_addr, ui_addr = %ui_addr, "cairn daemon starting");

    let status_svc = CtlStatusServer::new(CtlStatusSvc {
        state: Arc::clone(&state),
    });
    let diag_svc = CtlDiagnosticsServer::new(CtlDiagSvc {
        state: Arc::clone(&state),
    });
    let projects_svc = CtlProjectsServer::new(CtlProjectsSvc {
        state: Arc::clone(&state),
    });

    // resume any durably-bound workspaces from a previous run (kill -9 safe, I2)
    let resume_home = state.home.clone();
    tokio::spawn(async move {
        let n = projects::resume_all(&resume_home).await;
        if n > 0 {
            tracing::info!(resumed = n, "re-attached bound workspaces");
        }
    });

    let ctl = tonic::transport::Server::builder()
        .add_service(status_svc)
        .add_service(diag_svc)
        .add_service(projects_svc)
        .serve(ctl_addr.parse()?);

    let ui = crate::dashboard::serve(ui_addr, Arc::clone(&state));

    // The daemon is safe to kill -9 at any point (I2): nothing here owns uncommitted state.
    tokio::select! {
        r = ctl => r?,
        r = ui => r?,
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("daemon: ctrl-c, exiting cleanly (all acked state already durable)");
        }
    }
    Ok(())
}

// ---------- login/logout (device enrollment; keychain storage per §13) ----------

/// Enroll + persist the FULL identity into the home store (daemon attach path needs
/// device_id/tenant_id, which the keychain payload alone does not carry).
pub async fn login_full(
    home: &std::path::Path,
    server: &str,
    code: &str,
    name: &str,
    allow_plaintext_file: bool,
    tls_ca: Option<String>,
) -> anyhow::Result<()> {
    // ensure local store exists first (doctor-friendly)
    let store = cairn_store::Store::open(home, Arc::new(WallClock))?;
    let server_url = if server.starts_with("https://") || server.starts_with("http://") {
        server.to_string()
    } else {
        format!("http://{server}")
    };
    // TLS-aware dial (self-signed dev CAs honored via --ca)
    let channel =
        cairn_sync::plane_grpc::connect_channel(&server_url, tls_ca.as_deref().map(str::as_bytes))
            .await
            .map_err(|e| anyhow::anyhow!("cannot reach server {server}: {e} — is it running?"))?;
    let mut auth = cairn_proto::pb::auth_client::AuthClient::new(channel);
    let resp = auth
        .enroll(cairn_proto::pb::EnrollRequest {
            code: code.to_string(),
            device_pubkey: "dev-local".into(), // device keypair generation lands with server auth (M2)
            device_name: name.to_string(),
        })
        .await?;
    let inner = resp.into_inner();
    let device_id = inner.device_id.clone();
    crate::projects::save_identity(
        &store,
        &crate::projects::Identity {
            server_url,
            token: inner.paseto.clone(),
            device_id,
            tenant_id: inner.tenant_id,
            tls_ca,
        },
    )
    .map_err(|e| anyhow::anyhow!("persist identity: {e}"))?;
    let _ = store_token(server, &inner.paseto, allow_plaintext_file);
    tracing::info!(server = %server, device = %inner.device_id, "device enrolled; identity persisted");
    Ok(())
}

/// Remove the locally stored token (server-side revocation is an admin action via ctl).
pub fn logout(home: &std::path::Path) {
    match keyring_entry() {
        Ok(e) => {
            let _ = e.delete_credential();
        }
        Err(e) => tracing::warn!("keychain unavailable at logout: {e}"),
    }
    let _ = std::fs::remove_file(token_file());
    if let Ok(store) = cairn_store::Store::open(home, Arc::new(WallClock)) {
        crate::projects::clear_identity(&store);
    }
}

fn keyring_entry() -> Result<keyring::Entry, String> {
    keyring::Entry::new("cairn", "device-token").map_err(|e| format!("{e}"))
}

fn token_file() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".cairn-token-dev")
}

fn store_token(server: &str, token: &str, allow_plaintext_file: bool) -> anyhow::Result<()> {
    let payload = format!("{server}|{token}");
    match keyring_entry() {
        Ok(e) => e
            .set_password(&payload)
            .map_err(|e| anyhow::anyhow!("keychain: {e}")),
        Err(msg) => {
            if allow_plaintext_file {
                tracing::warn!(
                    "keychain unavailable ({msg}); using 0600 dev file (NOT for production)"
                );
                let path = token_file();
                std::fs::write(&path, &payload)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
                }
                Ok(())
            } else {
                anyhow::bail!("no credential store: {msg}")
            }
        }
    }
}

/// Read the stored token if any (server: token).
#[must_use]
#[allow(dead_code)] // used by future daemon-server attach wiring (M8+ ctl integration)
pub fn load_token() -> Option<(String, String)> {
    if let Ok(e) = keyring_entry() {
        if let Ok(p) = e.get_password() {
            let mut parts = p.splitn(2, '|');
            if let (Some(s), Some(t)) = (parts.next(), parts.next()) {
                return Some((s.to_string(), t.to_string()));
            }
        }
    }
    let p = token_file();
    std::fs::read_to_string(&p).ok().and_then(|s| {
        let mut parts = s.splitn(2, '|');
        Some((
            parts.next()?.to_string(),
            parts.next().unwrap_or("").to_string(),
        ))
    })
}
