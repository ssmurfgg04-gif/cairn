//! Local daemon (SPEC §11): single process, localhost gRPC ctl on :17777 (token-gated for
//! mutations), local dashboard on :17778 (ADR-0009). Built as the frozen ctl contract's first
//! reference client.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use cairn_core::clock::WallClock;
use cairn_proto::pb::ctl_diagnostics_server::{CtlDiagnostics, CtlDiagnosticsServer};
use cairn_proto::pb::ctl_presence_server::{CtlPresence, CtlPresenceServer};
use cairn_proto::pb::ctl_projects_server::{CtlProjects, CtlProjectsServer};
use cairn_proto::pb::ctl_status_server::{CtlStatus, CtlStatusServer};
use cairn_proto::pb::{
    Ack, AttachRootRequest, AttachRootResponse, DetachRootRequest, DoctorCheck, DoctorReport,
    DoctorRequest, FlagInfo, GetFlagsRequest, GetFlagsResponse, ListProjectsCtlRequest,
    ListProjectsCtlResponse, PresenceEventMsg, ProjectInfoCtl, ProjectStatus, SendPresenceRequest,
    SetFlagRequest, StatusRequest, StatusResponse, WatchPresenceRequest,
};
use tokio::sync::RwLock;
use tokio_stream::StreamExt as _;
use tonic::{Request, Response, Status};

use crate::audit::AuditFile;
use crate::doctor;
use crate::projects;

use cairn_core::rbac::{self, Permission, Role};

/// Daemon-wide swarm options (ADR-0017): the signal server to rendezvous
/// through + the join code the host shared (swarm admission, §7).
#[derive(Clone)]
pub struct SwarmOpts {
    /// Signal server `host:port`.
    pub signal: String,
    /// The host-shared join code. `None` = the well-known dev-key path
    /// (smoke tests only; pairs with `cairn signal --dev-key`).
    ///
    /// Not `Debug`-printed: a join code is a credential. (JoinCode's own
    /// Debug renders it — acceptable for a host-side shareable — but opts
    /// structs end up in generic debug logs, so we stay manual.)
    pub join_code: Option<cairn_p2p::JoinCode>,
}

/// Shared daemon state.
pub struct DaemonState {
    /// Data directory.
    pub home: PathBuf,
    /// Start time.
    pub started: Instant,
    /// Kill switches (config_flags mirror; daemon-side copy, server-side authoritative).
    pub flags: RwLock<Vec<(String, String)>>,
    /// WO6-3: live recall jobs (ctl RecallStatus surface); shared with background tasks.
    pub recall_jobs: std::sync::Arc<RwLock<HashMap<String, crate::daemon::RecallJob>>>,
}

impl DaemonState {
    fn new(home: PathBuf) -> Self {
        DaemonState {
            home,
            started: Instant::now(),
            flags: RwLock::new(default_flags()),
            recall_jobs: Arc::new(RwLock::new(HashMap::new())),
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
        // ADR-0023: live presence — OFF until the editor of THIS device opts
        // in. Presence is per-device by construction (the flag lives in this
        // machine's home store): flipping it never exposes anyone else.
        // Applies at swarm join (attach/daemon start).
        ("live_presence".into(), "false".into()),
        // ADR-0023: zero-touch semantic timeline merge — OFF by default; every
        // editor decides for themselves whether frame-disjoint re-cuts
        // auto-merge (C11) or escalate (C3).
        ("semantic_merge".into(), "false".into()),
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
                    last_error: v.last_error.clone().unwrap_or_default(),
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

// ---------- live presence (ADR-0023 §2) ----------

/// The daemon's live-presence service: the ctl-boundary front door for
/// playhead/drag/selection telemetry. OFF by default — both RPCs fail with
/// `failed_precondition` naming the flag until the editor opts in.
pub struct CtlPresenceSvc {
    pub state: Arc<DaemonState>,
}

impl CtlPresenceSvc {
    /// Is live presence on for this device? (In-memory mirror of the flag —
    /// the same copy `set_flag` mutates and mirrors into the home store.)
    async fn presence_on(&self) -> bool {
        self.state
            .flags
            .read()
            .await
            .iter()
            .any(|(k, v)| k == "live_presence" && v == "true")
    }
}

#[tonic::async_trait]
impl CtlPresence for CtlPresenceSvc {
    async fn send_presence(
        &self,
        request: Request<SendPresenceRequest>,
    ) -> Result<Response<Ack>, Status> {
        if !self.presence_on().await {
            return Err(Status::failed_precondition(
                "live presence is OFF on this device — flip flag 'live_presence' (applies at \
                 next swarm join/daemon start)",
            ));
        }
        let req = request.into_inner();
        if req.payload.len() > 1200 {
            return Err(Status::invalid_argument(format!(
                "presence payload {} bytes exceeds the 1200-byte telemetry bound",
                req.payload.len()
            )));
        }
        let project = req.project;
        // RBAC: ViewPresence for the acting device against that project's
        // members (ledgered, fail-closed on corrupt members)
        rbac_guard(
            &self.state,
            &project,
            None,
            Permission::ViewPresence,
            "ctl/send-presence",
        )
        .await?;
        // 1) local echo: subscribers see this device's own events without a
        //    second round-trip
        let from = {
            let store = cairn_store::Store::open(&self.state.home, Arc::new(WallClock))
                .map_err(|e| Status::failed_precondition(e.message))?;
            acting_device(&store)
        };
        let _ = projects::PRESENCE_TX.send(projects::LocalPresence {
            from,
            project: project.clone(),
            payload: req.payload.clone(),
            seen_at_ms: now_ms_i64(),
            local: true,
        });
        // 2) relay into the project's swarm (encrypted sessions; no-op when
        //    the project has no swarm or the join predates the flag flip)
        let reached = {
            let map = projects::RUNTIMES.read().await;
            map.get(&project)
                .and_then(|rt| rt.swarm.blocking_lock().clone())
                .map(|sw| sw.broadcast_presence(&req.payload))
                .unwrap_or(0)
        };
        tracing::trace!(project = %project, peers = reached, "presence sent");
        Ok(Response::new(Ack { ok: true }))
    }

    type WatchPresenceStream =
        std::pin::Pin<Box<dyn futures::Stream<Item = Result<PresenceEventMsg, Status>> + Send>>;

    async fn watch_presence(
        &self,
        request: Request<WatchPresenceRequest>,
    ) -> Result<Response<Self::WatchPresenceStream>, Status> {
        if !self.presence_on().await {
            return Err(Status::failed_precondition(
                "live presence is OFF on this device — flip flag 'live_presence' (applies at \
                 next swarm join/daemon start)",
            ));
        }
        rbac_guard(
            &self.state,
            "",
            None,
            Permission::ViewPresence,
            "ctl/watch-presence",
        )
        .await?;
        let filter = request.into_inner().project;
        let rx = projects::PRESENCE_TX.subscribe();
        // BroadcastStream: Lagged (slow consumer) yields Err items we skip —
        // presence is a signal, not a log; the next event resyncs.
        let stream =
            tokio_stream::wrappers::BroadcastStream::new(rx).filter_map(move |res| match res {
                Ok(ev) if filter.is_empty() || ev.project == filter => Some(Ok(PresenceEventMsg {
                    from: ev.from,
                    project: ev.project,
                    payload: ev.payload,
                    seen_at_ms: ev.seen_at_ms,
                    local: ev.local,
                })),
                Ok(_) => None,
                Err(_) => None, // Lagged — skip missed events
            });
        Ok(Response::new(Box::pin(stream)))
    }
}

// ---------- daemon-side RBAC (the Ctl boundary, ADR-0020 §4) ----------

fn now_ms_i64() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// The acting device: the daemon's own enrolled identity. The machine is
/// the actor — the CLI shares this home, and studio members.json keys
/// roles by device id, so `role_of(our device)` is exactly "what this
/// machine's person may do".
fn acting_device(store: &cairn_store::Store) -> String {
    crate::projects::load_identity(store)
        .map(|i| i.device_id)
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| "local".into())
}

/// Resolve a project's primary root for enforcement: the first durable
/// root binding, else any live runtime's workspace.
async fn project_root(state: &DaemonState, project_id: &str) -> Option<PathBuf> {
    if let Ok(store) = cairn_store::Store::open(&state.home, Arc::new(WallClock)) {
        if let Some(b) = cairn_sync::workspace::list_roots(&store, project_id)
            .into_iter()
            .next()
        {
            return Some(b.path);
        }
    }
    let map = projects::RUNTIMES.read().await;
    map.values()
        .find(|rt| rt.project_id == project_id)
        .map(|rt| rt.workspace.clone())
}

/// Enforce `perm` for the acting device against the project's synced
/// members.json — the daemon side of the ADR-0020 matrix. Every decision
/// (allow AND deny) lands in the project's audit ledger, which syncs to
/// every peer: the log is not fiction.
///
/// No resolvable root (first attach, unbound project, daemon with no
/// projects) enforces nothing — there is no members.json to read; the
/// CLI-side guards remain the second layer for root-present commands.
/// A CORRUPT members.json fails closed (parse error → precondition),
/// never open.
pub(crate) async fn rbac_guard(
    state: &DaemonState,
    project_id: &str,
    root_hint: Option<&std::path::Path>,
    perm: Permission,
    action: &str,
) -> Result<Role, Status> {
    let root = match root_hint {
        Some(p) => Some(p.to_path_buf()),
        None => project_root(state, project_id).await,
    };
    let Some(root) = root else {
        tracing::trace!(action, "rbac: no project root to enforce against");
        return Ok(Role::Editor);
    };
    let store = cairn_store::Store::open(&state.home, Arc::new(WallClock))
        .map_err(|e| Status::failed_precondition(e.message))?;
    let device = acting_device(&store);
    // fail CLOSED on corrupt members (parse error propagates)
    let members = crate::members::load(&root)
        .map_err(|e| Status::failed_precondition(format!("members.json unreadable: {e}")))?;
    let role = members.role_of(&device);
    let allowed = rbac::allows(role, perm);
    if let Err(e) = AuditFile::decision(
        &root,
        now_ms_i64(),
        &device,
        role.as_str(),
        action,
        project_id,
        allowed,
    ) {
        // bookkeeping is never allowed to break enforcement, but it must
        // be loud when it breaks
        tracing::warn!(error = %e, "audit ledger write failed (decision still enforced)");
    }
    if !allowed {
        return Err(Status::permission_denied(format!(
            "device '{device}' (role '{}') may not {action} — ask the owner to adjust \
             .cairn/members.json",
            role.as_str()
        )));
    }
    Ok(role)
}

#[tonic::async_trait]
impl CtlProjects for CtlProjectsSvc {
    async fn attach_root(
        &self,
        request: Request<AttachRootRequest>,
    ) -> Result<Response<AttachRootResponse>, Status> {
        let req = request.into_inner();
        let root = std::path::PathBuf::from(&req.root_path);
        // RBAC: binding a machine to a project is a member decision —
        // the members file in the root being attached is the authority
        rbac_guard(
            &self.state,
            &req.project_id,
            Some(&root),
            Permission::AttachRoot,
            "ctl/attach-root",
        )
        .await?;
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
        // RBAC: an assistant-class device must not be able to unbind the
        // studio machine from the project (the "cairn detach --project a"
        // story) — enforced HERE, not in the CLI
        rbac_guard(
            &self.state,
            &req.project_id,
            None,
            Permission::DetachRoot,
            "ctl/detach-root",
        )
        .await?;
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
        // RBAC: kill switches are global on this machine — enforce
        // ManageFlags against every attached project (any denial blocks;
        // no projects attached = nothing to enforce against)
        let mut pids: Vec<String> = {
            let map = projects::RUNTIMES.read().await;
            map.values().map(|rt| rt.project_id.clone()).collect()
        };
        pids.sort();
        pids.dedup();
        for pid in &pids {
            rbac_guard(
                &self.state,
                pid,
                None,
                Permission::ManageFlags,
                "ctl/set-flag",
            )
            .await?;
        }
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
///
/// `swarm` (ADR-0017): when set, every attached project joins the signal server's
/// swarm and hydration goes peer-first (LAN blocks before cloud egress).
pub async fn run(
    home: PathBuf,
    ctl_addr: String,
    ui_addr: String,
    review_addr: Option<String>,
    swarm: Option<SwarmOpts>,
) -> anyhow::Result<()> {
    let state = Arc::new(DaemonState::new(home));
    // persist the ctl endpoint so CLI status/attach in THIS home find the right daemon
    // (multi-daemon machines run several ctl ports; 17777 is only the default)
    if let Ok(store) = cairn_store::Store::open(&state.home, Arc::new(WallClock)) {
        let _ = store.meta_set("ctl/addr", &format!("http://{ctl_addr}"));
        // swarm opts are daemon-wide and durable: loop restarts rejoin without
        // re-passing flags (same meta pattern as ctl/addr). The join code is
        // stored in its normalized display form so `JoinCode::parse` accepts
        // it on the rejoin path. It is a credential and this home is
        // user-private (device tokens live here too) — never logged.
        match &swarm {
            Some(SwarmOpts { signal, join_code }) => {
                let _ = store.meta_set("swarm/signal", signal);
                match join_code {
                    Some(code) => {
                        let _ = store.meta_set("swarm/join-code", &code.display());
                        // clear the legacy raw-key slot so it can never shadow
                        // the code on the read path
                        let _ = store.meta_set("swarm/key", "");
                    }
                    None => {
                        let _ = store.meta_set("swarm/join-code", "");
                        let _ = store.meta_set("swarm/key", "cairn-dev-swarm-key");
                    }
                }
                tracing::info!(
                    signal = %signal,
                    "swarm transport enabled (join-code gated, peer-first hydration)"
                );
            }
            None => {
                let _ = store.meta_set("swarm/signal", "");
                let _ = store.meta_set("swarm/join-code", "");
                let _ = store.meta_set("swarm/key", "");
            }
        }
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
    // WO6-3: the FULL ctl contract is now served — snapshots, pins, recall
    let snapshots_svc = CtlSnapshotsServer::new(CtlSnapshotsSvc {
        state: Arc::clone(&state),
    });
    let pins_svc = CtlPinsServer::new(CtlPinsSvc {
        state: Arc::clone(&state),
    });
    let recall_svc = CtlRecallServer::new(CtlRecallSvc {
        state: Arc::clone(&state),
    });
    // ADR-0023: live presence (first ctl-side streaming RPC)
    let presence_svc = CtlPresenceServer::new(CtlPresenceSvc {
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
        .add_service(snapshots_svc)
        .add_service(pins_svc)
        .add_service(recall_svc)
        .add_service(presence_svc)
        .serve(ctl_addr.parse()?);

    let ui = crate::dashboard::serve(ui_addr, Arc::clone(&state));

    // client review portal (ADR-0020): token-gated routes only, OFF by
    // default. Runs detached: a portal failure must not take the sync
    // daemon down with it (clients can retry; the engine keeps syncing).
    if let Some(addr) = review_addr {
        let portal = cairn_review::http::Portal::new(Arc::new(crate::review::RuntimesProvider));
        tokio::spawn(async move {
            match cairn_review::http::serve(addr, portal).await {
                Ok(()) => tracing::info!("review portal closed"),
                Err(e) => tracing::error!(error = %e, "review portal failed"),
            }
        });
    }

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

// ---------- WO6-3: ctl contract completeness (snapshots / pins / recall) ----------

use cairn_proto::pb::{
    ctl_pins_server::{CtlPins, CtlPinsServer},
    ctl_recall_server::{CtlRecall, CtlRecallServer},
    ctl_snapshots_server::{CtlSnapshots, CtlSnapshotsServer},
    download_client::DownloadClient,
    snapshot_client::SnapshotClient,
    CreateSnapshotRequest, CreateSnapshotResponse, FoldNowRequest, GetManifestRequest,
    ListPinsRequest, ListPinsResponse, ListSnapshotsRequest, ListSnapshotsResponse, PinInfo,
    PinRequest, RecallStatusRequest, RecallStatusResponse, RestoreSnapshotRequest,
    RestoreSnapshotResponse, SnapshotInfo, StartRecallRequest, StartRecallResponse, UnpinRequest,
};

/// Shared server context: identity + an authed channel to the sync server.
/// Built per-call (cheap: tonic channels multiplex); identity comes from the store.
async fn server_ctx(
    home: &std::path::Path,
) -> Result<
    (
        cairn_proto::pb::snapshot_client::SnapshotClient<tonic::transport::Channel>,
        String,
    ),
    Status,
> {
    let store = cairn_store::Store::open(home, Arc::new(WallClock))
        .map_err(|e| Status::failed_precondition(e.message))?;
    let server_url = crate::projects::load_identity(&store)
        .map(|i| i.server_url)
        .ok_or_else(|| Status::failed_precondition("no device identity: run `cairn login`"))?;
    let channel = cairn_sync::plane_grpc::connect_channel(&server_url, None)
        .await
        .map_err(|e| Status::unavailable(format!("server {server_url}: {e}")))?;
    Ok((SnapshotClient::new(channel), server_url))
}

fn bearer_md(
    token: String,
) -> Result<tonic::metadata::MetadataValue<tonic::metadata::Ascii>, Status> {
    token
        .parse()
        .map_err(|_| Status::internal("bad token chars"))
}

fn bearer(home: &std::path::Path) -> Result<(String, String), Status> {
    let store = cairn_store::Store::open(home, Arc::new(WallClock))
        .map_err(|e| Status::failed_precondition(e.message))?;
    let id = crate::projects::load_identity(&store)
        .ok_or_else(|| Status::failed_precondition("no device identity: run `cairn login`"))?;
    Ok((id.tenant_id, id.token))
}

pub struct CtlSnapshotsSvc {
    pub state: Arc<DaemonState>,
}

#[tonic::async_trait]
impl CtlSnapshots for CtlSnapshotsSvc {
    async fn create_snapshot(
        &self,
        request: Request<CreateSnapshotRequest>,
    ) -> Result<Response<CreateSnapshotResponse>, Status> {
        let req = request.into_inner();
        rbac_guard(
            &self.state,
            &req.project_id,
            None,
            Permission::Snapshot,
            "ctl/create-snapshot",
        )
        .await?;
        let (mut client, _url) = server_ctx(&self.state.home).await?;
        let (tenant, token) = bearer(&self.state.home)?;
        let mut r = Request::new(FoldNowRequest {
            tenant_id: tenant,
            project_id: req.project_id.clone(),
        });
        r.metadata_mut()
            .insert("authorization", bearer_md(token.clone())?);
        let out = client
            .fold_now(r)
            .await
            .map_err(|s| Status::internal(s.message()))?;
        let inner = out.into_inner();
        tracing::info!(project = %req.project_id, commit = %inner.commit_hash, "snapshot created (fold)");
        Ok(Response::new(CreateSnapshotResponse {
            commit_hash: inner.commit_hash,
        }))
    }

    async fn list_snapshots(
        &self,
        request: Request<ListSnapshotsRequest>,
    ) -> Result<Response<ListSnapshotsResponse>, Status> {
        let req = request.into_inner();
        let (mut client, _url) = server_ctx(&self.state.home).await?;
        let (tenant, token) = bearer(&self.state.home)?;
        // head ref → walk the commit chain (parents) up to a bounded depth
        let mut r = Request::new(cairn_proto::pb::ListRefsRequest {
            tenant_id: tenant.clone(),
            project_id: req.project_id.clone(),
        });
        r.metadata_mut()
            .insert("authorization", bearer_md(token.clone())?);
        let refs = client
            .list_refs(r)
            .await
            .map_err(|s| Status::internal(s.message()))?
            .into_inner();
        let Some(main) = refs.refs.iter().find(|r| r.ref_name == "main") else {
            return Ok(Response::new(ListSnapshotsResponse { snapshots: vec![] }));
        };
        let mut snapshots = Vec::new();
        let mut cursor = main.commit_hash.clone();
        for _ in 0..512 {
            let mut gr = Request::new(GetManifestRequest {
                tenant_id: tenant.clone(),
                manifest_hash: cursor.clone(),
            });
            gr.metadata_mut()
                .insert("authorization", bearer_md(token.clone())?);
            let bytes = download_commit(&self.state.home, gr).await?;
            let (tree, parent, author, label, seq) = match cairn_core::commit::parse_commit(&bytes)
            {
                Ok(v) => v,
                Err(_) => break, // not a commit object — chain ends
            };
            let _ = cairn_core::hash::Hash::from_hex(&cursor)
                .ok_or_else(|| Status::internal("bad commit hash"))?;
            let _ = tree;
            snapshots.push(SnapshotInfo {
                commit_hash: cursor.clone(),
                parent: parent.map(|p| p.hex()).unwrap_or_default(),
                label,
                author,
                snapshot_seq: seq,
                server_ts: 0, // commit objects carry seq, not wall time (frozen format)
            });
            match parent {
                Some(p) => cursor = p.hex(),
                None => break,
            }
        }
        Ok(Response::new(ListSnapshotsResponse { snapshots }))
    }

    async fn restore_snapshot(
        &self,
        request: Request<RestoreSnapshotRequest>,
    ) -> Result<Response<RestoreSnapshotResponse>, Status> {
        let req = request.into_inner();
        // RBAC: restore overwrites the workspace from a commit — the
        // most destructive ctl mutation, guarded hardest (Owner-only in
        // the matrix)
        rbac_guard(
            &self.state,
            &req.project_id,
            None,
            Permission::Restore,
            "ctl/restore-snapshot",
        )
        .await?;
        let (_client, _url) = server_ctx(&self.state.home).await?;
        let (tenant, token) = bearer(&self.state.home)?;
        // commit → tree → (path, manifest) entries
        let mut cr = Request::new(GetManifestRequest {
            tenant_id: tenant.clone(),
            manifest_hash: req.commit_hash.clone(),
        });
        cr.metadata_mut()
            .insert("authorization", bearer_md(token.clone())?);
        let commit_bytes = download_commit(&self.state.home, cr).await?;
        let (tree, _parent, _author, _label, _seq) =
            cairn_core::commit::parse_commit(&commit_bytes)
                .map_err(|e| Status::internal(e.message))?;
        let mut tr = Request::new(GetManifestRequest {
            tenant_id: tenant.clone(),
            manifest_hash: tree.hex(),
        });
        tr.metadata_mut()
            .insert("authorization", bearer_md(token.clone())?);
        let tree_bytes = download_commit(&self.state.home, tr).await?;
        let entries =
            cairn_core::commit::parse_tree(&tree_bytes).map_err(|e| Status::internal(e.message))?;

        // materialize each file via the SAME machinery hydration uses (CAS→plane,
        // hash-verified), writing into the workspace (or target_path)
        let store = cairn_store::Store::open(&self.state.home, Arc::new(WallClock))
            .map_err(|e| Status::failed_precondition(e.message))?;
        let conn = store.conn_handle();
        let cas = cairn_store::Cas::open(&store.root().join("blobs"), conn.clone())
            .map_err(|e| Status::internal(e.message))?;
        let id = crate::projects::load_identity(&store)
            .ok_or_else(|| Status::failed_precondition("no device identity"))?;
        let channel = cairn_sync::plane_grpc::connect_channel(&id.server_url, None)
            .await
            .map_err(|e| Status::unavailable(e.message))?;
        let plane: Arc<dyn cairn_sync::plane::Plane> =
            Arc::new(cairn_sync::plane_grpc::GrpcPlane::from_channel(
                channel,
                id.token.clone(),
                tenant.clone(),
            ));
        let root = cairn_sync::workspace::workspace_dir(&store, &req.project_id);
        let base = if req.target_path.is_empty() {
            root.clone()
        } else {
            std::path::PathBuf::from(&req.target_path)
        };
        std::fs::create_dir_all(&base).map_err(|e| Status::internal(e.to_string()))?;
        let mut restored_files = 0u64;
        let mut bytes_total = 0u64;
        let mut manifest_cache: HashMap<String, cairn_core::manifest::Manifest> = HashMap::new();
        for (path, manifest_hex) in &entries {
            // WO6-9: tree entries were written from pushed journal ops; never let a
            // crafted commit materialize bytes OUTSIDE the target directory.
            cairn_core::pathutil::validate_rel_path(path)
                .map_err(|e| Status::invalid_argument(e.message))?;
            let target = base.join(path);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).map_err(|e| Status::internal(e.to_string()))?;
            }
            // STREAMING restore (review round): chunks stream chunk-by-chunk into a temp
            // file and atomically rename over the target — a 50GB restore never holds
            // the file in RAM, and a mid-stream verify failure leaves the old file intact.
            let tmp = target.with_extension("cairn-restore-tmp");
            let mut f = std::fs::File::create(&tmp)
                .map_err(|e| Status::internal(format!("restore {path}: {e}")))?;
            let written = match cairn_sync::hydrate::hydrate_one_into(
                plane.as_ref(),
                None, // snapshot restore: plane only (no swarm plumbing here yet)
                &cas,
                &tenant,
                manifest_hex,
                path,
                &mut manifest_cache,
                &mut f,
            )
            .await
            {
                Ok(w) => w,
                Err(e) => {
                    // mid-stream failure: leave the OLD file intact, remove the partial
                    // temp, and surface the error (I2: never half-materialize)
                    drop(f);
                    let _ = std::fs::remove_file(&tmp);
                    return Err(Status::internal(format!("restore {path}: {e}")));
                }
            };
            f.sync_all()
                .map_err(|e| Status::internal(format!("restore {path}: {e}")))?;
            drop(f);
            std::fs::rename(&tmp, &target)
                .map_err(|e| Status::internal(format!("restore {path}: {e}")))?;
            restored_files += 1;
            bytes_total += written;
        }
        // restored files become dirty rows so the engine pushes the restore state
        for (path, manifest_hex) in &entries {
            if let Some(mut row) = store.get_file(&req.project_id, path) {
                row.manifest_hash = Some(manifest_hex.clone());
                let _ = store.put_file(&row);
                let _ = store.set_file_state(&req.project_id, path, "dirty");
            }
        }
        tracing::info!(project = %req.project_id, files = restored_files, "snapshot restored");
        Ok(Response::new(RestoreSnapshotResponse {
            restored_files,
            bytes: bytes_total,
        }))
    }
}

/// Fetch a stored object (commit/tree) through the Download service with auth.
async fn download_commit(
    home: &std::path::Path,
    mut request: Request<GetManifestRequest>,
) -> Result<Vec<u8>, Status> {
    let (_tenant, token) = bearer(home)?;
    let store = cairn_store::Store::open(home, Arc::new(WallClock))
        .map_err(|e| Status::failed_precondition(e.message))?;
    let id = crate::projects::load_identity(&store)
        .ok_or_else(|| Status::failed_precondition("no device identity"))?;
    let channel = cairn_sync::plane_grpc::connect_channel(&id.server_url, None)
        .await
        .map_err(|e| Status::unavailable(e.message))?;
    let mut c = DownloadClient::new(channel);
    request
        .metadata_mut()
        .insert("authorization", bearer_md(token)?);
    let out = c
        .get_manifest(request)
        .await
        .map_err(|s| Status::internal(s.message()))?;
    Ok(out.into_inner().body)
}

pub struct CtlPinsSvc {
    pub state: Arc<DaemonState>,
}

#[tonic::async_trait]
impl CtlPins for CtlPinsSvc {
    async fn pin(
        &self,
        request: Request<PinRequest>,
    ) -> Result<Response<cairn_proto::pb::Ack>, Status> {
        let req = request.into_inner();
        rbac_guard(
            &self.state,
            &req.project_id,
            None,
            Permission::OrganizeBins,
            "ctl/pin",
        )
        .await?;
        let store = cairn_store::Store::open(&self.state.home, Arc::new(WallClock))
            .map_err(|e| Status::failed_precondition(e.message))?;
        // pin = ensure chunks local (recall-one) + record file-level pin
        recall_paths(&self.state, &store, &req.project_id, Some(&req.path)).await?;
        store
            .pin_file(&req.project_id, &req.path)
            .map_err(|e| Status::not_found(e.message))?;
        tracing::info!(project = %req.project_id, path = %req.path, "pinned (recalled + eviction-exempt)");
        Ok(Response::new(cairn_proto::pb::Ack { ok: true }))
    }

    async fn unpin(
        &self,
        request: Request<UnpinRequest>,
    ) -> Result<Response<cairn_proto::pb::Ack>, Status> {
        let req = request.into_inner();
        rbac_guard(
            &self.state,
            &req.project_id,
            None,
            Permission::OrganizeBins,
            "ctl/unpin",
        )
        .await?;
        let store = cairn_store::Store::open(&self.state.home, Arc::new(WallClock))
            .map_err(|e| Status::failed_precondition(e.message))?;
        store
            .unpin_file(&req.project_id, &req.path)
            .map_err(|e| Status::internal(e.message))?;
        tracing::info!(project = %req.project_id, path = %req.path, "unpinned (evictable again)");
        Ok(Response::new(cairn_proto::pb::Ack { ok: true }))
    }

    async fn list_pins(
        &self,
        request: Request<ListPinsRequest>,
    ) -> Result<Response<ListPinsResponse>, Status> {
        let req = request.into_inner();
        let store = cairn_store::Store::open(&self.state.home, Arc::new(WallClock))
            .map_err(|e| Status::failed_precondition(e.message))?;
        let pins = store
            .list_pins(&req.project_id)
            .into_iter()
            .map(|(path, size)| PinInfo {
                path,
                size,
                state: "pinned".into(),
            })
            .collect();
        Ok(Response::new(ListPinsResponse { pins }))
    }
}

/// Recall engine (pin's fetch step + background jobs): materialize missing files
/// (whole project or one path) — ctl recall = cold-tier fetch with progress (WO6-3).
async fn recall_paths(
    _state: &DaemonState,
    store: &cairn_store::Store,
    project_id: &str,
    only: Option<&str>,
) -> Result<(), Status> {
    recall_paths_simple(store, project_id, only).await
}

#[derive(Default)]
pub struct RecallJob {
    pub state: String,
    pub progress: f64,
    pub bytes_done: u64,
    pub bytes_total: u64,
}

pub struct CtlRecallSvc {
    pub state: Arc<DaemonState>,
}

#[tonic::async_trait]
impl CtlRecall for CtlRecallSvc {
    async fn start_recall(
        &self,
        request: Request<StartRecallRequest>,
    ) -> Result<Response<StartRecallResponse>, Status> {
        let req = request.into_inner();
        rbac_guard(
            &self.state,
            &req.project_id,
            None,
            Permission::Read,
            "ctl/start-recall",
        )
        .await?;
        let job_id = uuid::Uuid::now_v7().to_string();
        let response_id = job_id.clone();
        let home = self.state.home.clone();
        let project = req.project_id.clone();
        let only = if req.path.is_empty() {
            None
        } else {
            Some(req.path.clone())
        };
        self.state.recall_jobs.write().await.insert(
            job_id.clone(),
            RecallJob {
                state: "running".into(),
                ..RecallJob::default()
            },
        );
        let registry = Arc::clone(&self.state.recall_jobs);
        tokio::spawn(async move {
            let Ok(store) = cairn_store::Store::open(&home, Arc::new(WallClock)) else {
                registry.write().await.insert(
                    job_id.clone(),
                    RecallJob {
                        state: "failed".into(),
                        ..RecallJob::default()
                    },
                );
                return;
            };
            let result = recall_paths_simple(&store, &project, only.as_deref()).await;
            let done = RecallJob {
                state: if result.is_ok() {
                    "completed".into()
                } else {
                    "failed".into()
                },
                progress: 1.0,
                ..RecallJob::default()
            };
            registry.write().await.insert(job_id, done);
        });
        Ok(Response::new(StartRecallResponse {
            job_id: response_id,
        }))
    }

    async fn recall_status(
        &self,
        request: Request<RecallStatusRequest>,
    ) -> Result<Response<RecallStatusResponse>, Status> {
        let req = request.into_inner();
        let jobs = self.state.recall_jobs.read().await;
        let Some(job) = jobs.get(&req.job_id) else {
            return Err(Status::not_found(format!(
                "unknown recall job {}",
                req.job_id
            )));
        };
        // ETA: no live bytes-per-sec meter in v1 (progress is per-file state);
        // eta_ms stays 0 = unknown, which the ctl contract permits.
        Ok(Response::new(RecallStatusResponse {
            state: job.state.clone(),
            progress: job.progress,
            bytes_done: job.bytes_done,
            bytes_total: job.bytes_total,
            eta_ms: 0,
        }))
    }
}

/// Fire-and-forget recall used by the background job (progress tracked at
/// job granularity: running → completed/failed; file-level metering rides
/// the dashboard hydration metrics).
async fn recall_paths_simple(
    store: &cairn_store::Store,
    project_id: &str,
    only: Option<&str>,
) -> Result<(), Status> {
    let id = crate::projects::load_identity(store)
        .ok_or_else(|| Status::failed_precondition("no device identity"))?;
    let channel = cairn_sync::plane_grpc::connect_channel(&id.server_url, None)
        .await
        .map_err(|e| Status::unavailable(e.message))?;
    let plane: Arc<dyn cairn_sync::plane::Plane> =
        Arc::new(cairn_sync::plane_grpc::GrpcPlane::from_channel(
            channel,
            id.token.clone(),
            id.tenant_id.clone(),
        ));
    let conn = store.conn_handle();
    let cas = cairn_store::Cas::open(&store.root().join("blobs"), conn.clone())
        .map_err(|e| Status::internal(e.message))?;
    let rows: Vec<_> = store
        .list_files(project_id)
        .into_iter()
        .filter(|r| r.mode == "file" && r.manifest_hash.is_some())
        .filter(|r| only.is_none_or(|p| r.path == p))
        .collect();
    let mut manifest_cache: HashMap<String, cairn_core::manifest::Manifest> = HashMap::new();
    for row in rows {
        if let Some(hex) = row.manifest_hash {
            if hex.is_empty() {
                continue;
            }
            // recall = warm the local CAS; bytes are discarded — stream to a sink so
            // RAM stays bounded by one chunk regardless of file size (review round)
            let _ = cairn_sync::hydrate::hydrate_one_into(
                plane.as_ref(),
                None, // recall warms from the plane; swarm blocks land as a side effect
                &cas,
                &id.tenant_id,
                &hex,
                &row.path,
                &mut manifest_cache,
                &mut std::io::sink(),
            )
            .await
            .map_err(|e| Status::internal(format!("recall {}: {e}", row.path)))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cairn_core::rbac::{MemberFile, Role};

    fn tmp_home() -> PathBuf {
        tempfile::tempdir().unwrap().keep()
    }

    fn enrolled_home(device: &str) -> PathBuf {
        let home = tmp_home();
        let store = cairn_store::Store::open(&home, Arc::new(WallClock)).unwrap();
        crate::projects::save_identity(
            &store,
            &crate::projects::Identity {
                server_url: "http://127.0.0.1:9".into(),
                token: "t".into(),
                device_id: device.into(),
                tenant_id: "tn".into(),
                tls_ca: None,
            },
        )
        .unwrap();
        home
    }

    fn members_root(device: &str, role: Role) -> PathBuf {
        let root = tempfile::tempdir().unwrap().keep();
        std::fs::create_dir_all(root.join(".cairn")).unwrap();
        let mut f = MemberFile::default();
        f.upsert(device, "the member", role, "dev-owner", 1);
        std::fs::write(crate::members::members_path(&root), f.to_json().unwrap()).unwrap();
        root
    }

    /// The daemon-side story from the audit round: an assistant-class
    /// device must not be able to unbind the machine from a project —
    /// enforced at the ctl boundary, not in the CLI.
    #[tokio::test]
    async fn assistant_cannot_detach_but_editor_can() {
        let home = enrolled_home("dev-artist");
        let state = Arc::new(DaemonState::new(home.clone()));
        let root = members_root("dev-artist", Role::Assistant);
        // root_hint simulates a resolvable project root
        let r = rbac_guard(
            &state,
            "proj",
            Some(&root),
            Permission::DetachRoot,
            "ctl/detach-root",
        )
        .await;
        assert!(r.is_err());
        let msg = r.err().unwrap().message().to_string();
        assert!(msg.contains("assistant"), "denial names the role: {msg}");
        // the denial is AUDITED (the log is not fiction)
        let audit = crate::audit::AuditFile::load(&root).unwrap();
        assert!(audit
            .iter()
            .any(|(_, e)| !e.allowed && e.action == "ctl/detach-root"));

        // an Editor-class device detaches fine, and that lands in the audit too
        let root2 = members_root("dev-artist", Role::Editor);
        let r2 = rbac_guard(
            &state,
            "proj",
            Some(&root2),
            Permission::DetachRoot,
            "ctl/detach-root",
        )
        .await;
        assert!(r2.is_ok());
        let audit2 = crate::audit::AuditFile::load(&root2).unwrap();
        assert!(audit2
            .iter()
            .any(|(_, e)| e.allowed && e.action == "ctl/detach-root"));
    }

    #[tokio::test]
    async fn reviewer_cannot_attach_but_every_creative_role_can() {
        let home = enrolled_home("dev-x");
        let state = Arc::new(DaemonState::new(home));
        for role in [
            Role::Owner,
            Role::LeadEditor,
            Role::Editor,
            Role::Assistant,
            Role::Colorist,
            Role::SoundDesigner,
        ] {
            let root = members_root("dev-x", role);
            assert!(
                rbac_guard(
                    &state,
                    "p",
                    Some(&root),
                    Permission::AttachRoot,
                    "ctl/attach-root"
                )
                .await
                .is_ok(),
                "{role:?} must attach"
            );
        }
        let root = members_root("dev-x", Role::Reviewer);
        assert!(rbac_guard(
            &state,
            "p",
            Some(&root),
            Permission::AttachRoot,
            "ctl/attach-root"
        )
        .await
        .is_err());
    }

    /// Unlisted device = the documented fail-open Editor default — and
    /// no-identity daemon ("local") behaves the same. Corrupt members
    /// fails CLOSED.
    #[tokio::test]
    async fn unlisted_defaults_to_editor_and_corrupt_fails_closed() {
        let home = enrolled_home("dev-unknown");
        let state = Arc::new(DaemonState::new(home));
        // no members.json at all -> Editor default, allowed + audited
        let root = tempfile::tempdir().unwrap().keep();
        let r = rbac_guard(
            &state,
            "p",
            Some(&root),
            Permission::DetachRoot,
            "ctl/detach-root",
        )
        .await;
        assert!(r.is_ok());
        assert_eq!(crate::audit::AuditFile::load(&root).unwrap().len(), 1);

        // corrupt members.json: fail closed (precondition), never open
        std::fs::create_dir_all(root.join(".cairn")).unwrap();
        std::fs::write(crate::members::members_path(&root), b"{ broken").unwrap();
        let r2 = rbac_guard(
            &state,
            "p",
            Some(&root),
            Permission::DetachRoot,
            "ctl/detach-root",
        )
        .await;
        assert!(r2.is_err());
        assert_eq!(r2.err().unwrap().code(), tonic::Code::FailedPrecondition);
    }

    /// No root resolvable -> nothing to enforce against (first attach on
    /// a machine with no bindings): allowed, no audit file invented.
    #[tokio::test]
    async fn no_root_means_nothing_to_enforce() {
        let home = enrolled_home("dev-a");
        let state = Arc::new(DaemonState::new(home));
        let r = rbac_guard(
            &state,
            "ghost",
            None,
            Permission::Restore,
            "ctl/restore-snapshot",
        )
        .await;
        assert!(r.is_ok());
    }
}
