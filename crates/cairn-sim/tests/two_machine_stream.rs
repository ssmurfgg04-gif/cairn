//! Two-machine file streaming simulation: device A authors a multi-chunk
//! media file, syncs it to the server; device B syncs + materializes it.
//! Asserts byte-identity end to end and reports stage timings from the new
//! `PassStats` ingest counters (P1 measurement) so regressions show up as
//! numbers, not vibes.

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use async_trait::async_trait;
use cairn_core::CairnError;
use cairn_proto::pb::{JournalOp, UploadReceipt};
use cairn_sim::world::World;
use cairn_store::state::LocalState;
use cairn_store::FileRow;
use cairn_sync::plane::{CompleteOut, Entry, Plane, Session};
use rand::{Rng, SeedableRng};

/// Deterministic incompressible-ish payload (xorshift64 — no external deps).
fn payload(len: usize, seed: u64) -> Vec<u8> {
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

/// Daemon-like pass: sync (push dirty, pull remote) THEN materialize —
/// the exact run_loop order in cairn-cli/src/projects.rs.
/// Returns (sync stats, hydrate stats).
async fn pass(
    world: &mut World,
    i: usize,
) -> (cairn_sync::PassStats, cairn_sync::hydrate::HydrateStats) {
    let dev = &mut world.devices[i];
    let engine = dev.engine.as_mut().expect("device live");
    let stats = engine.sync_pass().await.expect("sync pass");
    let hstats = cairn_sync::hydrate::materialize_missing(
        engine.plane.as_ref(),
        None, // sim: plane-only hydration (swarm transport is ADR-0017, sim-side TBD)
        &engine.store,
        &engine.cas,
        &engine.headers,
        "t1",
        "p1",
    )
    .await
    .expect("materialize");
    (stats, hstats)
}

fn mark_dirty(world: &mut World, i: usize, path: &str, len: u64) {
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

fn ws(world: &World, i: usize) -> std::path::PathBuf {
    let p = world.devices[i].root.path().join("store").join("workspace");
    std::fs::create_dir_all(&p).unwrap();
    p
}

/// Latency-injecting plane wrapper: adds `delay` to every chunk PUT and
/// chunk GET while recording peak in-flight concurrency. Lets the test
/// prove overlap deterministically (no wall-clock assertions).
struct LatencyPlane {
    inner: Arc<dyn Plane>,
    delay: std::time::Duration,
    put_in_flight: AtomicUsize,
    put_peak: Arc<AtomicUsize>,
    get_in_flight: AtomicUsize,
    get_peak: Arc<AtomicUsize>,
}

impl LatencyPlane {
    fn track(entry: &AtomicUsize, peak: &Arc<AtomicUsize>) {
        let cur = entry.fetch_add(1, Ordering::SeqCst) + 1;
        peak.fetch_max(cur, Ordering::SeqCst);
    }

    fn release(entry: &AtomicUsize) {
        entry.fetch_sub(1, Ordering::SeqCst);
    }
}

#[async_trait]
impl Plane for LatencyPlane {
    async fn batch_exists(&self, t: &str, h: &[String]) -> Result<Vec<String>, CairnError> {
        self.inner.batch_exists(t, h).await
    }
    async fn create_session(
        &self,
        t: &str,
        d: &str,
        p: &str,
        m: &[String],
    ) -> Result<Session, CairnError> {
        self.inner.create_session(t, d, p, m).await
    }
    async fn complete(&self, s: &str, r: &[UploadReceipt]) -> Result<CompleteOut, CairnError> {
        self.inner.complete(s, r).await
    }
    async fn put_presigned(
        &self,
        url: &str,
        bytes: &[u8],
        checksum_hex: &str,
    ) -> Result<(), CairnError> {
        Self::track(&self.put_in_flight, &self.put_peak);
        tokio::time::sleep(self.delay).await;
        let r = self.inner.put_presigned(url, bytes, checksum_hex).await;
        Self::release(&self.put_in_flight);
        r
    }
    async fn put_manifest(&self, t: &str, mh: &str, b: &[u8]) -> Result<(), CairnError> {
        self.inner.put_manifest(t, mh, b).await
    }
    async fn get_manifest(&self, t: &str, mh: &str) -> Result<Vec<u8>, CairnError> {
        self.inner.get_manifest(t, mh).await
    }
    async fn fetch_object(&self, t: &str, h: &str) -> Result<Vec<u8>, CairnError> {
        Self::track(&self.get_in_flight, &self.get_peak);
        tokio::time::sleep(self.delay).await;
        let r = self.inner.fetch_object(t, h).await;
        Self::release(&self.get_in_flight);
        r
    }
    async fn append(
        &self,
        t: &str,
        p: &str,
        d: &str,
        rid: &str,
        op: JournalOp,
        tok: u64,
    ) -> Result<(u64, bool), CairnError> {
        self.inner.append(t, p, d, rid, op, tok).await
    }
    async fn fetch_batch(
        &self,
        t: &str,
        p: &str,
        after: u64,
        limit: u32,
    ) -> Result<Vec<Entry>, CairnError> {
        self.inner.fetch_batch(t, p, after, limit).await
    }
}

/// A 10 MiB pseudo-random file streams A → server → B byte-identical:
/// multi-chunk ingest, upload session, fold, pull, materialize.
#[tokio::test]
async fn two_machine_file_stream_is_byte_identical() {
    let mut world = World::boot(42).await;
    // 10 MiB: above the 8 MiB inline threshold, so the rayon offload lane
    // runs, and big enough for several coarse chunks.
    let bytes = payload(10 * 1024 * 1024, 0xC41A7);
    std::fs::write(ws(&world, 0).join("reel01.braw"), &bytes).unwrap();
    mark_dirty(&mut world, 0, "reel01.braw", bytes.len() as u64);

    let t0 = std::time::Instant::now();
    let (a_stats, _) = pass(&mut world, 0).await;
    let push_dt = t0.elapsed();
    assert!(
        a_stats.uploaded_chunks > 0,
        "A must upload chunks, got {a_stats:?}"
    );

    let t1 = std::time::Instant::now();
    let (_, b_hstats) = pass(&mut world, 1).await;
    let pull_dt = t1.elapsed();

    let got = std::fs::read(ws(&world, 1).join("reel01.braw")).unwrap();
    assert_eq!(
        got.len(),
        bytes.len(),
        "B materialized short file: {} vs {} bytes",
        got.len(),
        bytes.len()
    );
    assert_eq!(got, bytes, "B's bytes must equal A's bytes exactly");

    println!(
        "STREAM push={push_dt:?} (read={}ms hash={}ms cas={}ms net={}ms up={} skip={}) \
         pull+hydrate={pull_dt:?} (mat={}) size={}B",
        a_stats.read_ms,
        a_stats.hash_ms,
        a_stats.cas_ms,
        a_stats.net_ms,
        a_stats.uploaded_chunks,
        a_stats.skipped_chunks,
        b_hstats.materialized,
        bytes.len(),
    );

    // Re-dirty the identical bytes: ingest must re-run, find every chunk
    // in CAS, and upload nothing (content-addressed dedup).
    mark_dirty(&mut world, 0, "reel01.braw", bytes.len() as u64);
    let (a2, _) = pass(&mut world, 0).await;
    assert_eq!(
        a2.uploaded_chunks, 0,
        "re-push of identical bytes must upload nothing, got {a2:?}"
    );
    assert!(
        a2.skipped_chunks > 0,
        "re-push of identical bytes must skip known chunks, got {a2:?}"
    );
}

/// Packet-loss injector: fails a seeded fraction of plane calls with a
/// retryable `Unavailable` (what a flaky link looks like below the HTTP
/// layer — timeouts/reset chunks, not clean partitions). Wraps the real
/// in-proc plane, so every fault below is one the engine must absorb.
struct FlakyPlane {
    inner: Arc<dyn Plane>,
    rng: std::sync::Mutex<rand::rngs::StdRng>,
    loss_rate: f64,
    injected: Arc<AtomicUsize>,
    calls: Arc<AtomicUsize>,
}

impl FlakyPlane {
    fn maybe_fail(&self, op: &str) -> Option<CairnError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let drop_it = {
            let mut rng = self.rng.lock().unwrap();
            rng.gen_bool(self.loss_rate)
        };
        if drop_it {
            self.injected.fetch_add(1, Ordering::SeqCst);
            Some(CairnError::new(
                cairn_core::ErrorKind::Unavailable,
                format!("simulated packet loss on {op}"),
            ))
        } else {
            None
        }
    }
}

#[async_trait]
impl Plane for FlakyPlane {
    async fn batch_exists(&self, t: &str, h: &[String]) -> Result<Vec<String>, CairnError> {
        if let Some(e) = self.maybe_fail("batch_exists") {
            return Err(e);
        }
        self.inner.batch_exists(t, h).await
    }
    async fn create_session(
        &self,
        t: &str,
        d: &str,
        p: &str,
        m: &[String],
    ) -> Result<Session, CairnError> {
        if let Some(e) = self.maybe_fail("create_session") {
            return Err(e);
        }
        self.inner.create_session(t, d, p, m).await
    }
    async fn complete(&self, s: &str, r: &[UploadReceipt]) -> Result<CompleteOut, CairnError> {
        if let Some(e) = self.maybe_fail("complete") {
            return Err(e);
        }
        self.inner.complete(s, r).await
    }
    async fn put_presigned(
        &self,
        url: &str,
        bytes: &[u8],
        checksum_hex: &str,
    ) -> Result<(), CairnError> {
        if let Some(e) = self.maybe_fail("put") {
            return Err(e);
        }
        self.inner.put_presigned(url, bytes, checksum_hex).await
    }
    async fn put_manifest(&self, t: &str, mh: &str, b: &[u8]) -> Result<(), CairnError> {
        if let Some(e) = self.maybe_fail("put_manifest") {
            return Err(e);
        }
        self.inner.put_manifest(t, mh, b).await
    }
    async fn get_manifest(&self, t: &str, mh: &str) -> Result<Vec<u8>, CairnError> {
        if let Some(e) = self.maybe_fail("get_manifest") {
            return Err(e);
        }
        self.inner.get_manifest(t, mh).await
    }
    async fn fetch_object(&self, t: &str, h: &str) -> Result<Vec<u8>, CairnError> {
        if let Some(e) = self.maybe_fail("fetch") {
            return Err(e);
        }
        self.inner.fetch_object(t, h).await
    }
    async fn append(
        &self,
        t: &str,
        p: &str,
        d: &str,
        rid: &str,
        op: JournalOp,
        tok: u64,
    ) -> Result<(u64, bool), CairnError> {
        if let Some(e) = self.maybe_fail("append") {
            return Err(e);
        }
        self.inner.append(t, p, d, rid, op, tok).await
    }
    async fn fetch_batch(
        &self,
        t: &str,
        p: &str,
        after: u64,
        limit: u32,
    ) -> Result<Vec<Entry>, CairnError> {
        if let Some(e) = self.maybe_fail("fetch_batch") {
            return Err(e);
        }
        self.inner.fetch_batch(t, p, after, limit).await
    }
}

/// Loss-tolerant pass: a pass may fail outright under loss (single-shot
/// metadata calls); the daemon just runs the next pass. Returns None on
/// pass failure, Some((sync, hydrate, progress)) otherwise.
async fn try_pass(
    world: &mut World,
    i: usize,
) -> Option<(cairn_sync::PassStats, cairn_sync::hydrate::HydrateStats)> {
    let dev = &mut world.devices[i];
    let engine = dev.engine.as_mut().expect("device live");
    let stats = engine.sync_pass().await.ok()?;
    let hstats = cairn_sync::hydrate::materialize_missing(
        engine.plane.as_ref(),
        None,
        &engine.store,
        &engine.cas,
        &engine.headers,
        "t1",
        "p1",
    )
    .await
    .ok()?;
    Some((stats, hstats))
}

/// Chunk PUTs overlap (upload path) and chunk GETs overlap (hydrate path):
/// with 150ms injected per-object latency on a 3-chunk file, peak
/// in-flight concurrency must exceed 1 on both sides. Deterministic —
/// no wall-clock assertions, just the overlap counters.
#[tokio::test]
async fn chunk_transfers_overlap_under_latency() {
    let mut world = World::boot(7).await;
    let put_peak = Arc::new(AtomicUsize::new(0));
    let get_peak = Arc::new(AtomicUsize::new(0));
    for i in 0..2 {
        let dev = &mut world.devices[i];
        let engine = dev.engine.as_mut().expect("device live");
        let inner = Arc::clone(&engine.plane);
        engine.plane = Arc::new(LatencyPlane {
            inner,
            delay: std::time::Duration::from_millis(150),
            put_in_flight: AtomicUsize::new(0),
            put_peak: Arc::clone(&put_peak),
            get_in_flight: AtomicUsize::new(0),
            get_peak: Arc::clone(&get_peak),
        });
    }

    let bytes = payload(10 * 1024 * 1024, 0xC41A7);
    std::fs::write(ws(&world, 0).join("reel02.braw"), &bytes).unwrap();
    mark_dirty(&mut world, 0, "reel02.braw", bytes.len() as u64);
    let (a_stats, _) = pass(&mut world, 0).await;
    assert!(
        a_stats.uploaded_chunks >= 2,
        "need ≥2 chunks for an overlap proof, got {a_stats:?}"
    );
    let (_, _) = pass(&mut world, 1).await;
    let got = std::fs::read(ws(&world, 1).join("reel02.braw")).unwrap();
    assert_eq!(got, bytes, "latency path must stay byte-identical");

    assert!(
        put_peak.load(Ordering::SeqCst) >= 2,
        "chunk PUTs must overlap: peak in-flight = {}",
        put_peak.load(Ordering::SeqCst)
    );
    assert!(
        get_peak.load(Ordering::SeqCst) >= 2,
        "chunk GETs must overlap: peak in-flight = {}",
        get_peak.load(Ordering::SeqCst)
    );
    println!(
        "OVERLAP put_peak={} get_peak={}",
        put_peak.load(Ordering::SeqCst),
        get_peak.load(Ordering::SeqCst)
    );
}

/// Flaky-link convergence: 30% seeded packet loss on EVERY plane call,
/// both devices. Sync must still converge byte-identical — per-call
/// retries absorb the common case, pass-level retry (durable outbox +
/// idempotent uploads) absorbs the rest. The injected-fault counter is
/// asserted nonzero so the test can't pass on a quiet link.
#[tokio::test]
async fn sync_converges_over_flaky_packet_loss_network() {
    let mut world = World::boot(11).await;
    let injected = Arc::new(AtomicUsize::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    for i in 0..2 {
        let dev = &mut world.devices[i];
        let engine = dev.engine.as_mut().expect("device live");
        let inner = Arc::clone(&engine.plane);
        engine.plane = Arc::new(FlakyPlane {
            inner,
            rng: std::sync::Mutex::new(rand::rngs::StdRng::seed_from_u64(1000 + i as u64)),
            loss_rate: 0.30,
            injected: Arc::clone(&injected),
            calls: Arc::clone(&calls),
        });
    }

    let bytes = payload(10 * 1024 * 1024, 0xF1414E59);
    std::fs::write(ws(&world, 0).join("reel03.braw"), &bytes).unwrap();
    mark_dirty(&mut world, 0, "reel03.braw", bytes.len() as u64);

    // Settle: alternate devices; stop after 2 consecutive quiet rounds
    // (no uploads, appends, or materializations) or 14 rounds max.
    let mut quiet = 0u32;
    let mut rounds = 0u32;
    for _ in 0..14 {
        rounds += 1;
        let mut progress = 0u64;
        for i in 0..2 {
            if let Some((s, h)) = try_pass(&mut world, i).await {
                progress += u64::from(s.uploaded_chunks) + u64::from(s.appended) + h.materialized;
            } else {
                // failed pass still counts as activity (work remains)
                progress += 1;
            }
        }
        if progress == 0 {
            quiet += 1;
            if quiet >= 2 {
                break;
            }
        } else {
            quiet = 0;
        }
    }

    let lost = injected.load(Ordering::SeqCst);
    assert!(
        lost > 0,
        "chaos gate: zero faults injected, link was quiet (seed/rate broken?)"
    );
    let got = std::fs::read(ws(&world, 1).join("reel03.braw")).unwrap();
    assert_eq!(got.len(), bytes.len(), "B materialized short under loss");
    assert_eq!(got, bytes, "B's bytes must equal A's bytes despite loss");
    println!(
        "FLAKY rounds={rounds} plane_calls={} injected_faults={lost} size={}B",
        calls.load(Ordering::SeqCst),
        bytes.len()
    );
}
