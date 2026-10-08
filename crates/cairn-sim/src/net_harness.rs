//! Real-network harness shared by the cross-network E2E test binaries
//! (`two_homes_e2e.rs`, `editing_session_e2e.rs`): ONE real
//! `cairn_server::ServerState` served over REAL TCP (tonic gRPC + axum
//! objects HTTP, both ephemeral loopback ports), plus full engine stacks
//! ("homes") that talk to it through the REAL `GrpcPlane` — the production
//! plane, not the sim's `InProcPlane`.
//!
//! P2P is OFF BY CONSTRUCTION: a home's engine holds a `GrpcPlane` only —
//! no swarm handle exists in this harness (the daemon's `quic_relay` flag
//! governs the swarm leg, which does not exist here), so every byte of
//! media and every journal entry flows through the meeting point. That is
//! exactly the "all traffic via the hosted meeting point" topology the
//! deployment runbook ships.
//!
//! Each home binds its OWN loopback address (127.0.0.2 / 127.0.0.3 — Linux
//! treats the whole loopback /8 as local, no root needed) and holds the
//! listener open for its lifetime, the way the daemon holds its console
//! bind. Distinct addresses/ports + distinct enrolled identities are
//! asserted by the tests. If a host refuses non-.1 loopback binds (some
//! containers map only 127.0.0.1), the bind falls back to distinct
//! 127.0.0.1 ports and says so loudly.
//!
//! The kill -9 ANALOGUE available in-process: drop every engine handle
//! without flush/close and reopen from disk (WAL replay) — the same
//! semantics `World::crash_device` uses, minus the in-proc plane.

use std::path::PathBuf;
use std::sync::Arc;

use cairn_core::clock::WallClock;
use cairn_store::state::LocalState;
use cairn_store::{Cas, FileRow, HeaderCache, Outbox, Store};

/// The meeting point: server state + the gRPC URL homes connect to. The
/// objects-HTTP router is spawned from the same store (presigned URLs point
/// at the ACTUAL bound port, like `cold_fetch.rs`).
pub struct MeetingPoint {
    pub state: Arc<cairn_server::ServerState>,
    pub grpc_url: String,
    /// Held so the server's on-disk state outlives the test body.
    #[allow(dead_code)] // liveness is the point, not reads
    pub dir: tempfile::TempDir,
}

/// Boot the real server (gRPC + objects HTTP on ephemeral 127.0.0.1 ports)
/// and seed the tenant + project rows the homes sync under.
pub async fn boot_meeting_point(tenant: &str, project: &str) -> MeetingPoint {
    let dir = tempfile::tempdir().unwrap();

    // objects HTTP first: the store's presigned base must carry the real port
    let obj_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let obj_port = obj_listener.local_addr().unwrap().port();
    let base = format!("http://127.0.0.1:{obj_port}/");
    let db = cairn_server::db::open(&dir.path().join("meta.db"))
        .await
        .unwrap();
    cairn_server::db::migrate(&db).await.unwrap();
    let auth = cairn_server::auth::Authenticator::load_or_create(
        &dir.path().join("keys"),
        Arc::new(WallClock),
    )
    .unwrap();
    let store = Arc::new(
        cairn_server::storage::LocalFsStore::open(
            &dir.path().join("objects"),
            b"net-harness-key",
            &base,
        )
        .unwrap(),
    );
    let state = Arc::new(cairn_server::ServerState {
        db: db.clone(),
        auth,
        store: Arc::clone(&store) as Arc<dyn cairn_server::storage::ObjectStore>,
        bloom: tokio::sync::RwLock::new(cairn_core::bloom::Bloom::empty()),
        clock: Arc::new(WallClock) as Arc<dyn cairn_core::clock::SystemClock>,
        dev_insecure: true,
    });
    state.migrate().await.unwrap();

    let router = Arc::clone(&store).router();
    tokio::spawn(async move { axum::serve(obj_listener, router).await });

    // the full production gRPC stack over a REAL TCP listener
    let grpc_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let grpc_port = grpc_listener.local_addr().unwrap().port();
    let st_journal = Arc::clone(&state);
    let st_lease = Arc::clone(&state);
    let st_upload = Arc::clone(&state);
    let st_download = Arc::clone(&state);
    let st_auth = Arc::clone(&state);
    let st_project = Arc::clone(&state);
    let st_snapshot = Arc::clone(&state);
    tokio::spawn(async move {
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
            .add_service(cairn_proto::pb::snapshot_server::SnapshotServer::new(
                cairn_server::services::SnapshotSvc { state: st_snapshot },
            ))
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(
                grpc_listener,
            ))
            .await;
    });

    sqlx::query("INSERT OR IGNORE INTO tenants(id, created_at) VALUES(?1,0)")
        .bind(tenant)
        .execute(&db)
        .await
        .unwrap();
    sqlx::query(
        "INSERT OR IGNORE INTO projects(tenant_id, project_id, created_at) VALUES(?1,?2,0)",
    )
    .bind(tenant)
    .bind(project)
    .execute(&db)
    .await
    .unwrap();

    MeetingPoint {
        state,
        grpc_url: format!("http://127.0.0.1:{grpc_port}"),
        dir,
    }
}

/// Enroll one device identity with the meeting point (the real PASETO
/// flow: enroll code → enroll → (token, device_id)).
pub async fn enroll(mp: &MeetingPoint, tenant: &str, name: &str) -> (String, String) {
    let code = mp
        .state
        .auth
        .enroll_code(tenant, &format!("{name}@homes.tv"), "sync", 600_000)
        .await;
    let (token, identity) = mp
        .state
        .auth
        .enroll(&mp.state.db, &code, &format!("pk-{name}"), name)
        .await
        .expect("enroll");
    (token, identity.device_id)
}

/// One full engine stack ("home"): own home dir, own store, own blobs, own
/// project root, own enrolled identity, own gRPC channel against the ONE
/// meeting point, and a console listener bound to the home's OWN loopback
/// address (held open for the home's lifetime).
pub struct Home {
    pub tag: &'static str,
    pub root: tempfile::TempDir,
    pub console: tokio::net::TcpListener,
    pub console_addr: std::net::SocketAddr,
    pub device_id: String,
    pub tenant_id: String,
    pub project_id: String,
    pub engine: Option<cairn_sync::Engine>,
}

impl Drop for Home {
    fn drop(&mut self) {
        // the engine may hold the only connection handle; drop it plainly.
        self.engine = None;
    }
}

/// Bind a per-home console listener on its OWN loopback address
/// (127.0.0.2 / 127.0.0.3), falling back to a distinct 127.0.0.1 port with
/// a loud note when the host does not map the whole loopback /8.
fn bind_home_console(tag: &str, addr: &str) -> (tokio::net::TcpListener, std::net::SocketAddr) {
    match std::net::TcpListener::bind(addr) {
        Ok(std_l) => {
            std_l.set_nonblocking(true).unwrap();
            let l = tokio::net::TcpListener::from_std(std_l).unwrap();
            let a = l.local_addr().unwrap();
            println!("home {tag}: console bound on {a} (distinct loopback address)");
            (l, a)
        }
        Err(e) => {
            let std_l = std::net::TcpListener::bind("127.0.0.1:0").unwrap_or_else(|e2| {
                panic!("home {tag}: cannot bind {addr} ({e}) nor fallback ({e2})")
            });
            std_l.set_nonblocking(true).unwrap();
            let l = tokio::net::TcpListener::from_std(std_l).unwrap();
            let a = l.local_addr().unwrap();
            println!("home {tag}: 127.x/8 unavailable ({e}) — fallback distinct port {a}");
            (l, a)
        }
    }
}

/// Open a home: dirs, store, blobs, real `GrpcPlane` against the meeting
/// point, console listener on `loopback_addr`.
pub async fn open_home(
    tag: &'static str,
    loopback_addr: &str,
    mp: &MeetingPoint,
    token: &str,
    device_id: &str,
    tenant: &str,
    project: &str,
) -> Home {
    let root = tempfile::tempdir().unwrap();
    let (console, console_addr) = bind_home_console(tag, loopback_addr);
    let engine = open_engine(root.path(), &mp.grpc_url, token, device_id, tenant, project).await;
    Home {
        tag,
        root,
        console,
        console_addr,
        device_id: device_id.to_string(),
        tenant_id: tenant.to_string(),
        project_id: project.to_string(),
        engine: Some(engine),
    }
}

/// (Re)build the engine over the home's EXISTING on-disk store — the
/// restart half of the kill -9 analogue (no flush, no close; WAL replay).
pub async fn open_engine(
    root: &std::path::Path,
    server_url: &str,
    token: &str,
    device_id: &str,
    tenant: &str,
    project: &str,
) -> cairn_sync::Engine {
    let store = Store::open(&root.join("store"), Arc::new(WallClock)).unwrap();
    let conn = store.conn_handle();
    let cas = Cas::open(&root.join("blobs"), conn.clone()).unwrap();
    let plane = cairn_sync::plane_grpc::GrpcPlane::connect(server_url, token, tenant, None)
        .await
        .unwrap_or_else(|e| panic!("{device_id}: plane connect: {e}"));
    cairn_sync::Engine {
        tenant_id: tenant.into(),
        project_id: project.into(),
        device_id: device_id.into(),
        local_ns: project.into(),
        author_id: device_id.into(),
        store,
        cas,
        outbox: Outbox::new(conn.clone()),
        headers: HeaderCache::new(conn),
        plane: Arc::new(plane),
        dicts: cairn_core::compress::DictRegistry::new(),
        gate: cairn_sync::Gate::new(),
    }
}

/// Drop every engine handle without flush/close and reopen from the home's
/// disk — the kill -9 + restart analogue (WAL replay restores committed
/// rows; nothing else is preserved). The meeting-point URL is passed
/// explicitly: the crash dropped the engine that carried it.
pub async fn crash_and_restart(home: &mut Home, grpc_url: &str, token: &str) {
    let root = home.root.path().to_path_buf();
    let device_id = home.device_id.clone();
    let tenant = home.tenant_id.clone();
    let project = home.project_id.clone();
    home.engine = None; // abrupt: no cleanup, no flush — the kill -9 half
    home.engine = Some(open_engine(&root, grpc_url, token, &device_id, &tenant, &project).await);
}

impl Home {
    /// Daemon-like pass: sync (push dirty, pull remote) THEN materialize —
    /// the exact run_loop order in cairn-cli/src/projects.rs.
    pub async fn pass(&mut self) -> (cairn_sync::PassStats, cairn_sync::hydrate::HydrateStats) {
        let engine = self.engine.as_mut().expect("home is live");
        let stats = engine.sync_pass().await.expect("sync pass");
        let hstats = self.hydrate().await;
        (stats, hstats)
    }

    /// Sync-only pass (no hydration) — parks a home BETWEEN "metadata
    /// applied" and "bytes materialized".
    pub async fn sync_only(&mut self) -> cairn_sync::PassStats {
        let engine = self.engine.as_mut().expect("home is live");
        engine.sync_pass().await.expect("sync pass")
    }

    /// Materialize missing bytes only (the hydrate half of a pass).
    pub async fn hydrate(&self) -> cairn_sync::hydrate::HydrateStats {
        let engine = self.engine.as_ref().expect("home is live");
        cairn_sync::hydrate::materialize_missing(
            engine.plane.as_ref(),
            None,
            &engine.store,
            &engine.cas,
            &engine.headers,
            &self.tenant_id,
            &self.project_id,
        )
        .await
        .expect("materialize")
    }

    /// The home's PROJECT ROOT (what a real attach binds): <home>/store/workspace.
    pub fn ws(&self) -> PathBuf {
        let p = self.root.path().join("store").join("workspace");
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// Write a real file into the home's project root and mark it dirty in
    /// the store (what the scanner does for a local edit).
    pub fn write_and_mark_dirty(&self, rel: &str, bytes: &[u8]) {
        let path = self.ws().join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, bytes).unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        let engine = self.engine.as_ref().expect("home is live");
        engine
            .store
            .put_file(&FileRow {
                path: rel.into(),
                project_id: self.project_id.clone(),
                manifest_hash: None,
                size: meta.len() as u64,
                mode: "file".into(),
                mtime: cairn_sync::scan::mtime_millis(&meta),
                local_state: LocalState::Dirty.as_str().into(),
            })
            .unwrap();
    }

    /// blake3 hex of a materialized file in the home's project root.
    pub fn blake3_of(&self, rel: &str) -> String {
        let bytes = std::fs::read(self.ws().join(rel))
            .unwrap_or_else(|e| panic!("home {}: read {rel}: {e}", self.tag));
        blake3::hash(&bytes).to_hex().to_string()
    }
}

/// Converge two homes: alternate full passes until a round does no work.
pub async fn converge(a: &mut Home, b: &mut Home, max_rounds: usize) {
    for _ in 0..max_rounds {
        let mut work = 0u64;
        let (s, h) = a.pass().await;
        work += u64::from(s.appended) + u64::from(s.uploaded_chunks) + h.materialized;
        let (s, h) = b.pass().await;
        work += u64::from(s.appended) + u64::from(s.uploaded_chunks) + h.materialized;
        if work == 0 {
            break;
        }
    }
}

/// Deterministic pseudo-random payload (xorshift64 — incompressible-ish,
/// no external deps), the same generator shape `two_machine_stream.rs` uses.
pub fn payload(len: usize, seed: u64) -> Vec<u8> {
    let mut out = vec![0u8; len];
    let mut x = seed | 1;
    for b in out.iter_mut() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *b = (x >> 33) as u8;
    }
    out
}

/// True when both ffmpeg AND ffprobe are runnable (the real-media E2Es
/// degrade to honest skips without them; CI ships them).
pub fn ffmpeg_and_ffprobe_present() -> bool {
    let ok = |bin: &str| {
        std::process::Command::new(bin)
            .arg("-version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    };
    ok("ffmpeg") && ok("ffprobe")
}
