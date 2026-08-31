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
}

const K_SERVER: &str = "auth/server";
const K_TOKEN: &str = "auth/token";
const K_DEVICE: &str = "auth/device_id";
const K_TENANT: &str = "auth/tenant_id";

/// Persist identity (called by `cairn login`; keychain remains the canonical token store).
pub fn save_identity(store: &Store, id: &Identity) -> Result<(), CairnError> {
    store.meta_set(K_SERVER, &id.server_url)?;
    store.meta_set(K_TOKEN, &id.token)?;
    store.meta_set(K_DEVICE, &id.device_id)?;
    store.meta_set(K_TENANT, &id.tenant_id)
}

/// Load identity from the store meta.
#[must_use]
pub fn load_identity(store: &Store) -> Option<Identity> {
    Some(Identity {
        server_url: store.meta_get(K_SERVER)?,
        token: store.meta_get(K_TOKEN)?,
        device_id: store.meta_get(K_DEVICE)?,
        tenant_id: store.meta_get(K_TENANT)?,
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
    /// file still matches the journaled size (a real edit changes the size → syncs).
    hydrated_recently: Mutex<HashMap<String, Instant>>,
    /// Set by the watcher when an event names a path with NO local row (brand-new file);
    /// the loop then runs a rescan (idempotent) so new local files sync too.
    pub rescan_requested: AtomicBool,
    pub files_synced: AtomicU64,
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
    /// materialized AND its on-disk size still equals the journaled size. An editor save
    /// (append/rewrite) changes the size → NOT an echo → must sync.
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
        meta.len() == row.size
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

    // ensure the project exists on the server (idempotent)
    ensure_project(&server_url, &identity, &pid).await?;

    let existing = RUNTIMES.read().await.get(&pid).cloned();
    if let Some(rt) = existing {
        // already attached: just refresh the workspace binding + identity
        rt.stop();
        spawn_loop(Arc::clone(&rt), store, identity, server_url);
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
        abort: tokio::sync::watch::channel(false).0,
    });
    RUNTIMES.write().await.insert(pid.clone(), Arc::clone(&rt));
    spawn_loop(Arc::clone(&rt), store, identity, server_url);
    Ok(pid)
}

/// Detach: stop the loop, clear the binding (files on disk are untouched).
pub async fn detach(home: &Path, project_id: &str) -> Result<(), CairnError> {
    let store = Store::open(home, Arc::new(WallClock))?;
    if let Some(rt) = RUNTIMES.write().await.remove(project_id) {
        rt.stop();
    }
    cairn_sync::workspace::clear_workspace(&store, project_id)
}

/// Registry of live runtimes for the daemon process (ctl status/list surface).
pub static RUNTIMES: std::sync::LazyLock<RwLock<HashMap<String, Arc<ProjectRuntime>>>> =
    std::sync::LazyLock::new(|| RwLock::new(HashMap::new()));

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
) -> Result<(), CairnError> {
    let channel = tonic::transport::Endpoint::from_shared(server_url.to_string())
        .map_err(|e| CairnError::new(ErrorKind::Io, format!("bad server addr: {e}")))?
        .connect_timeout(Duration::from_secs(5))
        .connect()
        .await
        .map_err(|e| {
            CairnError::new(
                ErrorKind::Unavailable,
                format!("cannot reach server {server_url}: {e}"),
            )
        })?;
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

fn spawn_loop(rt: Arc<ProjectRuntime>, store: Store, identity: Identity, server_url: String) {
    let mut shutdown = rt.abort.subscribe();
    tokio::spawn(async move {
        // The loop is the re-entry point for ALL failures (I2): server not yet up,
        // partitions, restarts — retry with fixed 5s backoff until the shutdown signal.
        loop {
            match run_loop(&rt, &store, &identity, &server_url, &mut shutdown).await {
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

async fn run_loop(
    rt: &Arc<ProjectRuntime>,
    store: &Store,
    identity: &Identity,
    server_url: &str,
    shutdown: &mut tokio::sync::watch::Receiver<bool>,
) -> Result<(), CairnError> {
    let pid = rt.project_id.clone();
    let plane: Arc<dyn Plane> =
        Arc::new(GrpcPlane::connect(server_url, &identity.token, &identity.tenant_id).await?);
    let conn = store.conn_handle();
    let cas = Cas::open(&store.root().join("blobs"), conn.clone())?;
    let outbox = Outbox::new(conn.clone());
    let headers = HeaderCache::new(conn.clone());
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
                if store2.get_file(&rt2.project_id, &rel).is_none() {
                    // brand-new local file: no row exists to mark dirty — request a rescan
                    rt2.rescan_requested.store(true, Ordering::Relaxed);
                    continue;
                }
                let _ = store2.set_file_state(&rt2.project_id, &rel, "dirty");
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
    loop {
        tokio::select! {
            _ = shutdown.changed() => return Ok(()),
            _ = tick.tick() => {}
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
            }
            (Err(e), _) | (_, Err(e)) => {
                let mut v = rt.view.write().await;
                v.state = "error".into();
                v.last_error = Some(e.message.clone());
                tracing::warn!(project = %pid, "pass error: {e}");
                // keep looping: full-jitter retry semantics live in the engine; the loop
                // is the re-entry point (I2)
            }
        }
    }
}
