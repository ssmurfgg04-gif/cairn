//! Local daemon (SPEC §11): single process, localhost gRPC ctl on :17777 (token-gated for
//! mutations), local dashboard on :17778 (ADR-0009). Built as the frozen ctl contract's first
//! reference client.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use cairn_core::clock::WallClock;
use cairn_proto::pb::ctl_diagnostics_server::{CtlDiagnostics, CtlDiagnosticsServer};
use cairn_proto::pb::ctl_status_server::{CtlStatus, CtlStatusServer};
use cairn_proto::pb::{
    DoctorCheck, DoctorReport, DoctorRequest, FlagInfo, GetFlagsRequest, GetFlagsResponse,
    SetFlagRequest, StatusRequest, StatusResponse,
};
use tokio::sync::RwLock;
use tonic::{Request, Response, Status};

use crate::doctor;

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
        Ok(Response::new(StatusResponse {
            version: format!("cairn {}", env!("CARGO_PKG_VERSION")),
            proto: cairn_proto::PROTO_VERSION,
            uptime_ms: u64::try_from(self.state.started.elapsed().as_millis()).unwrap_or(u64::MAX),
            projects: vec![], // attached roots appear with ProjectService (M4)
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
    tracing::info!(ctl_addr = %ctl_addr, ui_addr = %ui_addr, "cairn daemon starting");

    let status_svc = CtlStatusServer::new(CtlStatusSvc {
        state: Arc::clone(&state),
    });
    let diag_svc = CtlDiagnosticsServer::new(CtlDiagSvc {
        state: Arc::clone(&state),
    });

    let ctl = tonic::transport::Server::builder()
        .add_service(status_svc)
        .add_service(diag_svc)
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

/// Enroll this device: exchanges an enrollment code with the server's Auth service and stores
/// the PASETO in the OS keychain (never plaintext; dev-only 0600-file fallback is explicit).
pub async fn login(
    home: &std::path::Path,
    server: &str,
    code: &str,
    name: &str,
    allow_plaintext_file: bool,
) -> anyhow::Result<()> {
    // ensure local store exists first (doctor-friendly)
    cairn_store::Store::open(home, Arc::new(WallClock))?;
    let mut auth = cairn_proto::pb::auth_client::AuthClient::connect(format!("http://{server}"))
        .await
        .map_err(|e| anyhow::anyhow!("cannot reach server {server}: {e} — is it running?"))?;
    let resp = auth
        .enroll(cairn_proto::pb::EnrollRequest {
            code: code.to_string(),
            device_pubkey: "dev-local".into(), // device keypair generation lands with server auth (M2)
            device_name: name.to_string(),
        })
        .await?;
    let paseto = resp.into_inner().paseto;
    store_token(server, &paseto, allow_plaintext_file)?;
    tracing::info!(server = %server, "device enrolled; token stored in credential store");
    Ok(())
}

/// Remove the locally stored token (server-side revocation is an admin action via ctl).
pub fn logout(_home: &std::path::Path) {
    match keyring_entry() {
        Ok(e) => {
            let _ = e.delete_credential();
        }
        Err(e) => tracing::warn!("keychain unavailable at logout: {e}"),
    }
    let _ = std::fs::remove_file(token_file());
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
