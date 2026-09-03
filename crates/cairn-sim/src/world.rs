//! The simulated world: server + N devices + schedule driver + assertions (a)–(d).

use std::sync::Arc;

use cairn_core::hash::Hash;
use cairn_proto::pb::journal_op::Op as OpKind;
use cairn_proto::pb::FileUpsertOp;
use cairn_store::state::LocalState;
use cairn_store::{Cas, FileRow, HeaderCache, Outbox, Store};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use sqlx::Row;
use std::sync::atomic::Ordering;

use crate::plane::{Faults, InProcPlane};

/// One simulated device (own store root, own engine).
pub struct Device {
    pub id: String,
    pub root: tempfile::TempDir,
    pub engine: Option<cairn_sync::Engine>,
    pub crashes: u32,
}

pub struct World {
    pub server_dir: tempfile::TempDir,
    pub state: Arc<cairn_server::ServerState>,
    pub faults: Arc<Faults>,
    pub devices: Vec<Device>,
    pub acked_appends: u64,
}

async fn server_state(dir: &std::path::Path) -> Arc<cairn_server::ServerState> {
    let db = cairn_server::db::open(&dir.join("meta.db")).await.unwrap();
    cairn_server::db::migrate(&db).await.unwrap();
    let auth = cairn_server::auth::Authenticator::load_or_create(
        &dir.join("keys"),
        Arc::new(cairn_core::clock::WallClock),
    )
    .unwrap();
    let store = cairn_server::storage::LocalFsStore::open(
        &dir.join("objects"),
        b"sim-object-key",
        "http://127.0.0.1:1/",
    )
    .unwrap();
    let state = cairn_server::ServerState {
        db,
        auth,
        store: Arc::new(store),
        bloom: tokio::sync::RwLock::new(cairn_core::bloom::Bloom::empty()),
        clock: Arc::new(cairn_core::clock::WallClock),
        dev_insecure: true,
    };
    state.migrate().await.unwrap();
    Arc::new(state)
}

fn open_device(
    root: &std::path::Path,
    tenant: &str,
    project: &str,
    id: &str,
    state: Arc<cairn_server::ServerState>,
    faults: Arc<Faults>,
) -> cairn_sync::Engine {
    let store = Store::open(&root.join("store"), Arc::new(cairn_core::clock::WallClock)).unwrap();
    let conn = store.conn_handle();
    let cas = Cas::open(&root.join("blobs"), conn.clone()).unwrap();
    let outbox = Outbox::new(conn.clone());
    let headers = HeaderCache::new(conn);
    cairn_sync::Engine {
        tenant_id: tenant.into(),
        project_id: project.into(),
        device_id: id.into(),
        store,
        cas,
        outbox,
        headers,
        plane: Arc::new(InProcPlane {
            state,
            tenant_id: tenant.into(),
            device_id: id.into(),
            faults,
        }),
        dicts: cairn_core::compress::DictRegistry::new(),
        gate: cairn_sync::Gate::new(),
    }
}

impl World {
    /// Boot a fresh world with 2 devices.
    pub async fn boot(seed: u64) -> Self {
        let _ = seed;
        let server_dir = tempfile::tempdir().unwrap();
        let state = server_state(server_dir.path()).await;
        sqlx_insert(
            &state,
            "INSERT OR IGNORE INTO tenants(id, created_at) VALUES('t1',0)",
        )
        .await;
        sqlx_insert(
            &state,
            "INSERT OR IGNORE INTO projects(tenant_id, project_id, created_at) VALUES('t1','p1',0)",
        )
        .await;
        let faults = Arc::new(Faults::default());
        let mut devices = Vec::new();
        for i in 0..2 {
            let root = tempfile::tempdir().unwrap();
            let engine = open_device(
                root.path(),
                "t1",
                "p1",
                &format!("dev-{i}"),
                Arc::clone(&state),
                Arc::clone(&faults),
            );
            devices.push(Device {
                id: format!("dev-{i}"),
                root,
                engine: Some(engine),
                crashes: 0,
            });
        }
        World {
            server_dir,
            state,
            faults,
            devices,
            acked_appends: 0,
        }
    }

    fn dev(&mut self, i: usize) -> &mut Device {
        let idx = i % self.devices.len();
        &mut self.devices[idx]
    }

    /// Kill -9 analogue: abandon the engine (drop handles), reopen from disk. WAL replay
    /// must restore every committed row.
    pub fn crash_device(&mut self, i: usize) {
        let idx = i % self.devices.len();
        let (state, faults, root_path, id) = {
            let d = &mut self.devices[idx];
            d.engine = None; // abrupt: no cleanup, no flush
            d.crashes += 1;
            (
                Arc::clone(&self.state),
                Arc::clone(&self.faults),
                d.root.path().to_path_buf(),
                d.id.clone(),
            )
        };
        let d = &mut self.devices[idx];
        d.engine = Some(open_device(&root_path, "t1", "p1", &id, state, faults));
    }

    /// One schedule tick: edit, sync, crash, partition, or fold — chosen by the seeded rng.
    pub async fn tick(&mut self, rng: &mut StdRng, tick: u64) {
        let action = rng.gen_range(0..7u8);
        match action {
            0..=2 => {
                // device edits a file locally (localized write like an NLE autosave)
                let i = rng.gen_range(0..self.devices.len());
                let file_idx = rng.gen_range(0..4u32);
                let path = format!("file{file_idx}.bin");
                let root = self.devices[i].root.path().to_path_buf();
                let ws = root.join("store").join("workspace");
                std::fs::create_dir_all(&ws).unwrap();
                let content = format!(
                    "content device {i} file {file_idx} tick {tick} seed-edit {}",
                    rng.gen::<u32>()
                );
                std::fs::write(ws.join(&path), &content).unwrap();
                let dev = self.dev(i);
                let engine = dev.engine.as_mut().expect("device live");
                engine
                    .store
                    .put_file(&FileRow {
                        path,
                        project_id: "p1".into(),
                        manifest_hash: None,
                        size: content.len() as u64,
                        mode: "file".into(),
                        mtime: tick as i64,
                        local_state: LocalState::Dirty.as_str().into(),
                    })
                    .unwrap();
            }
            3 | 4 => {
                // sync pass on a live device
                let i = rng.gen_range(0..self.devices.len());
                let i = i % self.devices.len();
                let dev = &mut self.devices[i];
                if let Some(engine) = dev.engine.as_mut() {
                    match engine.sync_pass().await {
                        Ok(stats) => self.acked_appends += u64::from(stats.appended),
                        Err(e) if e.code() == "UNAVAILABLE" => {} // partitioned; retried next tick
                        Err(e) if e.code() == "CONFLICT" => {}    // conflict copy path exercised
                        Err(e) if e.code() == "STALE_LEASE" => {} // surfaced to user (§14)
                        Err(e) => panic!("unexpected sync error: {e}"),
                    }
                }
            }
            5 => {
                // partition toggle
                let on = self.faults.partition.load(Ordering::SeqCst);
                self.faults.partition.store(!on, Ordering::SeqCst);
            }
            _ => {
                // crash a device mid-flight (I2 target)
                let i = rng.gen_range(0..self.devices.len());
                self.crash_device(i);
            }
        }
    }

    /// Final assertions (a)–(d) for the schedule.
    pub async fn verify(&mut self) -> crate::Verdict {
        // settle: heal partition, sync both devices until quiet
        self.faults.partition.store(false, Ordering::SeqCst);
        for _ in 0..4 {
            for i in 0..self.devices.len() {
                let idx = i % self.devices.len();
                let engine = self.devices[idx].engine.as_mut();
                if let Some(engine) = engine {
                    if let Ok(stats) = engine.sync_pass().await {
                        self.acked_appends += u64::from(stats.appended);
                    }
                }
            }
        }

        // (a) every acknowledged append is in the journal (vacuously true when
        // the fault script allowed none — `vacuous` below marks that regime).
        let journal_count: i64 = sqlx_count(&self.state, "SELECT COUNT(*) FROM journal").await;
        let acked_ok = journal_count >= i64::try_from(self.acked_appends).unwrap_or(i64::MAX);
        let vacuous = self.acked_appends == 0;

        // (b) convergence: devices' live views match the folded snapshot view.
        // An empty view is converged UNLESS some device believes it synced a
        // file — that would be a lost append (real violation, not vacuous).
        let view = cairn_server::fold::materialize(&self.state.db, "t1", "p1", 0)
            .await
            .unwrap();
        let mut converged = true;
        if view.is_empty() {
            for d in &self.devices {
                if let Some(engine) = &d.engine {
                    let synced = engine
                        .store
                        .list_files("p1")
                        .into_iter()
                        .filter(|f| f.local_state == LocalState::Synced.as_str())
                        .count();
                    if synced > 0 {
                        converged = false;
                    }
                }
            }
        }
        for (path, ps) in &view {
            if let cairn_server::fold::PathState::Present {
                manifest_hash,
                size,
            } = ps
            {
                for d in &self.devices {
                    if let Some(engine) = &d.engine {
                        match engine.store.get_file("p1", path) {
                            Some(f) => {
                                if f.manifest_hash.as_deref() != Some(manifest_hash.as_str())
                                    || f.size != *size
                                {
                                    converged = false;
                                }
                            }
                            None => converged = false,
                        }
                    }
                }
            }
        }

        // (c) no corrupt manifest: every registered manifest parses + hash matches
        let rows = sqlx_all(&self.state, "SELECT hash, size FROM manifests").await;
        let mut no_corrupt = true;
        for (hash, size) in &rows {
            let bytes = self
                .state
                .store
                .get(&cairn_server::storage::LocalFsStore::object_key("t1", hash))
                .await
                .unwrap_or_default();
            if bytes.len() != *size || Hash::of(&bytes).hex() != *hash {
                no_corrupt = false;
            }
        }

        // (d) GC shadow (d): reachable objects never flagged; full GC lands at M6 — here the
        // shadow pass verifies every chunk referenced by live manifests exists (the invariant
        // GC must preserve).
        let mut shadow_clean = true;
        for (path, ps) in &view {
            if let cairn_server::fold::PathState::Present { manifest_hash, .. } = ps {
                if let Ok(bytes) = self
                    .state
                    .store
                    .get(&cairn_server::storage::LocalFsStore::object_key(
                        "t1",
                        manifest_hash,
                    ))
                    .await
                {
                    if let Ok(m) = cairn_core::manifest::Manifest::parse(&bytes) {
                        // Fanout-safe (review round): `flatten()` is leaf-only; a Node
                        // made this shadow check vacuous for >8,192-chunk files. Walk
                        // the tree through the async store (depth-guarded).
                        for e in collect_store_manifest_entries(self, "t1", &m, 0).await {
                            let key = cairn_server::storage::LocalFsStore::chunk_key(
                                "t1",
                                &e.chunk_hash.hex(),
                            );
                            if self.state.store.head(&key).await.is_err() {
                                tracing::error!(%path, chunk = %e.chunk_hash, "GC shadow violation");
                                shadow_clean = false;
                            }
                        }
                    }
                }
            }
        }

        crate::Verdict {
            acked_appends_survived: acked_ok,
            devices_converged: converged,
            no_corrupt_manifests: no_corrupt,
            gc_shadow_clean: shadow_clean,
            ticks: 0,
            appends_acked: self.acked_appends,
            vacuous,
        }
    }
}

/// Fanout-safe manifest walk for the GC shadow check (review round): recursively
/// collects every leaf entry of a manifest tree, fetching child manifest objects
/// through the server's async object store. Depth-guarded (content-addressed trees
/// cannot cycle; the guard is against a corrupt store serving manifest-shaped loops).
async fn collect_store_manifest_entries(
    world: &World,
    tenant: &str,
    m: &cairn_core::manifest::Manifest,
    depth: u32,
) -> Vec<cairn_core::manifest::ManifestEntry> {
    Box::pin(collect_store_manifest_entries_inner(
        world, tenant, m, depth,
    ))
    .await
}

async fn collect_store_manifest_entries_inner(
    world: &World,
    tenant: &str,
    m: &cairn_core::manifest::Manifest,
    depth: u32,
) -> Vec<cairn_core::manifest::ManifestEntry> {
    use cairn_core::manifest::Manifest;
    const MAX_DEPTH: u32 = 8;
    match m {
        Manifest::Leaf { entries, .. } => entries.clone(),
        Manifest::Node { children, .. } => {
            let mut out = Vec::new();
            if depth > MAX_DEPTH {
                tracing::error!(%tenant, depth, "manifest tree beyond MAX_DEPTH in sim shadow check");
                return out;
            }
            for c in children {
                let Ok(bytes) = world
                    .state
                    .store
                    .get(&cairn_server::storage::LocalFsStore::object_key(
                        tenant,
                        &c.hash.hex(),
                    ))
                    .await
                else {
                    continue;
                };
                if let Ok(child) = Manifest::parse(&bytes) {
                    out.extend(
                        collect_store_manifest_entries(world, tenant, &child, depth + 1).await,
                    );
                }
            }
            out
        }
    }
}

/// Run one full schedule.
pub async fn run_schedule(seed: u64, ticks: u64) -> crate::Verdict {
    let mut world = World::boot(seed).await;
    let mut rng = StdRng::seed_from_u64(seed);
    for tick in 0..ticks {
        world.tick(&mut rng, tick).await;
    }
    let mut v = world.verify().await;
    v.ticks = ticks;
    v
}

async fn sqlx_insert(state: &cairn_server::ServerState, sql: &str) {
    sqlx::query(sql).execute(&state.db).await.unwrap();
}

async fn sqlx_count(state: &cairn_server::ServerState, sql: &str) -> i64 {
    sqlx::query_scalar(sql)
        .fetch_one(&state.db)
        .await
        .unwrap_or(0)
}

async fn sqlx_all(state: &cairn_server::ServerState, sql: &str) -> Vec<(String, usize)> {
    sqlx::query(sql)
        .fetch_all(&state.db)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|r| {
            (
                r.try_get::<String, _>(0).unwrap_or_default(),
                r.try_get::<i64, _>(1).unwrap_or(0).max(0) as usize,
            )
        })
        .collect()
}

// keep unused-proto imports referenced
#[allow(dead_code)]
fn _probe(_: FileUpsertOp, _: OpKind) {}

#[cfg(test)]
mod w5_tests {
    use super::*;

    /// W5 (round 13), the deterministic-conflict recipe the windows matrix
    /// drives, replayed here against the REAL server + REAL journal conflict
    /// rule: device B's local edit is UNDISCOVERED (edit-discovery lag: the
    /// row still says clean -- exactly what a plain-file workspace does
    /// between scans), device A edits live and appends. B's pull must NOT
    /// clobber B's bytes (pre-fix: materialize overwrote them silently), and
    /// B's append must NOT supersede A's linearly (pre-fix: the cursor-based
    /// base_seq made the server accept it with NO conflict copy). Contract:
    /// exactly ONE conflict copy, the original path converges to A's version
    /// on BOTH devices, the copy carries B's version.
    #[tokio::test]
    async fn conflict_copy_survives_edit_discovery_lag() {
        let mut world = World::boot(7).await;
        // roots captured UP FRONT (TempDirs live in `world`; the closure must
        // not hold a borrow across the mutable pass calls)
        let ws: Vec<std::path::PathBuf> = (0..2usize)
            .map(|i| {
                let p = world.devices[i].root.path().join("store").join("workspace");
                std::fs::create_dir_all(&p).unwrap();
                p
            })
            .collect();
        let wspath = |i: usize| ws[i].clone();
        // daemon-like pass: sync (push dirty, pull remote) THEN materialize
        // (the exact run_loop order in cairn-cli/src/projects.rs)
        async fn pass(world: &mut World, i: usize) {
            let dev = &mut world.devices[i];
            let engine = dev.engine.as_mut().expect("device live");
            engine.sync_pass().await.expect("sync pass");
            cairn_sync::hydrate::materialize_missing(
                engine.plane.as_ref(),
                &engine.store,
                &engine.cas,
                &engine.headers,
                "t1",
                "p1",
            )
            .await
            .expect("materialize");
        }
        fn set_dirty(world: &mut World, i: usize, path: &str, len: u64) {
            let meta = std::fs::metadata(
                world.devices[i]
                    .root
                    .path()
                    .join("store/workspace")
                    .join(path),
            )
            .unwrap();
            let dev = &mut world.devices[i];
            let engine = dev.engine.as_mut().expect("device live");
            engine
                .store
                .put_file(&FileRow {
                    path: path.into(),
                    project_id: "p1".into(),
                    manifest_hash: None,
                    size: len,
                    mode: "file".into(),
                    mtime: cairn_sync::scan::mtime_millis(&meta),
                    local_state: LocalState::Dirty.as_str().into(),
                })
                .unwrap();
        }

        // (1) A authors the seed; both devices converge on v1
        std::fs::write(wspath(0).join("probe.bin"), b"seed").unwrap();
        set_dirty(&mut world, 0, "probe.bin", 4);
        pass(&mut world, 0).await;
        pass(&mut world, 1).await;
        assert_eq!(
            std::fs::read(wspath(1).join("probe.bin")).unwrap(),
            b"seed",
            "B must materialize the seed before the divergence"
        );

        // (2) B's UNDISCOVERED local edit: disk changes, the row still says
        // clean (edit-discovery lag). mtime bumped so the drift is unambiguous.
        std::fs::write(wspath(1).join("probe.bin"), b"seed+B-offline-edit").unwrap();
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(2);
        std::fs::File::options()
            .append(true)
            .open(wspath(1).join("probe.bin"))
            .unwrap()
            .set_modified(later)
            .unwrap();

        // (3) A edits LIVE (discovered) and appends: server head = v2A
        std::fs::write(wspath(0).join("probe.bin"), b"seed+A-live-edit").unwrap();
        set_dirty(&mut world, 0, "probe.bin", 15);
        pass(&mut world, 0).await;
        assert_eq!(
            std::fs::read(wspath(0).join("probe.bin")).unwrap(),
            b"seed+A-live-edit"
        );

        // (4) B syncs: the pull sees A's v2A over a clean-but-drifted row ->
        // guard re-dirties + forks; the push appends with the FORK lineage ->
        // server CONFLICT -> conflict copy; the re-pinned replay re-delivers
        // the winner to the original path; materialize writes it.
        pass(&mut world, 1).await;
        // B's bytes may already be at the copy now; settle both sides
        pass(&mut world, 0).await;
        pass(&mut world, 1).await;

        // (5) THE CONTRACT
        let copies: Vec<std::path::PathBuf> = [0usize, 1usize]
            .into_iter()
            .flat_map(|i| {
                let mut v: Vec<std::path::PathBuf> = Vec::new();
                for e in std::fs::read_dir(&ws[i]).unwrap() {
                    let e = e.unwrap();
                    if e.file_name()
                        .to_string_lossy()
                        .starts_with("probe (conflict")
                    {
                        v.push(e.path());
                    }
                }
                v
            })
            .collect();
        assert_eq!(copies.len(), 1, "exactly ONE conflict copy, saw {copies:?}");
        assert_eq!(
            std::fs::read(&copies[0]).unwrap(),
            b"seed+B-offline-edit",
            "the copy must carry B's divergent version"
        );
        for (i, w) in ws.iter().enumerate() {
            assert_eq!(
                std::fs::read(w.join("probe.bin")).unwrap(),
                b"seed+A-live-edit",
                "original path must converge to A's version on BOTH devices (device {i})"
            );
        }
    }
}
