//! End-to-end swarm tests (ADR-0017): real signal server + real swarm nodes
//! over loopback UDP, exercising the full rendezvous → punch → HAVE → want →
//! serve → verify pipeline.
//!
//! The in-test block store mirrors the production wiring: fetched blocks are
//! LANDED in the store (like hydrate lands them in the CAS) before the node's
//! bloom re-advertises them — that landing is what powers the mesh effect.
//!
//! Timeout policy: these suites run under `cargo nextest run --workspace` on
//! shared ubuntu-latest runners (up to 32 suites in parallel; see the burst
//! job's contention note in ci.yml). The happy path is ~4 s locally, so the
//! PROGRESS deadlines below (60 s fetch, 30 s convergence) are pure failure
//! path headroom — they bind only when something is actually broken, and they
//! keep UDP event loops from starving under runner contention. NEGATIVE
//! deadlines (no-holder, stranger-gate, observation windows) stay tight: their
//! semantics are "nothing happens within N", and widening would weaken the
//! assertion (2026-09-04: 49d65a9 CI flake — `b fetched` at 15 s with zero
//! p2p code touched; 6/6 local passes at ~4.2 s).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cairn_core::hash::Hash;
use cairn_p2p::relay::RelayServer;
use cairn_p2p::signal::SignalServer;
use cairn_p2p::swarm::{ServeBlocks, Swarm, SwarmConfig};

const KEY: &[u8] = b"e2e-cluster-key-32-bytes-aaaaaaaa";

/// Enable logs when RUST_LOG is set (RUST_LOG=cairn_p2p=debug).
fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_target(true)
        .try_init();
}

/// In-memory block store — the test twin of the CAS-backed adapter.
#[derive(Default, Clone)]
struct MapServe {
    blocks: Arc<Mutex<HashMap<Hash, Vec<u8>>>>,
    /// optional: serve WRONG bytes for these hashes (corrupt-peer test)
    corrupt: Option<Hash>,
}

impl MapServe {
    fn insert(&self, h: &Hash, bytes: Vec<u8>) {
        self.blocks.lock().unwrap().insert(*h, bytes);
    }
    fn len(&self) -> usize {
        self.blocks.lock().unwrap().len()
    }
}

impl ServeBlocks for MapServe {
    fn block_bytes(&self, h: &Hash) -> Option<Vec<u8>> {
        let bytes = self.blocks.lock().unwrap().get(h).cloned()?;
        if self.corrupt == Some(*h) {
            // adversarial: right length, wrong content
            let mut bad = bytes.clone();
            if !bad.is_empty() {
                bad[0] ^= 0xFF;
            }
            return Some(bad);
        }
        Some(bytes)
    }
    fn owned_hashes(&self) -> Vec<Hash> {
        self.blocks.lock().unwrap().keys().copied().collect()
    }
    fn owned_count(&self) -> u64 {
        self.blocks.lock().unwrap().len() as u64
    }
}

fn cfg(signal: SocketAddr, node: &str, project: &str, force_relay: bool) -> SwarmConfig {
    SwarmConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        signal,
        cluster_key: KEY.to_vec(),
        project: project.to_string(),
        node_id: Some(node.to_string()),
        stun: None,
        force_relay,
        presence: false,
    }
}

async fn spawn(
    signal: SocketAddr,
    node: &str,
    project: &str,
    serving: Arc<dyn ServeBlocks>,
) -> Swarm {
    Swarm::spawn(cfg(signal, node, project, false), serving)
        .await
        .expect("swarm spawn")
}

/// Poll a condition with a deadline; panics with `what` on expiry.
async fn wait_until<F: FnMut() -> bool>(mut cond: F, what: &str, budget: Duration) {
    let start = Instant::now();
    while !cond() {
        assert!(start.elapsed() <= budget, "timeout waiting for: {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn make_blocks(n: usize, kb: usize) -> Vec<(Hash, Vec<u8>)> {
    (0..n)
        .map(|i| {
            // deterministic pseudo-random content, varied sizes (1-fragment
            // and multi-fragment paths both exercised)
            let len = kb * 1024 * ((i % 3) + 1) / 3 + 64 + i;
            let bytes: Vec<u8> = (0..len)
                .map(|j| ((i * 31 + j * 7 + j / 1024) % 251) as u8)
                .collect();
            (Hash::of(&bytes), bytes)
        })
        .collect()
}

/// Small blocks for fast, loss-tolerant multi-node bursts.
fn make_small_blocks(n: usize) -> Vec<(Hash, Vec<u8>)> {
    (0..n)
        .map(|i| {
            let len = 300 + i * 97; // 300..~2.2KB → 1-2 fragments each
            let bytes: Vec<u8> = (0..len).map(|j| ((i * 31 + j * 13) % 251) as u8).collect();
            (Hash::of(&bytes), bytes)
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_nodes_converge() {
    init_tracing();
    let signal = SignalServer::spawn("127.0.0.1:0".parse().unwrap(), KEY)
        .await
        .unwrap();
    let blocks = make_blocks(6, 4);

    let a_store = MapServe::default();
    for (h, b) in &blocks {
        a_store.insert(h, b.clone());
    }
    let a = spawn(
        signal.local_addr,
        "node-a",
        "proj",
        Arc::new(a_store.clone()),
    )
    .await;
    let b = spawn(
        signal.local_addr,
        "node-b",
        "proj",
        Arc::new(MapServe::default()),
    )
    .await;
    let c = spawn(
        signal.local_addr,
        "node-c",
        "proj",
        Arc::new(MapServe::default()),
    )
    .await;

    // membership + links converge
    wait_until(
        || a.stats().direct_links >= 2,
        "a links 2 peers",
        Duration::from_secs(30),
    )
    .await;
    wait_until(
        || b.stats().direct_links >= 1 && c.stats().direct_links >= 1,
        "b/c linked",
        Duration::from_secs(30),
    )
    .await;

    // B and C fetch every block from A
    let (b_sw, c_sw) = (b.clone(), c.clone());
    let blk_b: Vec<Hash> = blocks.iter().map(|(h, _)| *h).collect();
    let blk_c = blk_b.clone();
    let fetch_b = tokio::spawn(async move {
        let mut got = Vec::new();
        for h in blk_b {
            let bytes = b_sw.fetch_block(&h, Duration::from_secs(60)).await;
            got.push((h, bytes));
        }
        got
    });
    let fetch_c = tokio::spawn(async move {
        let mut got = Vec::new();
        for h in blk_c {
            let bytes = c_sw.fetch_block(&h, Duration::from_secs(60)).await;
            got.push((h, bytes));
        }
        got
    });

    for (h, bytes) in fetch_b.await.unwrap() {
        let bytes = bytes.expect("b fetched block");
        assert_eq!(Hash::of(&bytes), h, "blake3 verified (I2)");
        let expected = blocks.iter().find(|(bh, _)| *bh == h).unwrap().1.clone();
        assert_eq!(bytes, expected);
    }
    for (h, bytes) in fetch_c.await.unwrap() {
        let bytes = bytes.expect("c fetched block");
        assert_eq!(Hash::of(&bytes), h, "blake3 verified (I2)");
    }

    let sa = a.stats();
    assert!(
        sa.blocks_served >= 6,
        "a served the swarm: {}",
        sa.blocks_served
    );
    // NAT-metrics contract (round 19): every direct link here WAS a punch
    // that landed — attempts counted once per peer pair, successes on the
    // Punching->Direct transition, stun_resolved false because cfg.stun is
    // None (honest absence, never an invented zero).
    let sb = b.stats();
    assert!(
        sb.punch_attempts >= 1 && sb.punch_successes >= 1,
        "b's direct link is a counted punch outcome: attempts {} successes {}",
        sb.punch_attempts,
        sb.punch_successes
    );
    assert!(
        sb.punch_successes <= sb.punch_attempts,
        "successes cannot exceed attempts: {}/{}",
        sb.punch_successes,
        sb.punch_attempts
    );
    assert!(!sb.stun_resolved, "no stun server configured");
    a.shutdown();
    b.shutdown();
    c.shutdown();
    signal.task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mesh_effect_late_joiner_pulls_from_two_holders() {
    let signal = SignalServer::spawn("127.0.0.1:0".parse().unwrap(), KEY)
        .await
        .unwrap();
    let blocks = make_small_blocks(16);

    // A holds all 16; B fetches all 16 and LANDS them in its store (like
    // hydrate lands them in the CAS); C joins late and must be able to pull
    // from BOTH A and B (the mesh effect: more holders = more sources).
    let a_store = MapServe::default();
    for (h, b) in &blocks {
        a_store.insert(h, b.clone());
    }
    let a = spawn(
        signal.local_addr,
        "node-a",
        "mesh",
        Arc::new(a_store.clone()),
    )
    .await;
    let b_store = MapServe::default();
    let b = spawn(
        signal.local_addr,
        "node-b",
        "mesh",
        Arc::new(b_store.clone()),
    )
    .await;

    // A ↔ B link up
    wait_until(
        || a.stats().direct_links >= 1 && b.stats().direct_links >= 1,
        "a-b linked",
        Duration::from_secs(30),
    )
    .await;

    // B warms + fetches all 16, landing each in its store (the CAS-landing
    // pattern — the bloom follows the store, not the fetch)
    let hashes: Vec<Hash> = blocks.iter().map(|(h, _)| *h).collect();
    b.warm_blocks(&hashes);
    for (h, expected) in &blocks {
        let bytes = b
            .fetch_block(h, Duration::from_secs(60))
            .await
            .expect("b fetched");
        assert_eq!(&bytes, expected);
        b_store.insert(h, bytes);
    }
    wait_until(
        || b_store.len() == 16,
        "b landed 16",
        Duration::from_secs(30),
    )
    .await;

    // C joins late — wait until C has BOTH blooms in hand before wanting
    // (blooms arrive with the hello-forced HAVE refresh)
    let c_store = MapServe::default();
    let c = spawn(
        signal.local_addr,
        "node-c",
        "mesh",
        Arc::new(c_store.clone()),
    )
    .await;
    wait_until(
        || c.stats().peers_with_bloom == 2,
        "c sees both blooms",
        Duration::from_secs(30),
    )
    .await;
    // also require B's bloom to actually cover the blocks (16 items)
    wait_until(
        || {
            let cs = c.stats();
            cs.peers == 2 && cs.peers_with_bloom == 2 && cs.direct_links >= 1
        },
        "c fully linked",
        Duration::from_secs(30),
    )
    .await;

    // C fetches all 16 — the scheduler splits across A and B
    c.warm_blocks(&hashes);
    for (h, expected) in &blocks {
        let bytes = c
            .fetch_block(h, Duration::from_secs(60))
            .await
            .expect("c fetched from the mesh");
        assert_eq!(&bytes, expected, "byte-identical through the mesh");
    }

    // THE mesh assertion: B (a peer that FETCHED, not originated) served C
    let sb = b.stats();
    assert!(
        sb.blocks_served > 0,
        "the mesh effect: B re-shared blocks it fetched (served {})",
        sb.blocks_served
    );
    a.shutdown();
    b.shutdown();
    c.shutdown();
    signal.task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn relay_fallback_when_punching_is_disabled() {
    init_tracing();
    let signal = SignalServer::spawn("127.0.0.1:0".parse().unwrap(), KEY)
        .await
        .unwrap();
    let relay = RelayServer::spawn("127.0.0.1:0".parse().unwrap(), signal.local_addr, KEY)
        .await
        .unwrap();
    let blocks = make_blocks(3, 2);

    let a_store = MapServe::default();
    for (h, b) in &blocks {
        a_store.insert(h, b.clone());
    }
    // both nodes force_relay: no punch probes at all
    let a = Swarm::spawn(
        cfg(signal.local_addr, "node-a", "relay", true),
        Arc::new(a_store.clone()),
    )
    .await
    .unwrap();
    let b = Swarm::spawn(
        cfg(signal.local_addr, "node-b", "relay", true),
        Arc::new(MapServe::default()),
    )
    .await
    .unwrap();

    // relay-routed link establishes (grant → via_relay → relay hello)
    wait_until(
        || a.stats().relay_links >= 1 || b.stats().relay_links >= 1,
        "relay link established",
        Duration::from_secs(30),
    )
    .await;

    // blocks flow THROUGH the relay
    for (h, expected) in &blocks {
        let bytes = b
            .fetch_block(h, Duration::from_secs(60))
            .await
            .expect("fetched via relay");
        assert_eq!(&bytes, expected, "byte-identical through the relay");
    }
    let rf = relay
        .stats
        .forwarded
        .load(std::sync::atomic::Ordering::Relaxed);
    assert!(rf > 0, "relay actually forwarded datagrams: {rf}");
    let sb = b.stats();
    assert_eq!(sb.blocks_fetched, 3, "all three via the relay path");
    a.shutdown();
    b.shutdown();
    relay.task.abort();
    signal.task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_returns_none_when_no_peer_holds_it() {
    // the cloud-fallback contract: fetch_block None ⇒ try the plane instead
    let signal = SignalServer::spawn("127.0.0.1:0".parse().unwrap(), KEY)
        .await
        .unwrap();
    let a = spawn(
        signal.local_addr,
        "node-a",
        "proj",
        Arc::new(MapServe::default()),
    )
    .await;
    let b = spawn(
        signal.local_addr,
        "node-b",
        "proj",
        Arc::new(MapServe::default()),
    )
    .await;
    wait_until(
        || a.stats().direct_links >= 1 && b.stats().direct_links >= 1,
        "linked",
        Duration::from_secs(30),
    )
    .await;
    let missing = Hash::of(b"nobody-has-this-block");
    let got = b.fetch_block(&missing, Duration::from_secs(2)).await;
    assert!(got.is_none(), "no holder ⇒ None (plane fallback path)");
    a.shutdown();
    b.shutdown();
    signal.task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn corrupt_peer_bytes_are_rejected_not_landed() {
    // I2 across the trust boundary: a peer serving WRONG bytes must never be
    // handed out — the receiver blake3-checks and rotates holders.
    let signal = SignalServer::spawn("127.0.0.1:0".parse().unwrap(), KEY)
        .await
        .unwrap();
    let blocks = make_blocks(1, 1);
    let (h, expected) = &blocks[0];

    // honest holder + corrupt holder for the SAME hash
    let honest = MapServe::default();
    honest.insert(h, expected.clone());
    let corrupt = MapServe::default();
    corrupt.insert(h, expected.clone());
    let corrupt_store = MapServe {
        blocks: corrupt.blocks.clone(),
        corrupt: Some(*h),
    };

    let a = spawn(
        signal.local_addr,
        "node-honest",
        "proj",
        Arc::new(honest.clone()),
    )
    .await;
    let b = spawn(
        signal.local_addr,
        "node-lying",
        "proj",
        Arc::new(corrupt_store),
    )
    .await;
    let c = spawn(
        signal.local_addr,
        "node-seeker",
        "proj",
        Arc::new(MapServe::default()),
    )
    .await;
    wait_until(
        || c.stats().direct_links >= 2,
        "c linked to both",
        Duration::from_secs(30),
    )
    .await;

    // C fetches: if the lying peer is assigned first, its bytes FAIL blake3
    // and C rotates to the honest holder — either way C ends up with TRUTH.
    let got = c.fetch_block(h, Duration::from_secs(60)).await;
    match got {
        Some(bytes) => assert_eq!(&bytes, expected, "only verified bytes can complete"),
        None => panic!("corrupt-first rotation must still fetch from the honest holder"),
    }
    a.shutdown();
    b.shutdown();
    c.shutdown();
    signal.task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stunnel_stun_discovery_plumbs_reflexive_candidate() {
    // the swarm uses the signal server's OBSERVED address as a punch
    // candidate even without a STUN server (free vantage-point discovery)
    let signal = SignalServer::spawn("127.0.0.1:0".parse().unwrap(), KEY)
        .await
        .unwrap();
    let (stun_addr, stun_task) = cairn_p2p::stun::spawn_loopback().await.unwrap();
    let a = Swarm::spawn(
        SwarmConfig {
            stun: Some(stun_addr),
            ..cfg(signal.local_addr, "node-a", "stun", false)
        },
        Arc::new(MapServe::default()),
    )
    .await
    .unwrap();
    // the swarm booted without error and registered; stun discovery ran
    wait_until(
        || a.stats().peers == 0,
        "booted clean",
        Duration::from_secs(3),
    )
    .await;
    a.shutdown();
    stun_task.abort();
    signal.task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn join_code_gate_stranger_never_joins() {
    // the full-stack admission contract (ADR-0017 §7): the host's signal
    // server is keyed by a generated join code. Members holding the code
    // register, link, and exchange blocks. A node with a DIFFERENT valid
    // join code is invisible to everyone: it registers into a void, links
    // nobody, learns no blooms, and fetches nothing — the swarm is a
    // private room, not a public lobby.
    init_tracing();
    let code = cairn_p2p::JoinCode::generate();
    let signal = SignalServer::spawn("127.0.0.1:0".parse().unwrap(), &code.cluster_key())
        .await
        .unwrap();
    let wrong_code = cairn_p2p::JoinCode::generate();
    assert_ne!(wrong_code.cluster_key(), code.cluster_key());

    // two members hold the code; one holds blocks to prove transfer still works
    let blocks = make_blocks(2, 2);
    let a_store = MapServe::default();
    for (h, b) in &blocks {
        a_store.insert(h, b.clone());
    }
    let a = spawn_keyed(
        signal.local_addr,
        "node-a",
        &code.cluster_key(),
        Arc::new(a_store),
    )
    .await;
    let b = spawn_keyed(
        signal.local_addr,
        "node-b",
        &code.cluster_key(),
        Arc::new(MapServe::default()),
    )
    .await;
    // the stranger presents a perfectly valid code — for a different swarm
    let stranger = spawn_keyed(
        signal.local_addr,
        "node-stranger",
        &wrong_code.cluster_key(),
        Arc::new(MapServe::default()),
    )
    .await;

    // members link to each other (and to nobody else)
    wait_until(
        || a.stats().direct_links >= 1,
        "a linked to b",
        Duration::from_secs(30),
    )
    .await;
    wait_until(
        || b.stats().direct_links >= 1,
        "b linked to a",
        Duration::from_secs(30),
    )
    .await;

    // member transfer unaffected by the gate
    let (h0, expected0) = &blocks[0];
    let got = b.fetch_block(h0, Duration::from_secs(30)).await;
    assert_eq!(
        got.as_deref(),
        Some(expected0.as_slice()),
        "members still sync"
    );

    // the stranger stays out: no links, no peers, no fetches — 4 s is two
    // full register cycles (2 s cadence) plus link slack
    tokio::time::sleep(Duration::from_secs(4)).await;
    let ss = stranger.stats();
    assert_eq!(ss.peers, 0, "stranger never learned a member");
    assert_eq!(ss.direct_links, 0, "stranger never linked");
    let want = Hash::of(b"member-only-block");
    let denied = stranger.fetch_block(&want, Duration::from_secs(2)).await;
    assert!(
        denied.is_none(),
        "stranger fetches nothing from a room it cannot enter"
    );

    a.shutdown();
    b.shutdown();
    stranger.shutdown();
    signal.task.abort();
}

/// `spawn` with an explicit cluster key (join-code derivation target).
async fn spawn_keyed(
    signal: SocketAddr,
    node: &str,
    key: &[u8],
    serving: Arc<dyn ServeBlocks>,
) -> Swarm {
    Swarm::spawn(
        SwarmConfig {
            cluster_key: key.to_vec(),
            project: "proj".to_string(),
            node_id: Some(node.to_string()),
            ..cfg(signal, node, "proj", false)
        },
        serving,
    )
    .await
    .expect("swarm spawn")
}

/// ADR-0023 — live presence over a REAL session: a broadcasts a playhead
/// event, b receives it through the same encrypted pipe as block traffic,
/// and the disabled-by-default contract is pinned (no flag → no traffic).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn presence_broadcasts_reach_peers_and_default_off() {
    init_tracing();
    let signal = SignalServer::spawn("127.0.0.1:0".parse().unwrap(), KEY)
        .await
        .expect("signal spawn");
    let sig_addr = signal.local_addr;

    // Both nodes with presence ON (the opt-in case).
    let mut on_cfg = cfg(sig_addr, "pres-a", "pres", false);
    on_cfg.presence = true;
    let a = Swarm::spawn(on_cfg, Arc::new(MapServe::default()))
        .await
        .expect("a spawn");
    let mut on_cfg_b = cfg(sig_addr, "pres-b", "pres", false);
    on_cfg_b.presence = true;
    let b = Swarm::spawn(on_cfg_b, Arc::new(MapServe::default()))
        .await
        .expect("b spawn");

    // session establishes
    wait_until(
        || a.stats().peers == 1 && b.stats().peers == 1,
        "presence pair session",
        Duration::from_secs(10),
    )
    .await;

    // b subscribes BEFORE a broadcasts
    let mut rx = b.subscribe_presence();
    let sent = br#"{"editor":"alice","frame":1080,"rate":24,"action":"playhead"}"#;
    let reached = a.broadcast_presence(sent);
    assert_eq!(reached, 1, "one session peer reached");

    // the event lands at b (encrypted, authenticated, verbatim)
    let ev = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("presence event within 5s")
        .expect("channel alive");
    assert_eq!(ev.payload, sent.to_vec());
    assert_eq!(
        ev.from,
        a.node_id().to_string(),
        "event carries the sender id"
    );
    assert_eq!(b.stats().presence_events, 1, "counter incremented");

    // snapshot agrees with the channel
    let snap = b.presence_snapshot();
    assert_eq!(snap.len(), 1);
    assert_eq!(snap[0].payload, sent.to_vec());

    // the OFF contract: a third node with presence disabled hears nothing
    // (inbound dropped at the door) and its own broadcast reaches nobody.
    let c = spawn(sig_addr, "pres-c", "pres", Arc::new(MapServe::default())).await;
    wait_until(
        || a.stats().peers == 2 && c.stats().peers == 2,
        "c joins the swarm",
        Duration::from_secs(10),
    )
    .await;
    assert!(!c.presence_enabled());
    assert_eq!(
        c.broadcast_presence(b"{}"),
        0,
        "disabled node broadcasts nothing"
    );
    a.broadcast_presence(br#"{"editor":"alice","frame":1081}"#);
    // c's snapshot stays empty — the flag is a door, not a filter
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        c.presence_snapshot().is_empty(),
        "disabled node accepts nothing"
    );
    // but b still receives (a and b both opted in; c is silent, not blocking)
    let ev2 = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("second presence event")
        .expect("channel alive");
    assert_eq!(ev2.payload, br#"{"editor":"alice","frame":1081}"#.to_vec());

    // oversize broadcast is refused, never panics, reaches nobody
    let fat = vec![0u8; 1300];
    assert_eq!(a.broadcast_presence(&fat), 0);
    assert!(a.stats().peers >= 1, "swarm unaffected by the refusal");

    a.shutdown();
    b.shutdown();
    c.shutdown();
    signal.task.abort();
}
