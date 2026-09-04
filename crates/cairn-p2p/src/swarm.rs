//! Swarm orchestrator (ADR-0017 §6) — the mesh.
//!
//! One UDP socket per node runs the whole state machine:
//!
//! - **Membership** — re-register with the signal server every 2 s; each
//!   REGISTERED reply refreshes the peer table (ids, keys, candidate
//!   addresses, relay hints).
//! - **Punching** — on discovering a peer, the FIRST probe fires immediately
//!   (not on the 250 ms tick); every tick rotates through candidate
//!   addresses. After [`PUNCH_DEADLINE`] (or immediately under
//!   `force_relay`), the lexicographically-LOWER node id requests a relay
//!   grant (one requester per pair — mismatched requests were a real bug),
//!   and both sides fall back to relay-routed frames.
//! - **HAVE exchange** — each node advertises a Bloom of owned chunk hashes.
//!   Refresh is change-driven (owned-count delta) + forced on HELLO and on
//!   block completion; sends are rate-limited to 100 ms per peer. This trio
//!   is what keeps LATE JOINERS fed — a stale bloom is a dead mesh.
//! - **Want scheduling** — wants go to holders whose bloom says "maybe",
//!   fewest-outstanding-first with round-robin tie-break; 8 s per attempt,
//!   750 ms re-request (covers dropped WANT/META), NAK on stall, DENY
//!   rotates to the next holder. A failed attempt never exhausts the want:
//!   only all-holders-tried + a 20 s orphan grace does.
//! - **Serving** — paced fragment streaming (yield + micro-sleeps) so a burst
//!   cannot overflow the receiver's UDP buffer; NAK retransmits re-read the
//!   block from the local store; DENY when we simply do not have it.
//! - **Verification** — every completed block is BLAKE3-checked (I2) before
//!   it can be claimed. A corrupting peer is rotated off for that hash.
//!
//! The mesh effect: completion re-announces to every peer (HAVE), so with
//! N holders the download of a wanted block spreads instead of funneling
//! through the original uploader.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use cairn_core::bloom::Bloom;
use cairn_core::hash::Hash;
use tokio::net::UdpSocket;
use tokio::sync::{oneshot, watch};

use crate::crypto::{derive_session, NodeKey};
use crate::relay::{build_routing_header, parse_routing_header, RELAY_MAGIC};
use crate::session::{
    build_clear_hello, parse_clear_hello, PeerMsg, PeerSession, Reassembly, FRAME_CLEAR_HELLO,
    FRAME_ENC, MAX_DATAGRAM, MAX_FRAG_DATA,
};
use crate::signal::{SignalClient, FLAG_IS_RELAY, TAG_REGISTERED, TAG_RELAY_GRANT};
use crate::stun;

const STUN_MAGIC: [u8; 4] = [0x21, 0x12, 0xA4, 0x42];

// ---- tunables ---------------------------------------------------------------
const REGISTER_EVERY: Duration = Duration::from_millis(2000);
const PUNCH_EVERY: Duration = Duration::from_millis(250);
const PUNCH_DEADLINE: Duration = Duration::from_millis(2000);
const HAVE_EVERY: Duration = Duration::from_millis(100);
const WANT_EVERY: Duration = Duration::from_millis(100);
const WANT_RE_REQUEST: Duration = Duration::from_millis(750);
const WANT_TIMEOUT: Duration = Duration::from_millis(8000);
const ORPHAN_GRACE: Duration = Duration::from_secs(20);
const STREAM_STALL: Duration = Duration::from_millis(300);
const HELLO_REPLY_GATE: Duration = Duration::from_millis(250);
const SERVE_IDLE_PRUNE: Duration = Duration::from_secs(8);
const PEER_STALE: Duration = Duration::from_secs(8);
/// Cap on concurrent wants assigned to one peer (memory + fairness bound).
const PER_PEER_ACTIVE_WANTS: u32 = 8;
/// Completed-but-unclaimed blocks retained for later fetch_block calls.
const COMPLETED_KEEP: usize = 64;
/// Bloom advertisement cap: over this, the bloom covers the first CAP sorted
/// hashes and receivers treat "absent" as "unknown" (want candidates stay in).
const BLOOM_CAP: u64 = 40_000;

// ---- public surface ----------------------------------------------------------

/// Swarm configuration.
pub struct SwarmConfig {
    /// Local UDP bind (default `0.0.0.0:0`).
    pub bind: SocketAddr,
    /// Signal server address.
    pub signal: SocketAddr,
    /// Cluster key (HMAC) — the same secret the signal server checks.
    pub cluster_key: Vec<u8>,
    /// Swarm scope (project id).
    pub project: String,
    /// Stable node id (default: generated).
    pub node_id: Option<String>,
    /// STUN server for reflexive-address discovery (optional).
    pub stun: Option<SocketAddr>,
    /// Skip punching entirely — always route via the relay (tests/strict NATs).
    pub force_relay: bool,
    /// Live presence telemetry (ADR-0023 §2): broadcast/accept ephemeral
    /// presence events on the existing encrypted sessions. **Default-able to
    /// false everywhere — presence is OFF unless an editor turns it on for
    /// THEIR device.** When false: no broadcasts, inbound Presence messages
    /// are dropped, no subscriber channels exist.
    pub presence: bool,
}

/// The local node's block-store view (implemented over the Cas by callers).
pub trait ServeBlocks: Send + Sync {
    /// Raw bytes of a locally-owned block.
    fn block_bytes(&self, h: &Hash) -> Option<Vec<u8>>;
    /// Hashes of all locally-owned blocks (builds the HAVE bloom).
    fn owned_hashes(&self) -> Vec<Hash>;
    /// Fast owned-count (default walks [`ServeBlocks::owned_hashes`] —
    /// implementors should override with a cheap COUNT when they can).
    fn owned_count(&self) -> u64 {
        self.owned_hashes().len() as u64
    }
}

/// A swarm stats snapshot (dashboard/test surface).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SwarmStats {
    pub node_id: String,
    pub peers: usize,
    pub peers_with_bloom: usize,
    pub direct_links: usize,
    pub relay_links: usize,
    pub wants_pending: usize,
    pub blocks_served: u64,
    pub bytes_served: u64,
    pub blocks_fetched: u64,
    pub bytes_fetched: u64,
    pub local_owned: u64,
    /// NAT observability, pair-level accounting (the WAN leg's success-rate
    /// numerator/denominator): did STUN hand us a reflexive address, how
    /// many peer pairs engaged in punching (our first probe, or the peer's
    /// probe landing on us), and how many of those pairs ended up with a
    /// DIRECT session — the rest fell back to the relay, and that ratio is
    /// the NAT success rate. `punch_successes <= punch_attempts` on every
    /// node by construction.
    pub stun_resolved: bool,
    pub punch_attempts: u64,
    pub punch_successes: u64,
    /// Inbound live-presence events accepted since spawn (0 when the
    /// presence flag is off — the flag is observable, not just behavioral).
    pub presence_events: u64,
}

/// One inbound live-presence event (ADR-0023 §2). `payload` is the peer's
/// app JSON, verbatim; interpreting it is the daemon's job (the swarm is a
/// transport, not a schema).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PresenceEvent {
    /// Sending node id (the device id string).
    pub from: String,
    /// App payload bytes (bounded by the session codec to 1200).
    pub payload: Vec<u8>,
    /// Reception time (ms since swarm spawn).
    pub at_ms: u64,
}

/// Presence entries older than this leave the snapshot (the heartbeat cadence
/// is ~0.5–2 s; 15 s means ~10 missed beats before an editor "disappears").
const PRESENCE_TTL: Duration = Duration::from_secs(15);
/// Presence broadcast channel capacity — subscribers that fall further
/// behind get `Lagged` and resnapshot (presence is a signal, not a log).
const PRESENCE_CHAN: usize = 256;

// ---- internal state -----------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Link {
    Punching { since: Instant },
    Direct,
    Relay,
}

struct Peer {
    pubkey: Option<[u8; 32]>,
    addrs: Vec<SocketAddr>,
    direct_addr: Option<SocketAddr>,
    session: Option<PeerSession>,
    bloom: Option<Bloom>,
    bloom_items: u32,
    bloom_partial: bool,
    bloom_pending: bool,
    bloom_last_sent: Instant,
    want_out: u32,
    link: Link,
    probe_idx: u32,
    last_hello_reply: Instant,
    last_signal_seen: Instant,
    relay_requested: bool,
    tried: HashSet<[u8; 32]>,
    nak_last: Instant,
}

impl Peer {
    fn new() -> Self {
        Peer {
            pubkey: None,
            addrs: Vec::new(),
            direct_addr: None,
            session: None,
            bloom: None,
            bloom_items: 0,
            bloom_partial: false,
            bloom_pending: false,
            bloom_last_sent: Instant::now(),
            want_out: 0,
            link: Link::Punching {
                since: Instant::now(),
            },
            probe_idx: 0,
            last_hello_reply: Instant::now().checked_sub(HELLO_REPLY_GATE).unwrap(),
            last_signal_seen: Instant::now(),
            relay_requested: false,
            tried: HashSet::new(),
            nak_last: Instant::now(),
        }
    }
}

#[derive(Debug)]
enum Phase {
    New,
    Requested {
        since: Instant,
        last_request: Instant,
    },
    Streaming {
        since: Instant,
    },
}

struct Want {
    assigned: Option<Vec<u8>>,
    tried: HashSet<Vec<u8>>,
    phase: Phase,
    reassembly: Option<Reassembly>,
    first: Instant,
}

struct ServeRecord {
    last: Instant,
}

#[derive(Default)]
struct Stats {
    blocks_served: AtomicU64,
    bytes_served: AtomicU64,
    blocks_fetched: AtomicU64,
    bytes_fetched: AtomicU64,
    stun_resolved: AtomicBool,
    punch_attempts: AtomicU64,
    punch_successes: AtomicU64,
    presence_events: AtomicU64,
}

/// Hashes currently being waited on by `fetch_block` callers (the wakeup
/// list per completed block).
type BlockWaiters = HashMap<[u8; 32], Vec<oneshot::Sender<Option<Vec<u8>>>>>;

#[derive(Default)]
struct State {
    peers: HashMap<Vec<u8>, Peer>,
    wants: HashMap<[u8; 32], Want>,
    completed: Vec<([u8; 32], Vec<u8>)>,
    waiters: BlockWaiters,
    serve: HashMap<(Vec<u8>, [u8; 32]), ServeRecord>,
    relay_addr: Option<SocketAddr>,
    /// the address the SIGNAL SERVER observed for us (a reflexive candidate
    /// from the server's vantage — free NAT discovery even without STUN)
    last_observed: Option<SocketAddr>,
    /// consecutive signal-registration failures. 3 in a row (~6 s) is almost
    /// certainly not a transient network blip — surface a loud hint covering
    /// the two real causes: signal unreachable, or the join code is wrong.
    register_fails: u32,
    /// Latest presence payload per peer (ADR-0023) — the LAST event wins,
    /// stale entries pruned by [`presence_snapshot`] and the periodic pass.
    presence: HashMap<Vec<u8>, (Instant, Vec<u8>)>,
}

impl State {
    fn claim_completed(&mut self, hash: &[u8; 32]) -> Option<Vec<u8>> {
        if let Some(pos) = self.completed.iter().position(|(h, _)| h == hash) {
            Some(self.completed.remove(pos).1)
        } else {
            None
        }
    }
    fn retain_completed(&mut self) {
        while self.completed.len() > COMPLETED_KEEP {
            self.completed.remove(0);
        }
    }
}

struct Inner {
    node: NodeKey,
    sock: Arc<UdpSocket>,
    project: String,
    force_relay: bool,
    serving: Arc<dyn ServeBlocks>,
    state: StdMutex<State>,
    signal_client: SignalClient,
    stun_waiters: StdMutex<HashMap<[u8; 12], oneshot::Sender<Vec<u8>>>>,
    /// cached serialized HAVE bloom — rebuilt only when the owned count moves
    /// (a full owned_hashes() walk per HAVE send would hammer the CAS).
    bloom_cache: StdMutex<Option<(u64, u32, Vec<u8>)>>, // (count, items, bytes)
    assign_rr: AtomicU64,
    stats: Stats,
    done: watch::Sender<bool>,
    /// ADR-0023: presence fanout. The sender half lives here even when
    /// disabled — a channel with zero subscribers and zero sends costs
    /// nothing; the flag gates every send/accept path.
    presence_tx: tokio::sync::broadcast::Sender<PresenceEvent>,
    presence_enabled: bool,
    started: Instant,
}

/// A running swarm node. Clone-safe handle; [`Swarm::shutdown`] stops the loops.
#[derive(Clone)]
pub struct Swarm {
    inner: Arc<Inner>,
    task: Arc<StdMutex<Option<tokio::task::JoinHandle<()>>>>,
}

impl Swarm {
    /// Spawn the node: bind the socket, discover the reflexive address via
    /// STUN (if configured), then run the register/punch/have/want loops.
    pub async fn spawn(cfg: SwarmConfig, serving: Arc<dyn ServeBlocks>) -> std::io::Result<Swarm> {
        // bind via a std socket so we can request a big receive buffer
        // before handing it to tokio (tokio's wrapper has no buffer-sizing
        // API; std's is also missing on this toolchain — pacing + NAK
        // recovery carry the burst-safety instead)
        let std_sock = std::net::UdpSocket::bind(cfg.bind)?;
        std_sock.set_nonblocking(true)?;
        let sock = Arc::new(UdpSocket::from_std(std_sock)?);

        let node = match cfg.node_id {
            Some(id) => NodeKey::generate(&id),
            None => NodeKey::generate(&format!("n-{}", cairn_core::ids::new_device_id())),
        };
        let signal_client = SignalClient::new(Arc::clone(&sock), cfg.signal, &cfg.cluster_key);

        let inner = Arc::new(Inner {
            node,
            sock,
            project: cfg.project,
            force_relay: cfg.force_relay,
            serving,
            state: StdMutex::new(State::default()),
            signal_client,
            stun_waiters: StdMutex::new(HashMap::new()),
            bloom_cache: StdMutex::new(None),
            assign_rr: AtomicU64::new(0),
            stats: Stats::default(),
            done: watch::channel(false).0,
            presence_tx: tokio::sync::broadcast::channel(PRESENCE_CHAN).0,
            presence_enabled: cfg.presence,
            started: Instant::now(),
        });

        if let Some(server) = cfg.stun {
            // spawned (not inline): stun_discover awaits a reply that only the
            // main loop can read — awaiting it here would self-deadlock
            let inner2 = Arc::clone(&inner);
            tokio::spawn(async move {
                match stun_discover(&inner2, server).await {
                    Ok(addr) => {
                        inner2.stats.stun_resolved.store(true, Ordering::Relaxed);
                        tracing::info!(reflexive = %addr, "stun: reflexive address");
                    }
                    Err(e) => tracing::warn!("stun discovery failed ({e}); advertising local only"),
                }
            });
        }

        let inner_loop = Arc::clone(&inner);
        let mut done_rx = inner.done.subscribe();
        let task = tokio::spawn(async move {
            // first periodic tick delayed one period: the inline register kick
            // below already covers t=0 (two concurrent roundtrips would race
            // their reply waiters — benign, but pointless)
            let mut register = tokio::time::interval_at(
                tokio::time::Instant::now() + REGISTER_EVERY,
                REGISTER_EVERY,
            );
            register.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut punch = tokio::time::interval(PUNCH_EVERY);
            let mut have = tokio::time::interval(HAVE_EVERY);
            let mut want = tokio::time::interval(WANT_EVERY);
            loop {
                let mut buf = vec![0u8; MAX_DATAGRAM];
                tokio::select! {
                    _ = done_rx.changed() => return,
                    r = inner_loop.sock.recv_from(&mut buf) => {
                        let Ok((n, from)) = r else { return };
                        dispatch(&inner_loop, &buf[..n], from);
                    }
                    // register passes run as TASKS: they await replies that
                    // only this loop can read — awaiting inline would deadlock
                    _ = register.tick() => {
                        let inner2 = Arc::clone(&inner_loop);
                        tokio::spawn(async move { register_pass(&inner2).await; });
                    }
                    _ = punch.tick() => punch_pass(&inner_loop).await,
                    _ = have.tick() => have_pass(&inner_loop),
                    _ = want.tick() => want_pass(&inner_loop),
                }
            }
        });

        // kick membership immediately (no first-tick wait). Safe inline: the
        // main loop above is already running to read the reply.
        register_pass(&inner).await;

        Ok(Swarm {
            inner,
            task: Arc::new(StdMutex::new(Some(task))),
        })
    }

    #[must_use]
    pub fn node_id(&self) -> &str {
        self.inner.node.node_id()
    }

    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.inner
            .sock
            .local_addr()
            .unwrap_or_else(|_| "0.0.0.0:0".parse().expect("static"))
    }

    /// Fast local check: does any session peer's bloom claim (maybe) this hash?
    /// Gates the peer path in hydration — no waiting, no traffic.
    #[must_use]
    pub fn may_have(&self, h: &Hash) -> bool {
        let st = self.inner.state.lock().expect("swarm state lock");
        st.peers.values().any(|p| {
            p.session.is_some()
                && match (&p.bloom, p.bloom_partial) {
                    (Some(b), false) => b.might_contain(h.hex().as_bytes()),
                    (Some(_), true) => true, // partial bloom: unknown ⇒ maybe
                    (None, _) => false,      // no bloom: definitely unknown to us
                }
        })
    }

    /// Register background wants (the hydrate warm pre-walk). Synchronous:
    /// the trait adapter keeps an async surface for callers, but this side
    /// only takes the state lock — nothing to await.
    pub fn warm_blocks(&self, hashes: &[Hash]) {
        let mut st = self.inner.state.lock().expect("swarm state lock");
        for h in hashes {
            st.wants.entry(h.0).or_insert_with(|| Want {
                assigned: None,
                tried: HashSet::new(),
                phase: Phase::New,
                reassembly: None,
                first: Instant::now(),
            });
        }
    }

    /// Fetch one block: instant when already completed; otherwise waits for
    /// the swarm up to `timeout`. `None` = "not available from peers" — the
    /// caller falls back to the cloud plane.
    pub async fn fetch_block(&self, h: &Hash, timeout: Duration) -> Option<Vec<u8>> {
        {
            let mut st = self.inner.state.lock().expect("swarm state lock");
            if let Some(bytes) = st.claim_completed(&h.0) {
                self.inner
                    .stats
                    .bytes_fetched
                    .fetch_add(bytes.len() as u64, Ordering::Relaxed);
                self.inner
                    .stats
                    .blocks_fetched
                    .fetch_add(1, Ordering::Relaxed);
                return Some(bytes);
            }
            st.wants.entry(h.0).or_insert_with(|| Want {
                assigned: None,
                tried: HashSet::new(),
                phase: Phase::New,
                reassembly: None,
                first: Instant::now(),
            });
        }
        let (tx, rx) = oneshot::channel();
        {
            let mut st = self.inner.state.lock().expect("swarm state lock");
            st.waiters.entry(h.0).or_default().push(tx);
        }
        let claimed = tokio::time::timeout(timeout, rx).await;
        match claimed {
            Ok(Ok(Some(bytes))) => {
                self.inner
                    .stats
                    .bytes_fetched
                    .fetch_add(bytes.len() as u64, Ordering::Relaxed);
                self.inner
                    .stats
                    .blocks_fetched
                    .fetch_add(1, Ordering::Relaxed);
                Some(bytes)
            }
            _ => {
                // timeout / None — drop our waiter slot only
                let mut st = self.inner.state.lock().expect("swarm state lock");
                if let Some(v) = st.waiters.get_mut(&h.0) {
                    v.clear();
                    if v.is_empty() {
                        st.waiters.remove(&h.0);
                    }
                }
                None
            }
        }
    }

    /// Snapshot for dashboards + tests.
    #[must_use]
    pub fn stats(&self) -> SwarmStats {
        let st = self.inner.state.lock().expect("swarm state lock");
        let mut peers_with_bloom = 0;
        let mut direct = 0;
        let mut relay = 0;
        for p in st.peers.values() {
            if p.bloom.is_some() {
                peers_with_bloom += 1;
            }
            match p.link {
                Link::Direct => direct += 1,
                Link::Relay => relay += 1,
                Link::Punching { .. } => {}
            }
        }
        SwarmStats {
            node_id: self.inner.node.node_id().to_string(),
            peers: st.peers.len(),
            peers_with_bloom,
            direct_links: direct,
            relay_links: relay,
            wants_pending: st.wants.len(),
            blocks_served: self.inner.stats.blocks_served.load(Ordering::Relaxed),
            bytes_served: self.inner.stats.bytes_served.load(Ordering::Relaxed),
            blocks_fetched: self.inner.stats.blocks_fetched.load(Ordering::Relaxed),
            bytes_fetched: self.inner.stats.bytes_fetched.load(Ordering::Relaxed),
            local_owned: self.inner.serving.owned_count(),
            stun_resolved: self.inner.stats.stun_resolved.load(Ordering::Relaxed),
            punch_attempts: self.inner.stats.punch_attempts.load(Ordering::Relaxed),
            punch_successes: self.inner.stats.punch_successes.load(Ordering::Relaxed),
            presence_events: self.inner.stats.presence_events.load(Ordering::Relaxed),
        }
    }

    // ---- Live presence (ADR-0023 §2) --------------------------------------

    /// Broadcast one presence event (app JSON, ≤ 1200 bytes) to every
    /// session peer — direct or relay, same encrypted pipe as blocks.
    /// Returns the peer count reached; `0` when presence is disabled or the
    /// payload exceeds the bound (never a panic, never a partial frame).
    pub fn broadcast_presence(&self, payload: &[u8]) -> usize {
        if !self.inner.presence_enabled || payload.len() > crate::session::MAX_FRAG_DATA {
            return 0;
        }
        let targets: Vec<Vec<u8>> = {
            let st = self.inner.state.lock().expect("swarm state lock");
            st.peers
                .iter()
                .filter(|(_, p)| p.session.is_some())
                .map(|(id, _)| id.clone())
                .collect()
        };
        let msg = PeerMsg::Presence {
            payload: payload.to_vec(),
        };
        for id in &targets {
            send_msg(self.inner_ref(), id, &msg);
        }
        targets.len()
    }

    /// Subscribe to inbound presence events. Lagging subscribers get
    /// `Lagged` and should call [`Swarm::presence_snapshot`] to resync.
    /// When presence is disabled the channel simply never fires.
    #[must_use]
    pub fn subscribe_presence(&self) -> tokio::sync::broadcast::Receiver<PresenceEvent> {
        self.inner.presence_tx.subscribe()
    }

    /// Recent presence per peer (last event wins, entries older than
    /// [`PRESENCE_TTL`] pruned). Sorted by peer id for deterministic output.
    #[must_use]
    pub fn presence_snapshot(&self) -> Vec<PresenceEvent> {
        let mut st = self.inner.state.lock().expect("swarm state lock");
        st.presence.retain(|_, (at, _)| at.elapsed() < PRESENCE_TTL);
        let at_ms = self.inner.started.elapsed().as_millis() as u64;
        let mut out: Vec<PresenceEvent> = st
            .presence
            .iter()
            .map(|(id, (_, payload))| PresenceEvent {
                from: id_str(id),
                payload: payload.clone(),
                at_ms,
            })
            .collect();
        out.sort_by(|a, b| a.from.cmp(&b.from));
        out
    }

    /// Is presence enabled on this node? (The flag, observable.)
    #[must_use]
    pub fn presence_enabled(&self) -> bool {
        self.inner.presence_enabled
    }

    fn inner_ref(&self) -> &Arc<Inner> {
        &self.inner
    }

    /// Stop all loops (idempotent).
    pub fn shutdown(&self) {
        let _ = self.inner.done.send(true);
        if let Some(t) = self.task.lock().expect("swarm task slot").take() {
            t.abort();
        }
    }
}

// ---- STUN over the shared socket ---------------------------------------------

async fn stun_discover(inner: &Inner, server: SocketAddr) -> std::io::Result<SocketAddr> {
    let seed = [
        inner.node.node_id_bytes().to_vec(),
        b"-stun-".to_vec(),
        inner
            .sock
            .local_addr()
            .map(|a| a.to_string().into_bytes())
            .unwrap_or_default(),
    ]
    .concat();
    let txid = stun::fresh_txid(&seed);
    let req = stun::binding_request(&txid);
    let (tx, rx) = oneshot::channel();
    inner
        .stun_waiters
        .lock()
        .expect("stun waiters lock")
        .insert(txid, tx);
    inner.sock.send_to(&req, server).await?;
    let reply = tokio::time::timeout(Duration::from_secs(2), rx).await;
    inner
        .stun_waiters
        .lock()
        .expect("stun waiters lock")
        .remove(&txid);
    match reply {
        Ok(Ok(dgram)) => stun::parse_response(&dgram, &txid).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "stun: malformed response")
        }),
        _ => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "stun: no response",
        )),
    }
}

// ---- datagram dispatch ---------------------------------------------------------

fn dispatch(inner: &Arc<Inner>, dgram: &[u8], from: SocketAddr) {
    if dgram.is_empty() {
        return;
    }
    // STUN responses: magic cookie at bytes 4..8, header 20 bytes
    if dgram.len() >= 20 && dgram[4..8] == STUN_MAGIC {
        if let Ok(txid) = <[u8; 12]>::try_from(&dgram[8..20]) {
            let waiter = inner
                .stun_waiters
                .lock()
                .expect("stun waiters lock")
                .remove(&txid);
            if let Some(tx) = waiter {
                let _ = tx.send(dgram.to_vec());
            }
        }
        return;
    }
    match dgram[0] {
        TAG_REGISTERED | TAG_RELAY_GRANT => inner.signal_client.handle_datagram(dgram),
        RELAY_MAGIC => {
            let Some((from_id, to_id)) = parse_routing_header(dgram) else {
                return;
            };
            let prefix = 3 + from_id.len() + to_id.len();
            let Some(payload) = dgram.get(prefix..) else {
                return;
            };
            handle_peer_frame(inner, &from_id, payload, from, true);
        }
        FRAME_CLEAR_HELLO => {
            let Some((peer_id, pubkey)) = parse_clear_hello(dgram) else {
                return;
            };
            on_hello(inner, &peer_id, pubkey, from, false);
        }
        FRAME_ENC => {
            let peer_id = {
                let st = inner.state.lock().expect("swarm state lock");
                st.peers
                    .iter()
                    .find(|(_, p)| p.direct_addr == Some(from) || p.addrs.contains(&from))
                    .map(|(id, _)| id.clone())
            };
            match peer_id {
                Some(peer_id) => handle_peer_frame(inner, &peer_id, dgram, from, false),
                None => {
                    tracing::debug!(from = %from, "enc frame from unknown address dropped");
                }
            }
        }
        _ => {}
    }
}

/// A session frame (clear hello or enc) from `peer_id`, arriving directly or
/// relay-wrapped.
fn handle_peer_frame(
    inner: &Arc<Inner>,
    peer_id: &[u8],
    frame: &[u8],
    from: SocketAddr,
    via_relay: bool,
) {
    if frame.first() == Some(&FRAME_CLEAR_HELLO) {
        if let Some((pid, pubkey)) = parse_clear_hello(frame) {
            on_hello(inner, &pid, pubkey, from, via_relay);
        }
        return;
    }
    let opened = {
        let st = inner.state.lock().expect("swarm state lock");
        match st.peers.get(peer_id) {
            Some(peer) => match peer.session.as_ref() {
                Some(session) => session.open(frame),
                None => None,
            },
            None => None,
        }
    };
    match opened {
        Some(msg) => {
            if !via_relay {
                // direct receipt upgrades relay/punching links to direct
                let mut st = inner.state.lock().expect("swarm state lock");
                if let Some(peer) = st.peers.get_mut(peer_id) {
                    peer.direct_addr = Some(from);
                    peer.link = Link::Direct;
                }
            }
            handle_msg(inner, peer_id, msg);
        }
        None => tracing::debug!(
            peer = %String::from_utf8_lossy(peer_id),
            "auth-failed frame dropped"
        ),
    }
}

/// HELLO handling — session bootstrap + self-healing handshake.
///
/// The REPLY hello goes out CLEAR (not sealed): the peer may not have a
/// session for us yet, and a sealed reply would be dropped — the classic
/// punch deadlock. Both sides therefore always end up holding sessions.
fn on_hello(
    inner: &Arc<Inner>,
    peer_id: &[u8],
    pubkey: [u8; 32],
    from: SocketAddr,
    via_relay: bool,
) {
    let (known, key_mismatch, had_session) = {
        let st = inner.state.lock().expect("swarm state lock");
        match st.peers.get(peer_id) {
            None => (false, false, false),
            Some(p) => (
                true,
                p.pubkey.is_some_and(|k| k != pubkey),
                p.session.is_some(),
            ),
        }
    };
    if !known {
        // not in our signal table yet (2 s cadence): fail-closed — punching
        // retries every 250 ms and the next REGISTERED reply introduces them
        tracing::debug!(peer = %String::from_utf8_lossy(peer_id), "hello from unknown peer ignored");
        return;
    }
    if key_mismatch {
        // signal-advertised (HMAC-covered) key ≠ presented key: refuse hard
        tracing::warn!(peer = %String::from_utf8_lossy(peer_id), "hello pubkey MISMATCH — refusing session");
        return;
    }

    if !had_session {
        let keys = match derive_session(&inner.node, peer_id, &pubkey) {
            Ok(k) => k,
            Err(_) => {
                tracing::warn!("session key derivation failed");
                return;
            }
        };
        let mut st = inner.state.lock().expect("swarm state lock");
        if let Some(peer) = st.peers.get_mut(peer_id) {
            peer.pubkey = Some(pubkey);
            peer.session = Some(PeerSession::new(keys));
            peer.link = if via_relay && peer.direct_addr.is_none() {
                Link::Relay
            } else {
                Link::Direct
            };
            // NAT metrics (pair-level accounting): a session established
            // DIRECTLY is a punch that landed. If we never probed this pair
            // ourselves (probe_idx == 0), the PEER's probe landed on us —
            // count the pair's attempt too, so punch_successes can never
            // exceed punch_attempts on any node.
            if matches!(peer.link, Link::Direct) {
                if peer.probe_idx == 0 {
                    inner.stats.punch_attempts.fetch_add(1, Ordering::Relaxed);
                }
                inner.stats.punch_successes.fetch_add(1, Ordering::Relaxed);
            }
            if !via_relay {
                peer.direct_addr = Some(from);
            }
            peer.bloom_pending = true; // forced refresh at hello
            tracing::debug!(
                peer = %String::from_utf8_lossy(peer_id),
                link = ?peer.link,
                "session established"
            );
        }
    } else if !via_relay {
        let mut st = inner.state.lock().expect("swarm state lock");
        if let Some(peer) = st.peers.get_mut(peer_id) {
            peer.direct_addr = Some(from);
        }
    }

    // reply with a CLEAR hello (rate-gated), then a fresh HAVE
    let reply_now = {
        let mut st = inner.state.lock().expect("swarm state lock");
        let Some(peer) = st.peers.get_mut(peer_id) else {
            return;
        };
        if peer.last_hello_reply.elapsed() >= HELLO_REPLY_GATE {
            peer.last_hello_reply = Instant::now();
            true
        } else {
            false
        }
    };
    if reply_now {
        send_clear(inner, peer_id);
    }
    send_have(inner, peer_id, true);
}

enum EofOutcome {
    Complete,
    Gaps(Vec<u16>),
    NoMeta,
    AlreadyComplete,
}

fn handle_msg(inner: &Arc<Inner>, peer_id: &[u8], msg: PeerMsg) {
    // Collect outbound actions under the lock, send after releasing —
    // send_msg takes the state lock itself (std mutex: no nesting).
    enum Action {
        Serve([u8; 32]),
        Nak {
            peer: Vec<u8>,
            hash: [u8; 32],
            idxs: Vec<u16>,
        },
        WantAgain {
            peer: Vec<u8>,
            hash: [u8; 32],
        },
        Fail {
            hash: [u8; 32],
            peer: Vec<u8>,
        },
        Finalize([u8; 32]),
        HealHave,
    }
    let mut actions: Vec<Action> = Vec::new();

    match msg {
        PeerMsg::Hello { node_id, pubkey: _ } => {
            if node_id != peer_id {
                tracing::debug!("hello id/route mismatch dropped");
                return;
            }
            actions.push(Action::HealHave);
        }
        PeerMsg::Have { bloom, items } => {
            let parsed = Bloom::parse(&bloom);
            let mut st = inner.state.lock().expect("swarm state lock");
            if let Some(peer) = st.peers.get_mut(peer_id) {
                match parsed {
                    Some(b) => {
                        peer.bloom = Some(b);
                        peer.bloom_items = items;
                        peer.bloom_partial = items > BLOOM_CAP as u32;
                    }
                    None => {
                        peer.bloom = None;
                        peer.bloom_partial = true;
                    }
                }
            }
        }
        PeerMsg::Want { hash } => actions.push(Action::Serve(hash)),
        PeerMsg::Meta {
            hash,
            total_len,
            frags,
        } => {
            let mut st = inner.state.lock().expect("swarm state lock");
            if let Some(want) = st.wants.get_mut(&hash) {
                let consistent = want
                    .reassembly
                    .as_ref()
                    .is_some_and(|r| r.total_len == total_len && r.frags == frags);
                if !consistent {
                    want.reassembly = Reassembly::start(&hash, total_len, frags);
                    want.phase = Phase::Streaming {
                        since: Instant::now(),
                    };
                } else if want.reassembly.is_none() {
                    // consistent-but-missing reassembly (duplicate META with
                    // no state — first start failed?) — restart it
                    want.reassembly = Reassembly::start(&hash, total_len, frags);
                    want.phase = Phase::Streaming {
                        since: Instant::now(),
                    };
                }
                if want.reassembly.is_some() {
                    if matches!(want.phase, Phase::New | Phase::Requested { .. }) {
                        want.phase = Phase::Streaming {
                            since: Instant::now(),
                        };
                    }
                } else {
                    actions.push(Action::Fail {
                        hash,
                        peer: peer_id.to_vec(),
                    });
                }
            } else {
                // stale meta for a want we dropped
            }
        }
        PeerMsg::Chunk { hash, idx, data } => {
            let mut st = inner.state.lock().expect("swarm state lock");
            if let Some(want) = st.wants.get_mut(&hash) {
                if let Some(r) = want.reassembly.as_mut() {
                    r.insert(idx, data);
                }
            }
        }
        PeerMsg::Eof { hash } => {
            let outcome = {
                let st = inner.state.lock().expect("swarm state lock");
                if st.completed.iter().any(|(h, _)| h == &hash) {
                    Some(EofOutcome::AlreadyComplete)
                } else {
                    st.wants.get(&hash).map(|w| match w.reassembly.as_ref() {
                        None => EofOutcome::NoMeta,
                        Some(r) if r.is_complete() => EofOutcome::Complete,
                        Some(r) => EofOutcome::Gaps(r.missing()),
                    })
                }
            };
            match outcome {
                Some(EofOutcome::Complete) => actions.push(Action::Finalize(hash)),
                Some(EofOutcome::Gaps(missing)) => actions.push(Action::Nak {
                    peer: peer_id.to_vec(),
                    hash,
                    idxs: missing,
                }),
                Some(EofOutcome::NoMeta) => actions.push(Action::WantAgain {
                    peer: peer_id.to_vec(),
                    hash,
                }),
                Some(EofOutcome::AlreadyComplete) | None => {}
            }
        }
        PeerMsg::Nak { hash, idxs } => retransmit(inner, peer_id, &hash, &idxs),
        PeerMsg::Deny { hash } => actions.push(Action::Fail {
            hash,
            peer: peer_id.to_vec(),
        }),
        PeerMsg::Presence { payload } => {
            // ADR-0023: ephemeral telemetry. Off → dropped at the door (no
            // state growth, no channel send, nothing observable). On →
            // last-event-wins map + fanout to subscribers.
            if inner.presence_enabled {
                let at_ms = inner.started.elapsed().as_millis() as u64;
                {
                    let mut st = inner.state.lock().expect("swarm state lock");
                    st.presence
                        .insert(peer_id.to_vec(), (Instant::now(), payload.clone()));
                }
                inner.stats.presence_events.fetch_add(1, Ordering::Relaxed);
                let _ = inner.presence_tx.send(PresenceEvent {
                    from: id_str(peer_id),
                    payload,
                    at_ms,
                });
            }
        }
    }

    for a in actions {
        match a {
            Action::Serve(hash) => spawn_serve(inner, peer_id, hash),
            Action::Nak { peer, hash, idxs } => {
                send_msg(inner, &peer, &PeerMsg::Nak { hash, idxs });
            }
            Action::WantAgain { peer, hash } => send_msg(inner, &peer, &PeerMsg::Want { hash }),
            Action::Fail { hash, peer } => fail_assignment(inner, &hash, &peer),
            Action::Finalize(hash) => finalize(inner, &hash),
            Action::HealHave => send_have(inner, peer_id, true),
        }
    }
}

/// Completion path: BLAKE3 verification, waiter wake, mesh re-announce.
fn finalize(inner: &Arc<Inner>, hash: &[u8; 32]) {
    let (bytes, assigned) = {
        let mut st = inner.state.lock().expect("swarm state lock");
        let want = st.wants.remove(hash);
        let bytes = want
            .as_ref()
            .and_then(|w| w.reassembly.as_ref())
            .and_then(|r| r.assemble());
        let assigned = want.as_ref().and_then(|w| w.assigned.clone());
        if let Some(peer_id) = &assigned {
            if let Some(peer) = st.peers.get_mut(peer_id) {
                peer.want_out = peer.want_out.saturating_sub(1);
            }
        }
        (bytes, assigned)
    };
    let Some(bytes) = bytes else { return };
    // I2: never hand out unverified bytes — even to a swarm peer
    if Hash::of(&bytes).0 != *hash {
        tracing::warn!(
            hash = &Hash(*hash).hex()[..16],
            "peer-served block FAILED blake3 verification — rotating holder"
        );
        if let Some(peer_id) = &assigned {
            fail_assignment(inner, hash, peer_id);
        }
        let mut st = inner.state.lock().expect("swarm state lock");
        st.wants.entry(*hash).or_insert_with(|| Want {
            assigned: None,
            tried: {
                let mut t = HashSet::new();
                if let Some(p) = assigned.clone() {
                    t.insert(p);
                }
                t
            },
            phase: Phase::New,
            reassembly: None,
            first: Instant::now(),
        });
        return;
    }
    {
        let mut st = inner.state.lock().expect("swarm state lock");
        st.completed.push((*hash, bytes.clone()));
        st.retain_completed();
        if let Some(waiters) = st.waiters.remove(hash) {
            for w in waiters {
                let _ = w.send(Some(bytes.clone()));
            }
        }
        // completion re-announce hint (the serving store's own growth is
        // detected by have_pass's bloom-cache change check)
        for peer in st.peers.values_mut() {
            if peer.session.is_some() {
                peer.bloom_pending = true;
            }
        }
    }
    // announce to every connected peer: the mesh effect
    let peer_ids: Vec<Vec<u8>> = {
        let st = inner.state.lock().expect("swarm state lock");
        st.peers.keys().cloned().collect()
    };
    for pid in peer_ids {
        send_have(inner, &pid, true);
    }
}

/// Peer failed us for `hash`: rotate. Never exhausts the want on a single
/// failure — exhaustion needs every holder tried plus the orphan grace.
fn fail_assignment(inner: &Arc<Inner>, hash: &[u8; 32], peer_id: &[u8]) {
    let mut st = inner.state.lock().expect("swarm state lock");
    let assigned_here = st
        .wants
        .get(hash)
        .is_some_and(|w| w.assigned.as_deref() == Some(peer_id));
    if let Some(peer) = st.peers.get_mut(peer_id) {
        peer.tried.insert(*hash);
        if assigned_here {
            peer.want_out = peer.want_out.saturating_sub(1);
        }
    }
    if assigned_here {
        if let Some(want) = st.wants.get_mut(hash) {
            want.assigned = None;
            want.phase = Phase::New;
            want.reassembly = None;
            want.tried.insert(peer_id.to_vec());
        }
    }
}

// ---- send helpers ---------------------------------------------------------------

/// Send one SEALED message to a peer (session required). The socket write is
/// spawned; the state lock is taken once and released before any await.
fn send_msg(inner: &Arc<Inner>, peer_id: &[u8], msg: &PeerMsg) {
    let outbound = {
        let mut st = inner.state.lock().expect("swarm state lock");
        let Some(peer) = st.peers.get_mut(peer_id) else {
            return;
        };
        let Some(session) = peer.session.as_mut() else {
            return;
        };
        let frame = session.seal(msg);
        match peer.link {
            Link::Direct => {
                let target = peer.direct_addr.or_else(|| peer.addrs.first().copied());
                target.map(|target| (frame, None, target))
            }
            Link::Relay | Link::Punching { .. } => st
                .relay_addr
                .map(|relay| (frame, Some(peer_id.to_vec()), relay)),
        }
    };
    if let Some((frame, wrap_for, target)) = outbound {
        let payload = match wrap_for {
            Some(to) => build_routing_header(inner.node.node_id_bytes(), &to, &frame),
            None => frame,
        };
        let sock = Arc::clone(&inner.sock);
        tokio::spawn(async move {
            let _ = sock.send_to(&payload, target).await;
        });
    }
}

/// Send our CLEAR hello to a peer — wrapped in the relay routing header when
/// the link is relay-routed (the relay only forwards `[0x52][from][to][…]`
/// datagrams; a bare hello sent to it is silently dropped).
fn send_clear(inner: &Arc<Inner>, peer_id: &[u8]) {
    let (target, via_relay) = {
        let st = inner.state.lock().expect("swarm state lock");
        let Some(peer) = st.peers.get(peer_id) else {
            return;
        };
        match peer.link {
            Link::Direct => (
                peer.direct_addr.or_else(|| peer.addrs.first().copied()),
                false,
            ),
            Link::Relay | Link::Punching { .. } => (st.relay_addr, true),
        }
    };
    let Some(target) = target else { return };
    let hello = build_clear_hello(&inner.node);
    let payload = if via_relay {
        build_routing_header(inner.node.node_id_bytes(), peer_id, &hello)
    } else {
        hello
    };
    let sock = Arc::clone(&inner.sock);
    tokio::spawn(async move {
        let _ = sock.send_to(&payload, target).await;
    });
}

/// Send (or queue) a HAVE with the current owned-bloom. `force` bypasses the
/// per-peer rate gate (hello/completion events must not be starved — the
/// stale-bloom mesh killer).
fn send_have(inner: &Arc<Inner>, peer_id: &[u8], force: bool) {
    let now = Instant::now();
    let due = {
        let st = inner.state.lock().expect("swarm state lock");
        let Some(peer) = st.peers.get(peer_id) else {
            return;
        };
        peer.session.is_some() && (force || now.duration_since(peer.bloom_last_sent) >= HAVE_EVERY)
    };
    if !due {
        return;
    }
    // build (or reuse) the cached bloom — rebuilt only when owned_count moves
    let (_changed, items, bloom_bytes) = current_bloom(inner);
    let msg = PeerMsg::Have {
        bloom: bloom_bytes,
        items,
    };
    {
        let mut st = inner.state.lock().expect("swarm state lock");
        if let Some(peer) = st.peers.get_mut(peer_id) {
            peer.bloom_last_sent = Instant::now();
            peer.bloom_pending = false;
        }
    }
    send_msg(inner, peer_id, &msg);
}

/// The owned-hash bloom, cached with change detection (a full `owned_hashes()`
/// walk per HAVE would hammer the backing store; the count check is cheap).
/// Returns (changed, items, bytes) so callers can re-announce on movement.
fn current_bloom(inner: &Arc<Inner>) -> (bool, u32, Vec<u8>) {
    let count = inner.serving.owned_count();
    let mut cache = inner.bloom_cache.lock().expect("bloom cache lock");
    if let Some((c, items, bytes)) = cache.as_ref() {
        if *c == count {
            return (false, *items, bytes.clone());
        }
    }
    let mut sorted = inner.serving.owned_hashes();
    sorted.sort_by_key(|a| a.hex());
    let mut bloom = Bloom::with_fpp(BLOOM_CAP, 0.01);
    for h in sorted.iter().take(BLOOM_CAP as usize) {
        bloom.insert(h.hex().as_bytes());
    }
    let items = u32::try_from(sorted.len()).unwrap_or(u32::MAX);
    let bytes = bloom.serialize();
    *cache = Some((count, items, bytes.clone()));
    (true, items, bytes)
}

// ---- loops --------------------------------------------------------------------

async fn register_pass(inner: &Arc<Inner>) {
    // advertise: our socket's local addr (the signal server canonicalizes
    // unspecified IPs to what it observed) + the last OBSERVED address (a
    // free reflexive candidate from the server's vantage)
    let advertised: Vec<SocketAddr> = {
        let st = inner.state.lock().expect("swarm state lock");
        match st.last_observed {
            Some(obs) if obs != inner.local_addr_pub() => vec![inner.local_addr_pub(), obs],
            _ => vec![inner.local_addr_pub()],
        }
    };
    let reply = inner
        .signal_client
        .register(&inner.node, &inner.project, &advertised, false)
        .await;
    let Ok(reply) = reply else {
        // transient blips happen; a sustained failure deserves a diagnosis.
        // a rejected join code is indistinguishable from an unreachable
        // server BY DESIGN (no oracle) — so the hint names both causes.
        let fails = {
            let mut st = inner.state.lock().expect("swarm state lock");
            st.register_fails += 1;
            st.register_fails
        };
        if fails == 3 || fails % 50 == 0 {
            tracing::warn!(
                signal = %inner.signal_client.server_addr(),
                fails,
                "cannot register with the signal server — is it reachable, and is \
                 the join code correct? (a wrong code is dropped silently by design)"
            );
        } else {
            tracing::debug!("signal register failed; retrying");
        }
        return;
    };
    {
        let mut st = inner.state.lock().expect("swarm state lock");
        st.register_fails = 0;
        st.last_observed = Some(reply.observed);
    }
    let mut immediate_punch: Vec<SocketAddr> = Vec::new();
    {
        let mut st = inner.state.lock().expect("swarm state lock");
        if st.relay_addr.is_none() {
            if let Some(relay_entry) = reply.peers.iter().find(|p| p.flags & FLAG_IS_RELAY != 0) {
                if let Some(addr) = relay_entry.addrs.first() {
                    st.relay_addr = Some(*addr);
                }
            }
        }
        let now = Instant::now();
        for entry in &reply.peers {
            if entry.id == inner.node.node_id_bytes() {
                continue;
            }
            if entry.flags & crate::signal::FLAG_IS_RELAY != 0 {
                continue; // infrastructure, not a session peer (addr harvested above)
            }
            let is_new = !st.peers.contains_key(&entry.id);
            let peer = st.peers.entry(entry.id.clone()).or_insert_with(Peer::new);
            peer.last_signal_seen = now;
            if peer.pubkey.is_none() {
                peer.pubkey = Some(entry.pubkey);
            }
            for a in &entry.addrs {
                if !peer.addrs.contains(a) {
                    peer.addrs.push(*a);
                }
            }
            if entry.via_relay && peer.direct_addr.is_none() {
                peer.link = Link::Relay;
            }
            // immediate first probe on discovery — EXCEPT under force_relay
            // (the probe would land before the relay fallback can engage)
            if is_new && !inner.force_relay {
                if let Some(target) = peer.addrs.first().copied() {
                    immediate_punch.push(target);
                }
            }
        }
        // prune peers the signal server no longer lists
        st.peers
            .retain(|_, p| now.duration_since(p.last_signal_seen) < PEER_STALE);
    }
    for target in immediate_punch {
        let hello = build_clear_hello(&inner.node);
        let sock = Arc::clone(&inner.sock);
        tokio::spawn(async move {
            let _ = sock.send_to(&hello, target).await;
        });
    }
}

impl Inner {
    fn local_addr_pub(&self) -> SocketAddr {
        self.sock
            .local_addr()
            .unwrap_or_else(|_| "0.0.0.0:0".parse().expect("static"))
    }
}

async fn punch_pass(inner: &Arc<Inner>) {
    let (direct_probes, relay_hellos): (Vec<SocketAddr>, Vec<Vec<u8>>) = {
        let mut st = inner.state.lock().expect("swarm state lock");
        let mut probes = Vec::new();
        let mut hellos = Vec::new();
        for (id, peer) in st.peers.iter_mut() {
            if peer.session.is_some() {
                continue; // linked
            }
            if let Link::Punching { since } = peer.link {
                if since.elapsed() >= PUNCH_DEADLINE || inner.force_relay {
                    // canonical single requester: the LOWER node id
                    if inner.node.node_id_bytes() < id.as_slice() && !peer.relay_requested {
                        peer.relay_requested = true;
                        let id2 = id.clone();
                        let inner2 = Arc::clone(inner);
                        tokio::spawn(async move {
                            request_relay_grant(&inner2, &id2).await;
                        });
                    }
                }
            }
            if inner.force_relay || peer.link == Link::Relay {
                // relay-routed handshake RETRIES: the first hello is a
                // learning-phase drop by design — without periodic retries
                // the pair deadlocks (nobody ever speaks again)
                hellos.push(id.clone());
                continue;
            }
            if peer.addrs.is_empty() {
                continue;
            }
            // the FIRST probe for a peer pair is one punch attempt (the
            // success-rate denominator); later passes are retries of the
            // same attempt, not new ones
            if peer.probe_idx == 0 {
                inner.stats.punch_attempts.fetch_add(1, Ordering::Relaxed);
            }
            let idx = peer.probe_idx as usize % peer.addrs.len();
            peer.probe_idx = peer.probe_idx.wrapping_add(1);
            if let Some(target) = peer.addrs.get(idx).copied() {
                probes.push(target);
            }
        }
        (probes, hellos)
    };
    for target in direct_probes {
        let hello = build_clear_hello(&inner.node);
        let sock = Arc::clone(&inner.sock);
        let _ = sock.send_to(&hello, target).await;
    }
    for pid in relay_hellos {
        send_clear(inner, &pid);
    }
}

async fn request_relay_grant(inner: &Arc<Inner>, peer_id: &[u8]) {
    match inner
        .signal_client
        .request_relay(&inner.node, peer_id)
        .await
    {
        Ok(Some(relay)) => {
            {
                let mut st = inner.state.lock().expect("swarm state lock");
                st.relay_addr = Some(relay);
                if let Some(peer) = st.peers.get_mut(peer_id) {
                    if peer.direct_addr.is_none() {
                        peer.link = Link::Relay;
                    }
                }
            }
            tracing::info!(
                peer = %String::from_utf8_lossy(peer_id),
                relay = %relay,
                "relay fallback engaged"
            );
            // bootstrap hello via the relay (starts the learning phase)
            send_clear(inner, peer_id);
        }
        Ok(None) => tracing::warn!("relay grant: no relay registered with the signal server"),
        Err(e) => tracing::debug!("relay grant failed ({e})"),
    }
}

fn have_pass(inner: &Arc<Inner>) {
    // change detection: serving-store count moved (fetched blocks landed by
    // the hydrate caller) → rebuild the cached bloom + refresh every session
    let (changed, _items, _bytes) = current_bloom(inner);
    let pending: Vec<Vec<u8>> = {
        let mut st = inner.state.lock().expect("swarm state lock");
        if changed {
            for peer in st.peers.values_mut() {
                if peer.session.is_some() {
                    peer.bloom_pending = true;
                }
            }
        }
        st.peers
            .iter()
            .filter(|(_, p)| p.bloom_pending && p.session.is_some())
            .map(|(id, _)| id.clone())
            .collect()
    };
    for pid in pending {
        send_have(inner, &pid, false);
    }
}

fn want_pass(inner: &Arc<Inner>) {
    let now = Instant::now();
    // outbound actions collected under the lock, sent after release
    let mut outbound: Vec<(Vec<u8>, PeerMsg)> = Vec::new();
    let mut failures: Vec<([u8; 32], Vec<u8>)> = Vec::new();
    let mut finalizes: Vec<[u8; 32]> = Vec::new();
    let mut serves: Vec<([u8; 32], Vec<u8>)> = Vec::new();
    {
        let mut st = inner.state.lock().expect("swarm state lock");

        // 1) fail orphaned wants past every grace
        let dead: Vec<[u8; 32]> = st
            .wants
            .iter()
            .filter(|(_, w)| w.first.elapsed() > ORPHAN_GRACE)
            .map(|(h, _)| *h)
            .collect();
        for hash in dead {
            let want = st.wants.remove(&hash);
            if let Some(w) = want {
                if let Some(peer_id) = w.assigned {
                    if let Some(peer) = st.peers.get_mut(&peer_id) {
                        peer.want_out = peer.want_out.saturating_sub(1);
                    }
                }
                if let Some(waiters) = st.waiters.remove(&hash) {
                    for w in waiters {
                        let _ = w.send(None);
                    }
                }
            }
        }

        // 2) schedule wants with no live assignment
        let unassigned: Vec<[u8; 32]> = st
            .wants
            .iter()
            .filter(|(_, w)| matches!(w.phase, Phase::New))
            .map(|(h, _)| *h)
            .collect();
        for hash in unassigned {
            let mut candidates: Vec<(&Vec<u8>, u32)> = Vec::new();
            for (id, peer) in &st.peers {
                if peer.session.is_none()
                    || peer.want_out >= PER_PEER_ACTIVE_WANTS
                    || peer.tried.contains(&hash)
                {
                    continue;
                }
                let maybe = match (&peer.bloom, peer.bloom_partial) {
                    (Some(b), false) => b.might_contain(Hash(hash).hex().as_bytes()),
                    (Some(_), true) => true, // partial bloom: unknown ⇒ candidate
                    (None, _) => false,      // no bloom yet: wait for hello/have
                };
                if maybe {
                    candidates.push((id, peer.want_out));
                }
            }
            if candidates.is_empty() {
                continue; // orphan: the grace timer above eventually fails it
            }
            // fewest-outstanding first; ties rotate so equal-load holders
            // actually share work (the load-balancing semantics)
            candidates.sort_by_key(|(_, load)| *load);
            let min_load = candidates[0].1;
            let tied: Vec<&Vec<u8>> = candidates
                .iter()
                .filter(|(_, l)| *l == min_load)
                .map(|(id, _)| *id)
                .collect();
            let pick = tied
                [(inner.assign_rr.fetch_add(1, Ordering::Relaxed) as usize) % tied.len()]
            .clone();
            if let Some(peer) = st.peers.get_mut(&pick) {
                peer.want_out += 1;
            }
            if let Some(want) = st.wants.get_mut(&hash) {
                want.assigned = Some(pick.clone());
                want.phase = Phase::Requested {
                    since: now,
                    last_request: now,
                };
                want.tried.remove(&pick); // fresh assignment clears a stale mark
            }
            outbound.push((pick, PeerMsg::Want { hash }));
        }

        // 3) re-request / timeouts / stall NAKs
        let hashes: Vec<[u8; 32]> = st.wants.keys().copied().collect();
        for hash in hashes {
            let Some(want) = st.wants.get(&hash) else {
                continue;
            };
            let assigned = want.assigned.clone();
            let (phase, since, last_request, stall_progress, missing, complete) = {
                let w = want;
                let (since, last_request) = match &w.phase {
                    Phase::Requested {
                        since,
                        last_request,
                    } => (*since, *last_request),
                    Phase::Streaming { since } => (*since, *since),
                    Phase::New => (now, now),
                };
                let (progress, missing, complete) = match w.reassembly.as_ref() {
                    Some(r) => (r.last_progress, r.missing(), r.is_complete()),
                    None => (now, Vec::new(), false),
                };
                (
                    w.phase.clone_debug(),
                    since,
                    last_request,
                    progress,
                    missing,
                    complete,
                )
            };
            let peer_id = assigned;

            if complete {
                // receiver-side completion (EOF may have been dropped —
                // the idle-NAK path's safety net)
                finalizes.push(hash);
                continue;
            }

            match phase {
                PhaseKind::New => {}
                PhaseKind::Requested => {
                    if let Some(peer_id) = peer_id {
                        if now.duration_since(last_request) >= WANT_RE_REQUEST {
                            outbound.push((peer_id.clone(), PeerMsg::Want { hash }));
                            if let Some(w) = st.wants.get_mut(&hash) {
                                w.phase = Phase::Requested {
                                    since,
                                    last_request: now,
                                };
                            }
                        }
                        if now.duration_since(since) > WANT_TIMEOUT {
                            failures.push((hash, peer_id));
                        }
                    }
                }
                PhaseKind::Streaming => {
                    if let Some(peer_id) = &peer_id {
                        // idle-NAK: fragment flow stalled (receiver-side
                        // overflow recovery) — bounded by STREAM_STALL
                        if stall_progress.elapsed() > STREAM_STALL && !missing.is_empty() {
                            if let Some(peer) = st.peers.get_mut(peer_id) {
                                if now.duration_since(peer.nak_last) >= STREAM_STALL {
                                    peer.nak_last = now;
                                    outbound.push((
                                        peer_id.clone(),
                                        PeerMsg::Nak {
                                            hash,
                                            idxs: missing,
                                        },
                                    ));
                                }
                            }
                        }
                        if now.duration_since(since) > WANT_TIMEOUT {
                            failures.push((hash, peer_id.clone()));
                        }
                    } else {
                        // streaming without assignment (post-failure re-heal):
                        // treat as stalled and restart from New
                        if let Some(w) = st.wants.get_mut(&hash) {
                            w.phase = Phase::New;
                        }
                    }
                }
            }
        }

        // 4) prune idle serve records
        st.serve
            .retain(|_, rec| now.duration_since(rec.last) < SERVE_IDLE_PRUNE);
    }
    for (peer, msg) in outbound {
        send_msg(inner, &peer, &msg);
    }
    for (hash, peer) in failures {
        fail_assignment(inner, &hash, &peer);
    }
    for hash in finalizes {
        finalize(inner, &hash);
    }
    for (hash, peer) in serves.drain(..) {
        spawn_serve(inner, &peer, hash);
    }
}

/// Debug-friendly phase tag for the want_pass bookkeeping pass.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PhaseKind {
    New,
    Requested,
    Streaming,
}

impl Phase {
    fn clone_debug(&self) -> PhaseKind {
        match self {
            Phase::New => PhaseKind::New,
            Phase::Requested { .. } => PhaseKind::Requested,
            Phase::Streaming { .. } => PhaseKind::Streaming,
        }
    }
}

// ---- serving --------------------------------------------------------------------

fn spawn_serve(inner: &Arc<Inner>, peer_id: &[u8], hash: [u8; 32]) {
    let inner = Arc::clone(inner);
    let peer_id = peer_id.to_vec();
    tokio::spawn(async move {
        let Some(bytes) = inner.serving.block_bytes(&Hash(hash)) else {
            send_msg(&inner, &peer_id, &PeerMsg::Deny { hash });
            return;
        };
        let total_len = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
        let frags = u16::try_from(bytes.len().div_ceil(MAX_FRAG_DATA)).unwrap_or(u16::MAX);
        {
            let mut st = inner.state.lock().expect("swarm state lock");
            st.serve.insert(
                (peer_id.clone(), hash),
                ServeRecord {
                    last: Instant::now(),
                },
            );
        }
        inner.stats.blocks_served.fetch_add(1, Ordering::Relaxed);
        inner
            .stats
            .bytes_served
            .fetch_add(bytes.len() as u64, Ordering::Relaxed);
        send_msg(
            &inner,
            &peer_id,
            &PeerMsg::Meta {
                hash,
                total_len,
                frags,
            },
        );
        // paced fragment stream: micro-yields keep the receiver's UDP buffer
        // from overflowing during multi-block bursts
        for idx in 0..frags {
            let start = usize::from(idx) * MAX_FRAG_DATA;
            let end = (start + MAX_FRAG_DATA).min(bytes.len());
            let data = bytes[start..end].to_vec();
            send_msg(&inner, &peer_id, &PeerMsg::Chunk { hash, idx, data });
            if idx % 8 == 7 {
                tokio::task::yield_now().await;
            }
            if idx % 64 == 63 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        }
        send_msg(&inner, &peer_id, &PeerMsg::Eof { hash });
    });
}

/// NAK retransmit: re-read the block (memory-light) and re-send only the
/// requested fragments.
fn retransmit(inner: &Arc<Inner>, peer_id: &[u8], hash: &[u8; 32], idxs: &[u16]) {
    let hash = *hash;
    let known = {
        let mut st = inner.state.lock().expect("swarm state lock");
        let key = (peer_id.to_vec(), hash);
        match st.serve.get_mut(&key) {
            Some(rec) => {
                rec.last = Instant::now();
                true
            }
            None => false, // stale NAK (record pruned) — want re-request recovers
        }
    };
    if !known {
        return;
    }
    let Some(bytes) = inner.serving.block_bytes(&Hash(hash)) else {
        return;
    };
    let inner = Arc::clone(inner);
    let peer_id = peer_id.to_vec();
    let idxs = idxs.to_vec();
    tokio::spawn(async move {
        for idx in idxs {
            let start = usize::from(idx) * MAX_FRAG_DATA;
            if start >= bytes.len() {
                continue;
            }
            let end = (start + MAX_FRAG_DATA).min(bytes.len());
            send_msg(
                &inner,
                &peer_id,
                &PeerMsg::Chunk {
                    hash,
                    idx,
                    data: bytes[start..end].to_vec(),
                },
            );
            tokio::task::yield_now().await;
        }
    });
}

/// Node id bytes → id string. Node ids are `NodeKey::node_id()` strings
/// (device ids); lossy decode keeps a hostile/garbled peer from failing the
/// whole presence path.
fn id_str(id: &[u8]) -> String {
    String::from_utf8_lossy(id).into_owned()
}

#[cfg(test)]
mod presence_tests {
    use super::*;

    #[test]
    fn presence_msg_codec_roundtrip_and_bounds() {
        use crate::session::PeerMsg;
        let msg = PeerMsg::Presence {
            payload: br#"{"editor":"alice","frame":1234,"rate":24,"action":"playhead"}"#.to_vec(),
        };
        let enc = msg.encode();
        assert_eq!(PeerMsg::decode(&enc), Some(msg));
        // empty payload round-trips
        let empty = PeerMsg::Presence {
            payload: Vec::new(),
        };
        assert_eq!(PeerMsg::decode(&empty.encode()), Some(empty));
        // oversized payload is REFUSED at decode (the bound is the contract)
        let fat = vec![0x41u8; 1201];
        let fat_msg = PeerMsg::Presence { payload: fat };
        let enc = fat_msg.encode();
        assert_eq!(PeerMsg::decode(&enc), None, ">1200B presence refused");
        // a well-formed 1200B payload is accepted
        let ok = vec![0x41u8; 1200];
        let ok_msg = PeerMsg::Presence { payload: ok };
        assert_eq!(PeerMsg::decode(&ok_msg.encode()), Some(ok_msg));
        // presence map prunes stale entries via TTL logic
        let mut st = State::default();
        let stale_at = Instant::now()
            .checked_sub(PRESENCE_TTL + Duration::from_secs(1))
            .expect("ttl fits in Instant range");
        st.presence.insert(vec![1, 2, 3], (stale_at, b"x".to_vec()));
        st.presence
            .insert(vec![4, 5, 6], (Instant::now(), b"y".to_vec()));
        st.presence.retain(|_, (at, _)| at.elapsed() < PRESENCE_TTL);
        assert_eq!(st.presence.len(), 1, "stale presence pruned");
        assert!(st.presence.contains_key(&vec![4, 5, 6]));
    }
}
