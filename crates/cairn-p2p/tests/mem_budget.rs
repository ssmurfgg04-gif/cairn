//! ADR-0024 — allocation budget gate for the live-presence hot path.
//!
//! Methodology lifted from Cloudflare's "How we saved 100 terabytes of
//! memory by optimizing 1.1.1.1" (blog.cloudflare.com/dns-cache-memory-
//! optimization-1111): wrap the system allocator with a counting shim,
//! measure the per-op allocation footprint on a production-shaped
//! workload, and CI-gate the number so a future change cannot silently
//! regress it. Their fleet held 250 billion cache entries; cairn's hot
//! in-memory surfaces are the swarm session buffers and the presence
//! map — orders of magnitude smaller, but the discipline is the same:
//! **a memory regression should fail a test, not a user's machine.**
//!
//! What is budgeted here, end to end per presence event:
//!   broadcast → session seal (ADR-0024: ONE datagram alloc; the
//!   plaintext/ciphertext scratches persist across messages) → UDP send
//!   → peer open+decode → presence-map update (replace, not append:
//!   the last event wins, steady-state live bytes stay flat) →
//!   broadcast-channel fanout.
//!
//! Noise handling: the swarm's periodic tasks (punch loop, signal
//! re-announce) allocate too, so the test measures a same-length idle
//! window first and subtracts a generously scaled floor. The budget is
//! deliberately ~3x the measured shape — it exists to catch
//! *regressions* (a per-event clone of a big struct, an accidental
//! growth loop), not to micro-optimize.

// the workspace denies unsafe_code; this test binary needs exactly one
// unsafe block — the GlobalAlloc impl — and nothing else.
#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cairn_core::hash::Hash;
use cairn_p2p::signal::SignalServer;
use cairn_p2p::swarm::{PresenceEvent, ServeBlocks, Swarm, SwarmConfig};

/// Shared test cluster key (same discipline as e2e.rs).
const KEY: &[u8] = b"mem-budget-key-32-bytes-aaaaaaaa";

// ---------------------------------------------------------------------------
// the counting allocator (this test binary only)
// ---------------------------------------------------------------------------

static ARMED: AtomicBool = AtomicBool::new(false);
static ALLOCS: AtomicU64 = AtomicU64::new(0);
/// LIVE is a SIGNED always-on delta (i64): entries allocated inside the
/// window but freed after disarm (the send-backlog draining) must still
/// decrement, or they read as phantom retention. ARMED gates only the
/// allocation COUNT and the counter reset points.
static LIVE: AtomicI64 = AtomicI64::new(0);

struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = System.alloc(layout);
        if !p.is_null() {
            if ARMED.load(Ordering::Relaxed) {
                ALLOCS.fetch_add(1, Ordering::Relaxed);
            }
            LIVE.fetch_add(layout.size() as i64, Ordering::Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size() as i64, Ordering::Relaxed);
        System.dealloc(ptr, layout);
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let p = System.realloc(ptr, layout, new_size);
        if !p.is_null() {
            if ARMED.load(Ordering::Relaxed) {
                ALLOCS.fetch_add(1, Ordering::Relaxed);
            }
            LIVE.fetch_add(new_size as i64, Ordering::Relaxed);
            LIVE.fetch_sub(layout.size() as i64, Ordering::Relaxed);
        }
        p
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Run `body` with counting armed; returns (allocations, out). The live
/// byte counter stays globally readable — read it AFTER the receiver has
/// drained to measure steady-state retention (transient backlog — the
/// outbound channel queue while the swarm task lags — must not be counted
/// as a leak).
fn measure<R>(body: impl FnOnce() -> R) -> (u64, R) {
    ALLOCS.store(0, Ordering::Relaxed);
    LIVE.store(0, Ordering::Relaxed); // delta origin
    ARMED.store(true, Ordering::Relaxed);
    let out = body();
    ARMED.store(false, Ordering::Relaxed);
    (ALLOCS.load(Ordering::Relaxed), out)
}

// ---------------------------------------------------------------------------
// helpers (e2e.rs's shape, kept minimal)
// ---------------------------------------------------------------------------

#[derive(Default, Clone)]
struct MapServe {
    blocks: Arc<Mutex<HashMap<Hash, Vec<u8>>>>,
}

impl ServeBlocks for MapServe {
    fn block_bytes(&self, h: &Hash) -> Option<Vec<u8>> {
        self.blocks.lock().expect("map poisoned").get(h).cloned()
    }
    fn owned_hashes(&self) -> Vec<Hash> {
        self.blocks
            .lock()
            .expect("map poisoned")
            .keys()
            .copied()
            .collect()
    }
    fn owned_count(&self) -> u64 {
        self.blocks.lock().expect("map poisoned").len() as u64
    }
}

fn cfg(signal: SocketAddr, node: &str, presence: bool) -> SwarmConfig {
    SwarmConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        signal,
        cluster_key: KEY.to_vec(),
        project: "membudget".to_string(),
        node_id: Some(node.to_string()),
        stun: None,
        force_relay: false,
        presence,
    }
}

async fn wait_until(mut cond: impl FnMut() -> bool, what: &str, budget: Duration) {
    let start = std::time::Instant::now();
    while !cond() {
        assert!(start.elapsed() < budget, "timeout waiting for {what}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Wait until the received-event count stops growing (presence is
/// UDP-loss-tolerant BY DESIGN — "a signal, not a log" — so "all N arrived"
/// is not a valid condition; quiescence is).
async fn wait_quiescent(count: impl Fn() -> u64, what: &str, budget: Duration) -> u64 {
    let start = std::time::Instant::now();
    let mut last = count();
    let mut quiet_polls = 0;
    loop {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let now = count();
        if now == last {
            quiet_polls += 1;
            if quiet_polls >= 3 {
                return now;
            }
        } else {
            quiet_polls = 0;
            last = now;
        }
        assert!(
            start.elapsed() < budget,
            "timeout waiting for {what} (stuck at {last})"
        );
    }
}

// ---------------------------------------------------------------------------
// the budget test
// ---------------------------------------------------------------------------

/// Per-event allocation budget for the presence hot path (ADR-0024).
/// Measured shape: ~10 allocs/event end-to-end; budget 3x for CI headroom.
const BUDGET_ALLOC_PER_EVENT: u64 = 32;
/// Steady-state live bytes may not grow with event count: the map holds ONE
/// entry per peer (last-event-wins) and channel events are consumed. After
/// 2000 events the live delta must stay under this ceiling.
const BUDGET_LIVE_CEILING: i64 = 64 * 1024;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn presence_hot_path_allocation_budget() {
    /// Drain a presence receiver fully, stepping over Lagged (an un-drained
    /// broadcast ring reads as phantom retention in the live counter).
    fn drain(rx: &mut tokio::sync::broadcast::Receiver<PresenceEvent>) {
        loop {
            match rx.try_recv() {
                Err(tokio::sync::broadcast::error::TryRecvError::Empty)
                | Err(tokio::sync::broadcast::error::TryRecvError::Closed) => break,
                _ => {}
            }
        }
    }

    let signal = SignalServer::spawn("127.0.0.1:0".parse().unwrap(), KEY)
        .await
        .expect("signal spawn");
    let sig = signal.local_addr;

    let a = Swarm::spawn(cfg(sig, "mb-a", true), Arc::new(MapServe::default()))
        .await
        .expect("a spawn");
    let b = Swarm::spawn(cfg(sig, "mb-b", true), Arc::new(MapServe::default()))
        .await
        .expect("b spawn");

    wait_until(
        || a.stats().peers == 1 && b.stats().peers == 1,
        "session established",
        Duration::from_secs(30),
    )
    .await;

    // keep a subscriber alive (the real consumer shape); drain() empties it
    // fully — try_recv returns Lagged when the receiver fell behind, and an
    // un-drained ring would read as phantom retention in the live counter
    let mut rx = b.subscribe_presence();

    // warmup: page in the whole path, grow the scratches to steady state.
    // The session can be mid-reconnect (e2e-grade tolerance): retry briefly.
    for i in 0..100 {
        let payload = format!(r#"{{"editor":"warm","frame":{i}}}"#);
        let mut reached = false;
        for _ in 0..100 {
            if a.broadcast_presence(payload.as_bytes()) == 1 {
                reached = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(reached, "warmup broadcast {i} could not reach the peer");
    }
    wait_quiescent(
        || b.stats().presence_events,
        "warmup events",
        Duration::from_secs(30),
    )
    .await;
    drain(&mut rx);

    // noise floor: a same-shape idle window
    let (noise, ()) = measure(|| {
        std::thread::sleep(Duration::from_millis(250));
    });
    drain(&mut rx);

    // the flood: 2000 events through the REAL encrypted session. UDP loss
    // and broadcast-channel lag are tolerated BY THE CHANNEL's CONTRACT
    // (e2e.rs presence_flood_stays_bounded: "lag-skips allowed"), so
    // completion = quiescence, not "all N landed". Measurement validity is
    // unaffected: every broadcast pays its send-path allocations whether
    // or not the datagram lands, so net/N is an upper bound per event.
    const N: u64 = 2000;
    let (allocs, ()) = measure(|| {
        for i in 0..N {
            let payload = format!(r#"{{"editor":"alice","frame":{i},"rate":24}}"#);
            let payload = payload.into_bytes();
            assert_eq!(a.broadcast_presence(&payload), 1, "broadcast {i}");
        }
    });
    let received = wait_quiescent(
        || b.stats().presence_events,
        "flood events",
        Duration::from_secs(30),
    )
    .await;
    // the receive path must have been exercised substantially (the flood
    // test's contract only promises "some events accepted, lag allowed")
    assert!(
        received > 100,
        "receive path barely exercised: only {received} events landed"
    );
    // let the receiver settle fully, THEN read the live counter — by now
    // the transient backlog (queued broadcasts, channel ring) has drained,
    // so LIVE reflects true steady-state retention: the map entries, the
    // per-session scratches, and nothing else.
    tokio::time::sleep(Duration::from_millis(250)).await;
    drain(&mut rx);
    let live = LIVE.load(Ordering::Relaxed).max(0);

    // subtract a generous noise floor (the flood window is a few seconds;
    // the floor was measured over 250ms — scale up 32x for headroom)
    let noise_scaled = noise.saturating_mul(32);
    let net = allocs.saturating_sub(noise_scaled);
    let per_event = net / N;

    println!(
        "mem_budget: {allocs} allocs total, noise {noise}, net {net} => {per_event}/event over {N} events ({received} landed); live delta {live} bytes"
    );

    assert!(
        per_event <= BUDGET_ALLOC_PER_EVENT,
        "presence hot path regressed: {per_event} allocs/event (budget {BUDGET_ALLOC_PER_EVENT}) — see ADR-0024"
    );
    assert!(
        live <= BUDGET_LIVE_CEILING,
        "presence is accumulating memory: live delta {live} bytes after {N} events (the map is last-event-wins, a signal not a log) — see ADR-0024"
    );

    // and the map itself: exactly one entry per opted-in peer — the LAST
    // event that LANDED won (frame within the tail of the flood; loss-tolerant)
    let snap = b.presence_snapshot();
    assert_eq!(snap.len(), 1, "last-event-wins: one entry per peer");
    let text = String::from_utf8_lossy(&snap[0].payload).to_string();
    let frame: u64 = text
        .split(r#""frame":"#)
        .nth(1)
        .and_then(|rest| {
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            digits.parse().ok()
        })
        .unwrap_or(0);
    assert!(
        text.contains(r#""editor":"alice""#) && frame >= N / 2,
        "a late flood event should have won, got: {text}"
    );

    // e2e hygiene: swarms down before the runtime drops
    a.shutdown();
    b.shutdown();
}
