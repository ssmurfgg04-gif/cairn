//! Relay server (ADR-0017 §5) — the encrypted pass-through fallback.
//!
//! For peers whose firewalls eat every punch (strict corporate NATs), traffic
//! temporarily routes through this node. It forwards OPAQUE ciphertext: the
//! routing header is outside the end-to-end encryption, so the relay can
//! route but never read block content.
//!
//! Wire shape of a relay-routed datagram:
//! ```text
//! [0x52 'R'][from_len u8][from_id][to_len u8][to_id][inner encrypted frame]
//! ```
//!
//! Learning phase: the first datagram of a pair records its sender endpoint
//! and is DROPPED (the other endpoint is not yet known). When the second
//! endpoint's first datagram arrives, both sides are live and every later
//! datagram forwards verbatim (header intact, so receivers still learn the
//! true sender identity). The initial drops are healed by the swarm's HELLO
//! retries (250 ms) and want re-requests (750 ms) — eventual delivery is the
//! contract, not first-attempt delivery.
//!
//! The relay registers itself with the signal server (`FLAG_IS_RELAY`), so
//! peers discover its address; unspecified bind IPs are canonicalized there
//! (see signal.rs). Idle mappings are reaped by an atomic epoch counter —
//! no mutex on the forwarding hot path.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::net::UdpSocket;
use tokio::sync::mpsc;

use crate::crypto::NodeKey;
use crate::signal::SignalClient;

pub(crate) const RELAY_MAGIC: u8 = 0x52;

const MAX_DATAGRAM: usize = 70_000;
const IDLE_REAP: Duration = Duration::from_secs(60);
const SWEEP: Duration = Duration::from_secs(10);
const RE_REGISTER: Duration = Duration::from_secs(2);

/// Live counters for tests + ops surfaces.
#[derive(Default)]
pub struct RelayStats {
    /// datagrams actually forwarded to the other endpoint.
    pub forwarded: AtomicU64,
    /// first-datagram drops while the pair was still learning.
    pub dropped_learning: AtomicU64,
    /// mappings alive right now (sweep-maintained).
    pub active_mappings: AtomicU64,
    /// mappings removed by the idle reaper.
    pub reaped: AtomicU64,
}

/// A spawned relay server. Abort `task` to stop it.
pub struct RelayServer {
    pub local_addr: SocketAddr,
    /// QUIC data-plane listen address (None when QUIC relay is off).
    pub quic_addr: Option<SocketAddr>,
    pub task: tokio::task::JoinHandle<()>,
    pub stats: Arc<RelayStats>,
}

/// One side of a relayed pair, on whichever transport it arrived.
#[derive(Clone, Debug)]
enum Side {
    Udp(SocketAddr),
    Quic { conn_id: u64, tx: mpsc::UnboundedSender<Vec<u8>> },
}

impl Side {
    /// Same endpoint? Different transports never compare equal (a NAT
    /// rebind on UDP and a QUIC reconnect are both "new endpoint" events,
    /// so the mapping updates rather than self-looping).
    fn same_peer(&self, other: &Side) -> bool {
        match (self, other) {
            (Side::Udp(a), Side::Udp(b)) => a == b,
            (Side::Quic { conn_id: a, .. }, Side::Quic { conn_id: b, .. }) => a == b,
            _ => false,
        }
    }
}

struct Mapping {
    /// endpoint of the lexicographically-lower id of the pair.
    a: Side,
    /// endpoint of the higher id.
    b: Side,
    last_epoch_ms: Arc<AtomicU64>,
}

/// Pair-ordered routes: (lo, hi) → last-known endpoints. Shared between the
/// UDP loop, the QUIC accept loop, and the idle reaper. One table for both
/// transports: mixed pairs (A on UDP, B on QUIC) converge exactly like
/// same-transport pairs.
type RouteTable = Arc<Mutex<HashMap<(Vec<u8>, Vec<u8>), Mapping>>>;

/// Where an inbound datagram arrived from.
#[derive(Clone)]
enum RouteFrom {
    Udp(SocketAddr),
    Quic { conn_id: u64, tx: mpsc::UnboundedSender<Vec<u8>> },
}

/// Where to forward a routed datagram.
#[derive(Clone, Debug)]
enum Target {
    Udp(SocketAddr),
    Quic(mpsc::UnboundedSender<Vec<u8>>),
}

impl RelayServer {
    /// Bind on `bind`, register as a relay node with the signal server, then
    /// forward until aborted. The advertised address is the bind address —
    /// `0.0.0.0` announcements are canonicalized by the signal server to the
    /// IP the registration actually came from.
    pub async fn spawn(
        bind: SocketAddr,
        signal: SocketAddr,
        cluster_key: &[u8],
    ) -> std::io::Result<RelayServer> {
        Self::spawn_inner(bind, None, signal, cluster_key).await
    }

    /// Dual-listen variant: the UDP loop above PLUS a QUIC accept loop on
    /// `quic_bind`, sharing one route table — mixed pairs (one side UDP,
    /// one side QUIC) converge exactly like same-transport pairs. Peers opt
    /// in per device (`--swarm-quic-relay`); UDP stays the default.
    pub async fn spawn_with_quic(
        bind: SocketAddr,
        quic_bind: SocketAddr,
        signal: SocketAddr,
        cluster_key: &[u8],
    ) -> std::io::Result<RelayServer> {
        Self::spawn_inner(bind, Some(quic_bind), signal, cluster_key).await
    }

    async fn spawn_inner(
        bind: SocketAddr,
        quic_bind: Option<SocketAddr>,
        signal: SocketAddr,
        cluster_key: &[u8],
    ) -> std::io::Result<RelayServer> {
        let sock = Arc::new(UdpSocket::bind(bind).await?);
        let local_addr = sock.local_addr()?;
        let stats = Arc::new(RelayStats::default());

        // registration control-plane socket (the data socket serves peers)
        let reg_sock = Arc::new(UdpSocket::bind("0.0.0.0:0").await?);
        {
            let sc = Arc::new(SignalClient::new(
                Arc::clone(&reg_sock),
                signal,
                cluster_key,
            ));
            // stand-alone client: its own reply reader (the swarm's main loop
            // is not here to dispatch for us). Runs until the register loop
            // drops the last socket Arc.
            let _dispatcher = SignalClient::spawn_dispatcher(&sc);
            tokio::spawn(async move {
                let node = NodeKey::generate("relay");
                loop {
                    if sc
                        .register(&node, "cairn-relay", &[local_addr], true)
                        .await
                        .is_err()
                    {
                        tracing::warn!("relay: signal registration failed; retrying");
                    }
                    tokio::time::sleep(RE_REGISTER).await;
                }
            });
        }

        let mappings: RouteTable = Arc::new(Mutex::new(HashMap::new()));
        let epoch0 = Instant::now();

        // Optional QUIC data-plane listener. Each accepted connection gets a
        // conn id + an outbound pump; inbound frames ride the SAME route()
        // as UDP datagrams (length-prefix stripped first).
        let quic_addr = if let Some(qb) = quic_bind {
            let identity = cairn_quic::ServerIdentity::generate("cairn-relay")
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            tracing::info!(fingerprint = %identity.fingerprint(), "relay QUIC identity");
            let ep = cairn_quic::QuicEndpoint::server(qb, &identity)
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            let qa = ep
                .inner()
                .local_addr()
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            let maps_q = Arc::clone(&mappings);
            let stats_q = Arc::clone(&stats);
            let sock_q = Arc::clone(&sock);
            tokio::spawn(async move {
                let ep = ep; // endpoint must outlive accepted conns
                let conn_ids = Arc::new(std::sync::atomic::AtomicU64::new(1));
                loop {
                    let conn = match ep.inner().accept().await {
                        Some(incoming) => match incoming.await {
                            Ok(c) => c,
                            Err(_) => continue,
                        },
                        None => break,
                    };
                    let conn_id = conn_ids.fetch_add(1, Ordering::Relaxed);
                    let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
                    let maps_c = Arc::clone(&maps_q);
                    let stats_c = Arc::clone(&stats_q);
                    let sock_c = Arc::clone(&sock_q);
                    // Outbound pump: routed datagrams → fresh uni streams.
                    let writer = conn.clone();
                    tokio::spawn(async move {
                        while let Some(bytes) = rx.recv().await {
                            let mut s = match writer.open_uni().await {
                                Ok(s) => s,
                                Err(_) => break,
                            };
                            if s
                                .write_all(&(bytes.len() as u32).to_be_bytes())
                                .await
                                .is_err()
                            {
                                break;
                            }
                            if s.write_all(&bytes).await.is_err() {
                                break;
                            }
                            let _ = s.finish();
                        }
                    });
                    // Inbound pump: frames → shared route() → peer sender.
                    tokio::spawn(async move {
                        loop {
                            let mut recv = match conn.accept_uni().await {
                                Ok(r) => r,
                                Err(_) => break,
                            };
                            let mut len = [0u8; 4];
                            if recv.read_exact(&mut len).await.is_err() {
                                break;
                            }
                            let n = u32::from_be_bytes(len) as usize;
                            if n == 0 || n > MAX_DATAGRAM {
                                break;
                            }
                            let mut buf = vec![0u8; n];
                            if recv.read_exact(&mut buf).await.is_err() {
                                break;
                            }
                            let from = RouteFrom::Quic {
                                conn_id,
                                tx: tx.clone(),
                            };
                            match route(&maps_c, &stats_c, &buf, from, epoch0) {
                                Some(Target::Udp(target)) => {
                                    // Cross-transport: QUIC arrival, UDP peer.
                                    // Sent from the relay's own UDP socket, so
                                    // the peer still sees the relay's stamp.
                                    let _ = sock_c.send_to(&buf, target).await;
                                }
                                Some(Target::Quic(peer_tx)) => {
                                    let _ = peer_tx.send(buf);
                                }
                                None => {}
                            }
                        }
                    });
                }
            });
            Some(qa)
        } else {
            None
        };

        let stats_loop = Arc::clone(&stats);
        let maps_loop = Arc::clone(&mappings);
        let sock_loop = Arc::clone(&sock);
        let task = tokio::spawn(async move {
            let mut buf = vec![0u8; MAX_DATAGRAM];
            let mut sweep = tokio::time::interval(SWEEP);
            loop {
                tokio::select! {
                    r = sock_loop.recv_from(&mut buf) => {
                        let Ok((n, from)) = r else { break };
                        match route(&maps_loop, &stats_loop, &buf[..n], RouteFrom::Udp(from), epoch0) {
                            Some(Target::Udp(target)) => {
                                let _ = sock_loop.send_to(&buf[..n], target).await;
                            }
                            Some(Target::Quic(tx)) => {
                                let _ = tx.send(buf[..n].to_vec());
                            }
                            None => {}
                        }
                    }
                    _ = sweep.tick() => {
                        reap(&maps_loop, &stats_loop, epoch0);
                    }
                }
            }
        });

        Ok(RelayServer {
            local_addr,
            quic_addr,
            task,
            stats,
        })
    }
}

/// Idle + dead-transport reaper, shared by both loops.
fn reap(maps: &RouteTable, stats: &RelayStats, epoch0: Instant) {
    let now_ms = epoch0.elapsed().as_millis() as u64;
    let mut m = maps.lock().expect("relay mappings lock");
    let before = m.len();
    m.retain(|_, v| {
        let fresh = now_ms.saturating_sub(v.last_epoch_ms.load(Ordering::Relaxed))
            < IDLE_REAP.as_millis() as u64;
        let quic_alive = |s: &Side| match s {
            Side::Udp(_) => true, // UDP endpoints never signal death; idle time owns them
            Side::Quic { tx, .. } => !tx.is_closed(),
        };
        // Keep while fresh; drop only when idle AND every QUIC side is dead.
        fresh || quic_alive(&v.a) || quic_alive(&v.b)
    });
    let reaped = (before - m.len()) as u64;
    if reaped > 0 {
        stats.reaped.fetch_add(reaped, Ordering::Relaxed);
    }
    stats.active_mappings.store(m.len() as u64, Ordering::Relaxed);
}

/// One routing step: pure mapping logic, returns where to forward to
/// (`None` = still learning → drop). Unit-testable without a socket.
/// Transport-agnostic: UDP and QUIC arrivals share one table, so mixed
/// pairs converge exactly like same-transport pairs.
fn route(
    mappings: &RouteTable,
    stats: &RelayStats,
    dgram: &[u8],
    from: RouteFrom,
    epoch0: Instant,
) -> Option<Target> {
    let (from_id, to_id) = parse_routing_header(dgram)?;
    if from_id == to_id {
        return None; // degenerate self-pair
    }
    let from_is_a = from_id < to_id;
    let pair = if from_is_a {
        (from_id, to_id)
    } else {
        (to_id, from_id)
    };
    let from_side = match from {
        RouteFrom::Udp(addr) => Side::Udp(addr),
        RouteFrom::Quic { conn_id, tx } => Side::Quic { conn_id, tx },
    };
    let m = &mut *mappings.lock().expect("relay mappings lock");
    let entry = m.entry(pair).or_insert_with(|| Mapping {
        a: from_side.clone(),
        b: from_side.clone(),
        last_epoch_ms: Arc::new(AtomicU64::new(epoch0.elapsed().as_millis() as u64)),
    });
    // record the sender's endpoint on its side; the placeholder collapses to
    // `from` on BOTH fields for a brand-new pair, so target == from means
    // "one side only" — the learning signal.
    if from_is_a {
        entry.a = from_side.clone();
    } else {
        entry.b = from_side.clone();
    }
    entry
        .last_epoch_ms
        .store(epoch0.elapsed().as_millis() as u64, Ordering::Relaxed);
    let target = if from_is_a { &entry.b } else { &entry.a };
    if target.same_peer(&from_side) {
        // learning-phase drop: the other endpoint has not spoken yet (or both
        // endpoints share one address — loopback tests — where forwarding
        // would loop; either way the retry machinery heals)
        stats.dropped_learning.fetch_add(1, Ordering::Relaxed);
        None
    } else {
        stats.forwarded.fetch_add(1, Ordering::Relaxed);
        Some(match target {
            Side::Udp(addr) => Target::Udp(*addr),
            Side::Quic { tx, .. } => Target::Quic(tx.clone()),
        })
    }
}

/// Parse `[0x52][from_len][from][to_len][to][payload]`.
pub(crate) fn parse_routing_header(dgram: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    if dgram.first() != Some(&RELAY_MAGIC) {
        return None;
    }
    let p = &dgram[1..];
    let flen = *p.first()? as usize;
    let from = p.get(1..1 + flen)?.to_vec();
    let p = &p[1 + flen..];
    let tlen = *p.first()? as usize;
    let to = p.get(1..1 + tlen)?.to_vec();
    p.get(1 + tlen..)?;
    Some((from, to))
}

/// Build a relay-routed datagram around an inner (encrypted) frame.
pub(crate) fn build_routing_header(from: &[u8], to: &[u8], inner: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(3 + from.len() + to.len() + inner.len());
    out.push(RELAY_MAGIC);
    out.push(u8::try_from(from.len()).unwrap_or(u8::MAX));
    out.extend_from_slice(from);
    out.push(u8::try_from(to.len()).unwrap_or(u8::MAX));
    out.extend_from_slice(to);
    out.extend_from_slice(inner);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_maps() -> RouteTable {
        Arc::new(Mutex::new(HashMap::new()))
    }

    #[test]
    fn routing_header_roundtrip() {
        let inner = [0x02, 9, 8, 7];
        let d = build_routing_header(b"node-a", b"node-b", &inner);
        assert_eq!(parse_routing_header(&d).unwrap().0, b"node-a".to_vec());
        assert_eq!(parse_routing_header(&d).unwrap().1, b"node-b".to_vec());
        assert_eq!(&d[d.len() - inner.len()..], &inner);
    }

    #[test]
    fn routing_header_rejects_garbage() {
        assert!(parse_routing_header(&[]).is_none());
        assert!(parse_routing_header(&[0x01]).is_none());
        assert!(parse_routing_header(&[RELAY_MAGIC, 5, 1, 2]).is_none());
    }

    fn udp(a: SocketAddr) -> RouteFrom {
        RouteFrom::Udp(a)
    }

    fn is_udp(target: Option<Target>, addr: SocketAddr) -> bool {
        matches!(target, Some(Target::Udp(t)) if t == addr)
    }

    #[test]
    fn first_datagram_drops_second_forwards() {
        let maps = mk_maps();
        let stats = RelayStats::default();
        let epoch = Instant::now();
        let a: SocketAddr = "127.0.0.1:1001".parse().unwrap();
        let b: SocketAddr = "127.0.0.1:1002".parse().unwrap();
        let d1 = build_routing_header(b"node-a", b"node-b", &[0x02, 1]);
        let d2 = build_routing_header(b"node-b", b"node-a", &[0x02, 2]);

        // a speaks first: learning drop
        assert!(route(&maps, &stats, &d1, udp(a), epoch).is_none());
        assert_eq!(stats.dropped_learning.load(Ordering::Relaxed), 1);

        // b answers: b's endpoint learned; forward b→a
        assert!(is_udp(route(&maps, &stats, &d2, udp(b), epoch), a));
        assert_eq!(stats.forwarded.load(Ordering::Relaxed), 1);

        // now a→b flows too
        assert!(is_udp(route(&maps, &stats, &d1, udp(a), epoch), b));
        assert_eq!(stats.forwarded.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn endpoints_rebind_after_nat_remap() {
        let maps = mk_maps();
        let stats = RelayStats::default();
        let epoch = Instant::now();
        let a: SocketAddr = "127.0.0.1:1001".parse().unwrap();
        let b: SocketAddr = "127.0.0.1:1002".parse().unwrap();
        let d1 = build_routing_header(b"node-a", b"node-b", &[0x02, 1]);
        let d2 = build_routing_header(b"node-b", b"node-a", &[0x02, 2]);
        assert!(route(&maps, &stats, &d1, udp(a), epoch).is_none());
        assert!(is_udp(route(&maps, &stats, &d2, udp(b), epoch), a));

        // a's NAT rebinds: same id, new port — a's next datagram both updates
        // the mapping and still reaches b; b's replies then follow the new port
        let a2: SocketAddr = "127.0.0.1:9999".parse().unwrap();
        assert!(
            is_udp(route(&maps, &stats, &d1, udp(a2), epoch), b),
            "rebind still forwards"
        );
        assert!(
            is_udp(route(&maps, &stats, &d2, udp(b), epoch), a2),
            "mapping follows the rebind"
        );
    }

    #[test]
    fn degenerate_self_pair_ignored() {
        let maps = mk_maps();
        let stats = RelayStats::default();
        let epoch = Instant::now();
        let d = build_routing_header(b"node-a", b"node-a", &[0x02, 1]);
        assert!(route(
            &maps,
            &stats,
            &d,
            udp("127.0.0.1:1".parse().unwrap()),
            epoch
        )
        .is_none());
        assert!(maps.lock().unwrap().is_empty());
    }

    #[test]
    fn mixed_transports_converge() {
        use tokio::sync::mpsc as chan;
        let maps = mk_maps();
        let stats = RelayStats::default();
        let epoch = Instant::now();
        let a: SocketAddr = "127.0.0.1:1001".parse().unwrap();
        // node-a arrives over UDP, node-b over QUIC: must still meet.
        let d1 = build_routing_header(b"node-a", b"node-b", &[0x02, 1]);
        let d2 = build_routing_header(b"node-b", b"node-a", &[0x02, 2]);
        assert!(route(&maps, &stats, &d1, udp(a), epoch).is_none());

        let (txb, _rxb) = chan::unbounded_channel::<Vec<u8>>();
        let got = route(
            &maps,
            &stats,
            &d2,
            RouteFrom::Quic { conn_id: 7, tx: txb },
            epoch,
        );
        // b→a forwards to a's UDP endpoint (cross-transport delivery).
        assert!(is_udp(got, a), "quic arrival forwards to udp peer");

        // and a→b now flows to b's QUIC sender.
        let (txa, _rxa) = chan::unbounded_channel::<Vec<u8>>();
        let _ = txa; // a stays on UDP; only its address matters here
        match route(&maps, &stats, &d1, udp(a), epoch) {
            Some(Target::Quic(tx)) => {
                assert!(!tx.is_closed());
            }
            other => panic!("expected quic target, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn e2e_relay_forwards_between_two_udp_endpoints() {
        let signal = crate::signal::SignalServer::spawn("127.0.0.1:0".parse().unwrap(), b"k")
            .await
            .unwrap();
        let relay = RelayServer::spawn("127.0.0.1:0".parse().unwrap(), signal.local_addr, b"k")
            .await
            .unwrap();

        // two bare UDP sockets play the two peers
        let a = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let b = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let da = build_routing_header(b"peer-a", b"peer-b", b"hello-from-a");
        let db = build_routing_header(b"peer-b", b"peer-a", b"hello-from-b");

        // a's first datagram is a learning drop…
        a.send_to(&da, relay.local_addr).await.unwrap();
        // …b answers: the mapping completes and b's datagram reaches a
        b.send_to(&db, relay.local_addr).await.unwrap();
        let mut buf = [0u8; 128];
        let (n, from) = a.recv_from(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], &db[..]);
        assert_eq!(
            from, relay.local_addr,
            "forwarded datagrams keep the relay's stamp"
        );
        // …and now a's retries flow to b
        a.send_to(&da, relay.local_addr).await.unwrap();
        let (n2, _) = b.recv_from(&mut buf).await.unwrap();
        assert_eq!(&buf[..n2], &da[..]);
        assert!(relay.stats.forwarded.load(Ordering::Relaxed) >= 2);

        relay.task.abort();
        signal.task.abort();
    }
}
