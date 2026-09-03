//! Attached-project runtime (WO1 AttachRoot walking skeleton): one task per attached root
//! running scan → push → pull → hydrate on a 1s cadence, with a watcher marking local
//! edits dirty after their 2s quiescence window. All durable state lives in the client
//! store (WAL) so kill -9 at any point resumes with zero duplicate journal entries (I2).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use cairn_core::clock::WallClock;
use cairn_core::pathutil::nfc_normalize;
use cairn_core::{CairnError, ErrorKind};
use cairn_proto::pb::project_client::ProjectClient;
use cairn_proto::pb::{CreateProjectRequest, GetProjectRequest};
use cairn_store::state::LocalState;
use cairn_store::{Cas, HeaderCache, Outbox, Store};
use cairn_sync::plane::Plane;
use cairn_sync::plane_grpc::GrpcPlane;
use cairn_sync::workspace::{set_workspace, workspace_dir};
use cairn_sync::{hydrate, scan, Engine, Gate};
use tokio::sync::{Mutex, RwLock};
use tonic::metadata::MetadataValue;
use tonic::Request;

/// Device identity, persisted in the home store's meta table at login.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub server_url: String,
    pub token: String,
    pub device_id: String,
    pub tenant_id: String,
    /// Optional PEM (self-signed dev CA) for `https://` endpoints.
    pub tls_ca: Option<String>,
}

const K_SERVER: &str = "auth/server";
const K_TOKEN: &str = "auth/token";
const K_DEVICE: &str = "auth/device_id";
const K_TENANT: &str = "auth/tenant_id";
const K_TLS_CA: &str = "auth/tls_ca";

/// Persist identity (called by `cairn login`; keychain remains the canonical token store).
pub fn save_identity(store: &Store, id: &Identity) -> Result<(), CairnError> {
    store.meta_set(K_SERVER, &id.server_url)?;
    store.meta_set(K_TOKEN, &id.token)?;
    store.meta_set(K_DEVICE, &id.device_id)?;
    store.meta_set(K_TENANT, &id.tenant_id)?;
    store.meta_set(K_TLS_CA, id.tls_ca.as_deref().unwrap_or(""))
}

/// Load identity from the store meta.
#[must_use]
pub fn load_identity(store: &Store) -> Option<Identity> {
    Some(Identity {
        server_url: store.meta_get(K_SERVER)?,
        token: store.meta_get(K_TOKEN)?,
        device_id: store.meta_get(K_DEVICE)?,
        tenant_id: store.meta_get(K_TENANT)?,
        tls_ca: store.meta_get(K_TLS_CA).filter(|s| !s.is_empty()),
    })
}

/// Forget identity (logout).
pub fn clear_identity(store: &Store) {
    for k in [K_SERVER, K_TOKEN, K_DEVICE, K_TENANT] {
        let _ = store.meta_set(k, "");
    }
}

/// Live view of one attached project (ctl status surface).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ProjView {
    pub state: String, // syncing | synced | error
    pub last_error: Option<String>,
    pub files_synced: u64,
    pub cursor: u64,
    pub pending_outbox: u64,
}

pub struct ProjectRuntime {
    pub project_id: String,
    pub workspace: PathBuf,
    pub view: RwLock<ProjView>,
    /// Paths hydrated recently — watcher echoes are suppressed ONLY while the on-disk
    /// file matches BOTH the journaled size AND journaled mtime (punch #5): a hydration
    /// echo has both; a size-preserving edit (byte flip, LUT swap, metadata rewrite)
    /// still touches mtime → NOT an echo → must sync. Silent divergence is the worst
    /// failure class a sync product has; the belt-and-braces [`ProjectRuntime::
    /// reconcile_sweep`] catches whatever the watcher misses entirely.
    hydrated_recently: Mutex<HashMap<String, Instant>>,
    /// Set by the watcher when an event names a path with NO local row (brand-new file);
    /// the loop then runs a rescan (idempotent) so new local files sync too.
    pub rescan_requested: AtomicBool,
    pub files_synced: AtomicU64,
    /// Monotonic sweep counter — rotates the bounded rehash sample so successive
    /// sweeps cover different files (full coverage over time, bounded cost per sweep).
    sweep_counter: AtomicU64,
    abort: tokio::sync::watch::Sender<bool>,
}

impl ProjectRuntime {
    pub async fn note_hydrated(&self, rel: &str) {
        self.hydrated_recently
            .lock()
            .await
            .insert(rel.to_string(), Instant::now());
    }

    /// True when this event is an echo of our own hydration: the path was recently
    /// materialized AND its on-disk state still equals the journaled size AND mtime.
    /// (punch #5) An echo has both; ANY real edit — including size-preserving ones
    /// (in-place byte flip, LUT swap, metadata rewrite) — touches mtime, so it is NOT
    /// suppressed and gets synced. Size-only checks silently swallow size-preserving
    /// edits: the worst failure class a sync product has.
    pub async fn should_suppress(&self, store: &Store, workspace: &Path, rel: &str) -> bool {
        {
            let map = self.hydrated_recently.lock().await;
            if !map.contains_key(rel) {
                return false;
            }
        }
        let Some(row) = store.get_file(&self.project_id, rel) else {
            return false;
        };
        let Ok(meta) = std::fs::metadata(workspace.join(rel)) else {
            return false;
        };
        let mtime = cairn_sync::scan::mtime_millis(&meta);
        meta.len() == row.size && mtime == row.mtime
    }

    pub fn stop(&self) {
        let _ = self.abort.send(true);
    }
}

/// Attach a root: validate, bind workspace, ensure the project exists server-side, spawn
/// the per-project sync task. Durable registration (meta) happens BEFORE the ack.
#[allow(clippy::too_many_arguments)]
pub async fn attach(
    home: &Path,
    root_path: &Path,
    project_id: Option<String>,
    server_override: Option<String>,
) -> Result<String, CairnError> {
    let store = Store::open(home, Arc::new(WallClock))?;
    let identity = load_identity(&store).ok_or_else(|| {
        CairnError::new(
            ErrorKind::Unauthenticated,
            "no device identity: run `cairn login` first",
        )
    })?;
    let server_url = server_override.map_or(identity.server_url.clone(), |s| {
        if s.starts_with("http://") || s.starts_with("https://") {
            s
        } else {
            format!("http://{s}")
        }
    });
    if !root_path.is_dir() {
        return Err(CairnError::new(
            ErrorKind::Io,
            format!("attach root {} is not a directory", root_path.display()),
        ));
    }
    let pid = project_id.unwrap_or_else(|| {
        let name = root_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "project".into());
        cairn_sync::workspace::project_id_from_name(&name)
    });

    // durable binding first (I2: the ack reflects committed state)
    set_workspace(&store, &pid, root_path)?;
    save_identity(
        &store,
        &Identity {
            server_url: server_url.clone(),
            ..identity.clone()
        },
    )?;

    let ca_pem = identity.tls_ca.as_ref().map(|c| c.as_bytes().to_vec());

    // ensure the project exists on the server (idempotent)
    ensure_project(&server_url, &identity, &pid, ca_pem.as_deref()).await?;

    let ca_pem = identity.tls_ca.as_ref().map(|c| c.as_bytes().to_vec());

    let existing = RUNTIMES.read().await.get(&pid).cloned();
    if let Some(rt) = existing {
        // already attached: just refresh the workspace binding + identity
        rt.stop();
        let root2 = root_path.to_path_buf();
        let pid2 = pid.clone();
        let id2 = identity.clone();
        let url2 = server_url.clone();
        let store2 = store.clone();
        let ca2 = ca_pem.clone();
        spawn_loop(Arc::clone(&rt), store, identity, server_url, ca_pem);
        connect_cfapi(&store2, &root2, &pid2, &id2, &url2, ca2.as_deref()).await;
        return Ok(pid);
    }

    let rt = Arc::new(ProjectRuntime {
        project_id: pid.clone(),
        workspace: workspace_dir(&store, &pid),
        view: RwLock::new(ProjView {
            state: "syncing".into(),
            ..ProjView::default()
        }),
        hydrated_recently: Mutex::new(HashMap::new()),
        rescan_requested: AtomicBool::new(false),
        files_synced: AtomicU64::new(0),
        sweep_counter: AtomicU64::new(0),
        abort: tokio::sync::watch::channel(false).0,
    });
    RUNTIMES.write().await.insert(pid.clone(), Arc::clone(&rt));
    let root2 = root_path.to_path_buf();
    let pid2 = pid.clone();
    let id2 = identity.clone();
    let url2 = server_url.clone();
    let store2 = store.clone();
    let ca2 = ca_pem.clone();
    spawn_loop(Arc::clone(&rt), store, identity, server_url, ca_pem);
    connect_cfapi(&store2, &root2, &pid2, &id2, &url2, ca2.as_deref()).await;
    Ok(pid)
}

/// Detach: stop the loop, clear the binding (files on disk are untouched).
pub async fn detach(home: &Path, project_id: &str) -> Result<(), CairnError> {
    let store = Store::open(home, Arc::new(WallClock))?;
    if let Some(rt) = RUNTIMES.write().await.remove(project_id) {
        rt.stop();
    }
    #[cfg(windows)]
    {
        CFAPI_CONNS.lock().expect("cfapi conns").remove(project_id); // drop disconnects the sync root
    }
    cairn_sync::workspace::clear_workspace(&store, project_id)
}

/// Registry of live runtimes for the daemon process (ctl status/list surface).
pub static RUNTIMES: std::sync::LazyLock<RwLock<HashMap<String, Arc<ProjectRuntime>>>> =
    std::sync::LazyLock::new(|| RwLock::new(HashMap::new()));

/// Live CfAPI write-back connections (Windows only): the connection must outlive
/// the attach — dropping it disconnects the root from the filter driver.
#[cfg(windows)]
pub static CFAPI_CONNS: std::sync::LazyLock<
    std::sync::Mutex<HashMap<String, cairn_fs_win::cfapi::Connection>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

/// Windows attach glue: register + bulk placeholders + write-back callbacks
/// (WO6-1/WO6-2; no-op on other platforms).
///
/// ASYNC since round 13: this runs INSIDE the daemon's ctl handler (an async
/// context) — the round-12 version used Handle::block_on here, which panics
/// with "Cannot start a runtime from within a runtime" on the FIRST real
/// windows attach (caught live by the W0 row of the windows-runner matrix;
/// Linux never compiles this path, and the cfapi_roundtrip test drives the
/// connection machinery OUTSIDE a runtime, so nothing else ever saw it).
/// The plane connect is simply awaited; the CfAPI callback threads keep using
/// the passed `rt` handle (block_on from NON-runtime threads is the
/// documented, legal pattern).
#[cfg(windows)]
async fn connect_cfapi(
    store: &Store,
    root: &Path,
    pid: &str,
    identity: &Identity,
    server_url: &str,
    ca_pem: Option<&[u8]>,
) {
    let plane: Arc<dyn cairn_sync::plane::Plane> = match cairn_sync::plane_grpc::GrpcPlane::connect(
        server_url,
        &identity.token,
        &identity.tenant_id,
        ca_pem,
    )
    .await
    {
        Ok(p) => Arc::new(p),
        Err(e) => {
            tracing::warn!(project = %pid, "CfAPI: plane connect failed ({e}); read-only until restart");
            return;
        }
    };
    let rt = tokio::runtime::Handle::current();
    match crate::win_attach::attach_windows(
        store,
        root,
        pid,
        &identity.tenant_id,
        &identity.device_id,
        plane,
        rt,
    ) {
        Ok(conn) => {
            CFAPI_CONNS
                .lock()
                .expect("cfapi conns")
                .insert(pid.to_string(), conn);
            tracing::info!(project = %pid, "CfAPI write-back connected (root registered)");
        }
        Err(e) => {
            // non-fatal: sync engine still works over plain files; the filter
            // surface (placeholders/badges/write-back) waits for reconnect
            tracing::warn!(project = %pid, "CfAPI attach failed: {e}");
        }
    }
}

#[cfg(not(windows))]
#[allow(clippy::unused_async)]
async fn connect_cfapi(
    _store: &Store,
    _root: &Path,
    _pid: &str,
    _identity: &Identity,
    _server_url: &str,
    _ca_pem: Option<&[u8]>,
) {
}

/// Re-attach all bound workspaces at daemon boot (crash-resume path).
pub async fn resume_all(home: &Path) -> usize {
    let Ok(store) = Store::open(home, Arc::new(WallClock)) else {
        return 0;
    };
    let identity_present = load_identity(&store).is_some_and(|i| !i.token.is_empty());
    if !identity_present {
        return 0;
    }
    let mut count = 0;
    let conn = store.conn_handle();
    let prefixes: Vec<String> = {
        let conn = conn.lock().expect("store poisoned");
        let mut stmt = match conn.prepare("SELECT key FROM meta WHERE key LIKE 'workspace:%'") {
            Ok(s) => s,
            Err(_) => return 0,
        };
        stmt.query_map([], |r| r.get::<_, String>(0))
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default()
    };
    for k in prefixes {
        let pid = k.trim_start_matches("workspace:").to_string();
        let Some(root) = store.meta_get(&k).filter(|p| !p.is_empty()) else {
            continue;
        };
        let path = PathBuf::from(&root);
        if !path.is_dir() {
            tracing::warn!(project = %pid, root = %root, "bound workspace missing; skipping");
            continue;
        }
        let _ = set_workspace(&store, &pid, &path);
        match attach(home, &path, Some(pid.clone()), None).await {
            Ok(_) => count += 1,
            Err(e) => tracing::error!(project = %pid, "resume attach failed: {e}"),
        }
    }
    count
}

async fn ensure_project(
    server_url: &str,
    identity: &Identity,
    project_id: &str,
    ca_pem: Option<&[u8]>,
) -> Result<(), CairnError> {
    let channel = cairn_sync::plane_grpc::connect_channel(server_url, ca_pem).await?;
    let mut c = ProjectClient::new(channel);
    let bearer = MetadataValue::try_from(format!("Bearer {}", identity.token))
        .map_err(|e| CairnError::new(ErrorKind::Internal, format!("auth header: {e}")))?;
    let mut get = Request::new(GetProjectRequest {
        tenant_id: identity.tenant_id.clone(),
        project_id: project_id.into(),
    });
    get.metadata_mut().insert("authorization", bearer.clone());
    match c.get_project(get).await {
        Ok(_) => Ok(()),
        Err(status) if status.code() == tonic::Code::NotFound => {
            let mut create = Request::new(CreateProjectRequest {
                tenant_id: identity.tenant_id.clone(),
                project_id: project_id.into(),
                name: project_id.into(),
            });
            create.metadata_mut().insert("authorization", bearer);
            c.create_project(create).await.map_err(|s| {
                CairnError::new(
                    ErrorKind::Internal,
                    format!("create_project: {}", s.message()),
                )
            })?;
            Ok(())
        }
        Err(status) => Err(CairnError::new(
            ErrorKind::Internal,
            format!("get_project: {}", status.message()),
        )),
    }
}

fn spawn_loop(
    rt: Arc<ProjectRuntime>,
    store: Store,
    identity: Identity,
    server_url: String,
    ca_pem: Option<Vec<u8>>,
) {
    let mut shutdown = rt.abort.subscribe();
    tokio::spawn(async move {
        // The loop is the re-entry point for ALL failures (I2): server not yet up,
        // partitions, restarts — retry with fixed 5s backoff until the shutdown signal.
        loop {
            match run_loop(
                &rt,
                &store,
                &identity,
                &server_url,
                ca_pem.as_deref(),
                &mut shutdown,
            )
            .await
            {
                Ok(()) => return, // detach/shutdown
                Err(e) => {
                    let mut v = rt.view.write().await;
                    v.state = "error".into();
                    v.last_error = Some(e.message.clone());
                    drop(v);
                    tracing::warn!(project = %rt.project_id, "sync loop stopped ({e}); retrying in 5s");
                    tokio::select! {
                        _ = shutdown.changed() => return,
                        () = tokio::time::sleep(Duration::from_secs(5)) => {}
                    }
                }
            }
        }
    });
}

/// ADR-0014 Phase 3 keepalive: (1) RENEW every lease this daemon process still holds
/// for `project_id` (heartbeat, no token bump — only takeover bumps fencing tokens);
/// (2) REAP rows whose owning process died on this machine — drop locally and
/// best-effort server-release so peers see the pen free immediately. A renew that
/// fails with STALE_LEASE means we were legitimately fenced: drop the local row (the
/// next save re-acquires or surfaces a conflict — never a silent overwrite).
async fn lease_keepalive(store: &Store, plane: &dyn Plane, tenant_id: &str, project_id: &str) {
    let me = i64::from(std::process::id());
    for row in store.list_leases_pid() {
        match row.pid {
            Some(pid) if pid == me => {
                if row.project_id.as_deref() != Some(project_id) {
                    continue; // another project's runtime renews its own rows
                }
                let device = row.device_id.clone().unwrap_or_default();
                match plane
                    .renew_lease(
                        tenant_id,
                        project_id,
                        &row.path,
                        &device,
                        row.token,
                        cairn_sync::LEASE_TTL_MS,
                    )
                    .await
                {
                    Ok(expires_at) => {
                        let _ = store.put_lease_pid(
                            &row.path,
                            row.token,
                            expires_at,
                            Some(me),
                            Some(project_id),
                            Some(&device),
                        );
                    }
                    Err(e) => {
                        // fenced or server-expired: stop claiming the pen locally
                        let _ = store.drop_lease(&row.path);
                        tracing::debug!(path = %row.path, "lease not renewed ({e}); local row dropped");
                    }
                }
            }
            Some(pid) if pid > 0 && !cairn_store::db::process_alive(pid) => {
                // dead process on this machine — free its pen (machine-global truth)
                let _ = store.drop_lease(&row.path);
                if let (Some(lproj), Some(ldev)) = (row.project_id.clone(), row.device_id.clone()) {
                    let _ = plane
                        .release_lease(tenant_id, &lproj, &row.path, &ldev, row.token)
                        .await;
                }
                tracing::info!(path = %row.path, pid, "reaped lease of dead process");
            }
            _ => {} // legacy rows (no pid): expire via TTL, as before
        }
    }
}

async fn run_loop(
    rt: &Arc<ProjectRuntime>,
    store: &Store,
    identity: &Identity,
    server_url: &str,
    ca_pem: Option<&[u8]>,
    shutdown: &mut tokio::sync::watch::Receiver<bool>,
) -> Result<(), CairnError> {
    let pid = rt.project_id.clone();
    let plane: Arc<dyn Plane> = Arc::new(
        GrpcPlane::connect(server_url, &identity.token, &identity.tenant_id, ca_pem).await?,
    );
    let conn = store.conn_handle();
    let cas = Cas::open(&store.root().join("blobs"), conn.clone())?;
    let outbox = Outbox::new(conn.clone());
    // WO6-5 reader-pool fix (burst CI evidence 2026-09-02): dedicated query-only
    // readers so 32-concurrent-open bursts don't serialize behind store writes
    let headers = HeaderCache::with_read_pool(conn.clone(), &store.root().join("db.sqlite"), 4);
    let engine = Engine {
        tenant_id: identity.tenant_id.clone(),
        project_id: pid.clone(),
        device_id: identity.device_id.clone(),
        store: store.clone(),
        cas,
        outbox: Outbox::new(conn.clone()),
        headers,
        plane: Arc::clone(&plane),
        dicts: cairn_core::compress::DictRegistry::new(),
        gate: Gate::default(),
    };

    // local-edit watcher: settled paths → dirty (suppress hydration echoes)
    let (wtx, wrx) = std::sync::mpsc::channel::<cairn_sync::watch::QuiescedEvent>();
    let _watcher = cairn_sync::watch::watch(&rt.workspace, wtx)?;
    let (ttx, mut trx) = tokio::sync::mpsc::unbounded_channel::<String>();
    std::thread::spawn(move || {
        while let Ok(ev) = wrx.recv() {
            let cairn_sync::watch::QuiescedEvent::Settled(abs) = ev;
            if ttx.send(abs).is_err() {
                return;
            }
        }
    });
    {
        let rt2 = Arc::clone(rt);
        let store2 = store.clone();
        let ws = rt.workspace.clone();
        tokio::spawn(async move {
            while let Some(abs) = trx.recv().await {
                let p = Path::new(&abs);
                let Ok(rel) = p.strip_prefix(&ws) else {
                    continue;
                };
                if rel.as_os_str().is_empty() {
                    continue;
                }
                let rel = nfc_normalize(&rel.to_string_lossy().replace('\\', "/"));
                if rt2.should_suppress(&store2, &ws, &rel).await {
                    continue;
                }
                match store2.get_file(&rt2.project_id, &rel) {
                    None => {
                        // brand-new local file: no row exists to mark dirty — request a rescan
                        rt2.rescan_requested.store(true, Ordering::Relaxed);
                    }
                    Some(row) if row.mode == "file" => {
                        let _ = store2.set_file_state(&rt2.project_id, &rel, "dirty");
                    }
                    Some(_) => {
                        // metadata row (dir/symlink): windows fires a parent-dir
                        // event the moment children appear; dirtying the DIR row
                        // wedged every push pass on fs::read(directory) EACCES
                        // (round 13, the W1 windows catch). The scan walk re-puts
                        // metadata rows; just request the rescan it implies.
                        rt2.rescan_requested.store(true, Ordering::Relaxed);
                    }
                }
            }
        });
    }

    // initial scan: marks everything new/changed dirty (idempotent, resumable)
    let stats = scan::scan_project(store, &pid)?;
    tracing::info!(
        project = %pid,
        files = stats.files_seen,
        dirty = stats.new_dirty + stats.redirtied,
        "initial scan complete"
    );

    let mut tick = tokio::time::interval(Duration::from_millis(1000));
    // periodic reconciliation sweep (punch #5, belt-and-braces): full stat walk +
    // bounded rotating rehash sample. Interval/budgets are env-tunable so the
    // acceptance harness can exercise the sweep quickly.
    let sweep_secs: u64 = std::env::var("CAIRN_SWEEP_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(300);
    let sweep_files: usize = std::env::var("CAIRN_SWEEP_SAMPLE_FILES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8);
    let sweep_bytes: u64 = std::env::var("CAIRN_SWEEP_SAMPLE_BYTES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(256 * 1024 * 1024);
    let mut last_sweep = tokio::time::Instant::now();
    // WO6-2: LRU eviction sweep — keeps the disk at/above the free-space target so a
    // 2–4 TB/seat library cannot fill local NVMe. 0 disables; the tiering_enabled kill
    // switch also disables it live (no restart).
    let evict_secs: u64 = std::env::var("CAIRN_EVICT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(300);
    let evict_target_pct: u64 = std::env::var("CAIRN_EVICT_TARGET_FREE_PCT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(60);
    let evict_min_age: i64 = std::env::var("CAIRN_EVICT_MIN_AGE_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3600);
    let mut last_evict = tokio::time::Instant::now();
    // ADR-0014 Phase 3: lease heartbeat — renew THIS daemon's ephemeral pens on the
    // 5s cadence (3 beats per 15s TTL), reap rows whose owning process died, and
    // server-release the reaped tokens. This is what turns a crashed editor's lock
    // into a seconds-long blip instead of a human-gated unlock.
    let mut last_lease_beat = tokio::time::Instant::now();

    // Explorer badge layer (P1 #2): derive the root status from sync-loop
    // facts and drive the CfAPI provider-status + error report when it
    // CHANGES (no-op passes skip the FFI round-trip entirely).
    #[cfg(windows)]
    let mut badge = cairn_fs_win::badge::BadgeMachine::new();
    #[cfg(windows)]
    let badge_root_utf16: Vec<u16> = {
        let mut v: Vec<u16> = rt.workspace.to_string_lossy().encode_utf16().collect();
        v.push(0);
        v
    };
    #[cfg(windows)]
    let mut badge_pass_ok = true;
    #[cfg(windows)]
    let mut badge_syncing = false;
    #[cfg(windows)]
    let mut badge_error: Option<cairn_fs_win::badge::RootError> = None;

    loop {
        tokio::select! {
            _ = shutdown.changed() => return Ok(()),
            _ = tick.tick() => {}
        }
        if evict_secs > 0 && last_evict.elapsed().as_secs() >= evict_secs {
            last_evict = tokio::time::Instant::now();
            let tiering_on = store
                .meta_get("flag:tiering_enabled")
                .map(|v| v != "false")
                .unwrap_or(true);
            if tiering_on {
                match cairn_store::eviction::evict_sweep(store, evict_target_pct, evict_min_age) {
                    Ok(r) if r.needed => {
                        tracing::info!(
                            project = %pid,
                            evicted = r.evicted_chunks,
                            freed_mb = r.freed_bytes / (1024 * 1024),
                            free_pct_before = r.free_before.checked_mul(100).map_or(0, |v| v / r.total),
                            "eviction sweep: LRU chunks reclaimed (pins + fresh chunks protected)"
                        );
                    }
                    Ok(_) => {} // disk above target — nothing to do
                    Err(e) => tracing::warn!(project = %pid, "eviction sweep failed: {e}"),
                }
            }
        }
        if last_sweep.elapsed().as_secs() >= sweep_secs {
            last_sweep = tokio::time::Instant::now();
            let counter = rt.sweep_counter.fetch_add(1, Ordering::Relaxed);
            match cairn_sync::scan::reconcile_sweep(
                store,
                &pid,
                &rt.workspace,
                counter,
                sweep_files,
                sweep_bytes,
            ) {
                Ok(s) if s.rehash_dirty > 0 => {
                    // silent divergence FOUND and redirtied — this log line matters:
                    // it means the belt-and-braces layer caught what the watcher missed
                    tracing::warn!(
                        project = %pid,
                        rehash_dirty = s.rehash_dirty,
                        rehashed = s.rehashed,
                        stat_redirtied = s.stat_redirtied,
                        "reconcile sweep found diverged file(s); re-pushing"
                    );
                }
                Ok(s) => {
                    tracing::debug!(
                        project = %pid,
                        rehashed = s.rehashed,
                        bytes = s.bytes_rehashed,
                        skipped_transform = s.skipped_transform,
                        stat_redirtied = s.stat_redirtied,
                        "reconcile sweep clean"
                    );
                }
                Err(e) => tracing::warn!(project = %pid, "reconcile sweep failed: {e}"),
            }
        }
        if last_lease_beat.elapsed().as_millis() >= u128::from(cairn_sync::LEASE_HEARTBEAT_MS) {
            last_lease_beat = tokio::time::Instant::now();
            lease_keepalive(store, plane.as_ref(), &identity.tenant_id, &pid).await;
        }
        {
            let mut v = rt.view.write().await;
            v.state = "syncing".into();
        }
        tracing::debug!(project = %pid, "pass tick");
        // brand-new local files (watcher saw an unknown path): rescan is idempotent and
        // only inserts rows for genuinely new/changed content
        if rt.rescan_requested.swap(false, Ordering::Relaxed) {
            match scan::scan_project(store, &pid) {
                Ok(s) => tracing::debug!(project = %pid, new = s.new_dirty, "rescan complete"),
                Err(e) => tracing::warn!(project = %pid, "rescan failed: {e}"),
            }
        }
        let pass = engine.sync_pass().await;
        let hydr = hydrate::materialize_missing(
            engine.plane.as_ref(),
            store,
            &engine.cas,
            &engine.headers,
            &identity.tenant_id,
            &pid,
        )
        .await;
        match (pass, hydr) {
            (Ok(p), Ok(h)) => {
                if h.materialized > 0 {
                    tracing::info!(project = %pid, hydrated = h.materialized, bytes = h.bytes,
                        "pulled remote files");
                    for path in &h.paths {
                        rt.note_hydrated(path).await;
                    }
                }
                // count synced FILE rows (dirs are infrastructure, not content)
                let rows = store.list_files(&pid);
                let mut synced = 0u64;
                for r in &rows {
                    if r.mode == "file"
                        && matches!(
                            LocalState::parse(&r.local_state),
                            Some(LocalState::Synced)
                                | Some(LocalState::Clean)
                                | Some(LocalState::Pinned)
                        )
                    {
                        synced += 1;
                    }
                }
                rt.files_synced.store(synced, Ordering::Relaxed);
                let dirty_left = rows
                    .iter()
                    .filter(|r| {
                        matches!(LocalState::parse(&r.local_state), Some(LocalState::Dirty))
                    })
                    .count();
                let mut v = rt.view.write().await;
                v.files_synced = synced;
                v.cursor = store.get_cursor(&identity.device_id, &pid);
                v.pending_outbox = outbox.pending_count(&pid);
                v.state = if dirty_left > 0 || v.pending_outbox > 0 {
                    "syncing".into()
                } else {
                    "synced".into()
                };
                v.last_error = None;
                let _ = p;
                #[cfg(windows)]
                {
                    badge_pass_ok = true;
                    badge_syncing = v.state == "syncing";
                    badge_error = None;
                }
            }
            (Err(e), _) | (_, Err(e)) => {
                let mut v = rt.view.write().await;
                v.state = "error".into();
                v.last_error = Some(e.message.clone());
                tracing::warn!(project = %pid, "pass error: {e}");
                #[cfg(windows)]
                {
                    badge_pass_ok = false;
                    badge_error = Some(cairn_fs_win::badge::RootError {
                        // E_UNEXPECTED-class code with the engine message as
                        // the Explorer-visible description (ASCII-truncated
                        // at 200 bytes to fit the shell's buffer)
                        code: 0x8000_FFFF,
                        description: {
                            let m = e.message.as_str();
                            let cut: String = m.chars().take(200).collect();
                            cut
                        },
                    });
                }
                // keep looping: full-jitter retry semantics live in the engine; the loop
                // is the re-entry point (I2)
            }
        }

        // badge apply (Windows): ride the CFAPI_CONNS connection the attach
        // registered for THIS project; only changes hit the FFI.
        #[cfg(windows)]
        {
            let facts = cairn_fs_win::badge::EngineFacts {
                server_reachable: badge_pass_ok,
                // pending_count is u64 (store-wide accounting); the badge
                // fact is usize — try_from is lossless on every real box
                // (round 13: shipped as a raw u64 in round 12, a
                // windows-only compile error Linux CI can never see)
                outbox_pending: usize::try_from(outbox.pending_count(&pid)).unwrap_or(usize::MAX),
                transfers_in_flight: usize::from(badge_syncing),
                last_error: badge_error.clone(),
            };
            if let Some(directive) = badge.next(&facts, cairn_fs_win::badge::Bulk::No) {
                let conns = CFAPI_CONNS.lock().expect("cfapi conns");
                if let Some(conn) = conns.get(&pid) {
                    let bc = cairn_fs_win::badge::ffi::BadgeConnection {
                        connection: conn,
                        root_utf16: badge_root_utf16.clone(),
                    };
                    match cairn_fs_win::badge::ffi::apply(&bc, &directive) {
                        Ok(()) => tracing::debug!(
                            project = %pid,
                            status = %directive.status,
                            "explorer badge updated"
                        ),
                        Err(code) => tracing::warn!(
                            project = %pid,
                            code = code,
                            "explorer badge update failed"
                        ),
                    }
                }
            }
        }
    }
}
