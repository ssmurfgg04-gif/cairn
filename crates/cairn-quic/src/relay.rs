#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;

use cairn_core::{CairnError, ErrorKind};

use crate::endpoint::{QuicEndpoint, ServerIdentity};

pub(crate) const RELAY_MAGIC: u8 = 0x52;
const REAP_EVERY: u64 = 128;

#[derive(Default)]
pub struct RelayStats {
    pub forwarded: AtomicU64,
    pub dropped_learning: AtomicU64,
    pub active_mappings: AtomicU64,
}

type Tx = mpsc::UnboundedSender<Vec<u8>>;

struct Side {
    conn_id: u64,
    tx: Tx,
}

struct Mapping {
    a: Option<Side>,
    b: Option<Side>,
}

type RouteTable = Arc<Mutex<HashMap<(Vec<u8>, Vec<u8>), Mapping>>>;

fn pair_key(from_id: &[u8], to_id: &[u8]) -> ((Vec<u8>, Vec<u8>), bool) {
    if from_id < to_id {
        ((from_id.to_vec(), to_id.to_vec()), true)
    } else {
        ((to_id.to_vec(), from_id.to_vec()), false)
    }
}

fn parse_routing_header(dgram: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
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

/// Route one datagram from `conn_id`/`tx`. Returns bytes to forward to the
/// peer's sender, or None for a learning drop. Pure mapping logic.
fn route(
    table: &RouteTable,
    stats: &RelayStats,
    dgram: &[u8],
    conn_id: u64,
    tx: &Tx,
) -> Option<(Tx, Vec<u8>)> {
    let (from_id, to_id) = parse_routing_header(dgram)?;
    if from_id == to_id {
        return None;
    }
    let (key, from_is_a) = pair_key(&from_id, &to_id);
    let mut m = table.lock().expect("quic relay table");
    let entry = m.entry(key).or_insert_with(|| Mapping { a: None, b: None });
    let side = Side {
        conn_id,
        tx: tx.clone(),
    };
    if from_is_a {
        entry.a = Some(side);
    } else {
        entry.b = Some(side);
    }
    let target = if from_is_a { &entry.b } else { &entry.a };
    match target {
        Some(t) if t.conn_id != conn_id => {
            stats.forwarded.fetch_add(1, Ordering::Relaxed);
            Some((t.tx.clone(), dgram.to_vec()))
        }
        _ => {
            stats.dropped_learning.fetch_add(1, Ordering::Relaxed);
            None
        }
    }
}

fn reap(table: &RouteTable, stats: &RelayStats) {
    let mut m = table.lock().expect("quic relay table");
    m.retain(|_, e| {
        let alive = |s: &Option<Side>| s.as_ref().is_some_and(|s| !s.tx.is_closed());
        alive(&e.a) || alive(&e.b)
    });
    stats
        .active_mappings
        .store(m.len() as u64, Ordering::Relaxed);
}

/// A spawned QUIC relay server. Abort `task` to stop it.
pub struct QuicRelayServer {
    pub local_addr: SocketAddr,
    pub fingerprint: String,
    pub task: tokio::task::JoinHandle<()>,
    pub stats: Arc<RelayStats>,
}

static CONN_IDS: AtomicU64 = AtomicU64::new(1);

impl QuicRelayServer {
    /// Bind a server endpoint presenting `identity`.
    pub fn spawn(bind: SocketAddr) -> Result<Self, CairnError> {
        let identity = ServerIdentity::generate("cairn-relay")
            .map_err(|e| CairnError::new(ErrorKind::Internal, format!("relay id: {e}")))?;
        let fingerprint = identity.fingerprint();
        let ep = QuicEndpoint::server(bind, &identity)
            .map_err(|e| CairnError::new(ErrorKind::Io, format!("relay bind: {e}")))?;
        let local_addr = ep
            .inner()
            .local_addr()
            .map_err(|e| CairnError::new(ErrorKind::Io, format!("relay addr: {e}")))?;
        let stats = Arc::new(RelayStats::default());
        let stats_for_task = Arc::clone(&stats);
        let table: RouteTable = Arc::new(Mutex::new(HashMap::new()));
        // Endpoint must outlive accepted conns: move it into the task.
        let task = tokio::spawn(async move {
            let ep_srv = ep;
            loop {
                let conn = match ep_srv.inner().accept().await {
                    Some(incoming) => match incoming.await {
                        Ok(c) => c,
                        Err(_) => continue,
                    },
                    None => break, // endpoint closed
                };
                let conn_id = CONN_IDS.fetch_add(1, Ordering::Relaxed);
                let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
                let table = Arc::clone(&table);
                let stats = Arc::clone(&stats_for_task);
                // Outbound pump: forwarded datagrams -> fresh uni streams via transport.
                let transport_out = crate::QuicTransport::from_accepted(conn.clone());
                tokio::spawn(async move {
                    while let Some(bytes) = rx.recv().await {
                        if transport_out.send_frame("r", &bytes).await.is_err() {
                            break;
                        }
                    }
                });
                // Inbound pump: frames -> route -> peer sender.
                let table2 = Arc::clone(&table);
                let stats2 = Arc::clone(&stats);
                tokio::spawn(async move {
                    let transport = crate::QuicTransport::from_accepted(conn);
                    let mut counter: u64 = 0;
                    loop {
                        let frame = match transport.recv_frame().await {
                            Ok(f) => f,
                            Err(_) => break,
                        };
                        if let Some((peer_tx, out)) =
                            route(&table2, &stats2, &frame.data, conn_id, &tx)
                        {
                            let _ = peer_tx.send(out);
                        }
                        counter += 1;
                        if counter % REAP_EVERY == 0 {
                            reap(&table2, &stats2);
                        }
                    }
                });
            }
        });
        tracing::info!(%local_addr, fingerprint = %fingerprint, "quic relay up");
        Ok(Self {
            local_addr,
            fingerprint,
            task,
            stats: Arc::clone(&stats),
        })
    }
}

/// QUIC relay client: one connection, many datagram frames.
/// Receive via `recv_loop`, which invokes `on_frame` per datagram.
#[derive(Clone)]
pub struct QuicRelayClient {
    transport: super::QuicTransport,
    _endpoint: QuicEndpoint,
}

impl QuicRelayClient {
    /// Dial a relay server (accept-any TLS — see `trust` docs).
    pub async fn dial(server: SocketAddr) -> Result<Self, CairnError> {
        let endpoint = QuicEndpoint::client_insecure()?;
        let transport =
            super::QuicTransport::connect(endpoint.inner(), server, "cairn-relay").await?;
        Ok(Self {
            transport,
            _endpoint: endpoint,
        })
    }

    /// Send one R-magic datagram (fire-and-forget; relay retries heal).
    pub async fn send(&self, dgram: &[u8]) -> Result<(), CairnError> {
        self.transport.send_frame("r", dgram).await
    }

    /// Receive loop: invokes `on_frame` per inbound datagram until the
    /// connection drops. Returns when the connection drops.
    pub async fn recv_loop(
        &self,
        on_frame: impl Fn(Vec<u8>) + Send + 'static,
    ) -> Result<(), CairnError> {
        loop {
            let frame = self.transport.recv_frame().await?;
            on_frame(frame.data.to_vec());
        }
    }
}

/// Build an R-magic datagram (same shape as the UDP relay path).
pub fn build_datagram(from: &[u8], to: &[u8], inner: &[u8]) -> Vec<u8> {
    assert!(from.len() <= 255, "from node_id exceeds 255 bytes");
    assert!(to.len() <= 255, "to node_id exceeds 255 bytes");
    let mut out = Vec::with_capacity(3 + from.len() + to.len() + inner.len());
    out.push(RELAY_MAGIC);
    out.push(from.len() as u8);
    out.extend_from_slice(from);
    out.push(to.len() as u8);
    out.extend_from_slice(to);
    out.extend_from_slice(inner);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::mpsc as chan;

    /// CI runners can take several seconds for the QUIC handshakes plus cert
    /// generation before the first datagram lands, so keep this generous.
    async fn recv_one(rx: &mut chan::UnboundedReceiver<Vec<u8>>) -> Vec<u8> {
        tokio::time::timeout(Duration::from_secs(15), rx.recv())
            .await
            .expect("timeout waiting for msg")
            .expect("channel closed")
    }

    #[tokio::test]
    async fn quic_relay_forwards_pair_both_ways() {
        let server = QuicRelayServer::spawn("127.0.0.1:0".parse().unwrap()).expect("relay bind");
        let (txa, mut rxa) = chan::unbounded_channel::<Vec<u8>>();
        let (txb, mut rxb) = chan::unbounded_channel::<Vec<u8>>();

        let a = Arc::new(
            QuicRelayClient::dial(server.local_addr)
                .await
                .expect("dial a"),
        );
        let b = Arc::new(
            QuicRelayClient::dial(server.local_addr)
                .await
                .expect("dial b"),
        );
        let a_recv = Arc::clone(&a);
        let b_recv = Arc::clone(&b);
        let txa_clone = txa.clone();
        let txb_clone = txb.clone();
        let a_recv_handle = tokio::spawn(async move {
            let _ = a_recv
                .recv_loop(move |f| {
                    let _ = txa_clone.send(f);
                })
                .await;
        });
        let b_recv_handle = tokio::spawn(async move {
            let _ = b_recv
                .recv_loop(move |f| {
                    let _ = txb_clone.send(f);
                })
                .await;
        });

        // The two peers dial independent connections, so the server's arrival
        // order for these first two datagrams is not deterministic. Exactly
        // one of them is dropped as the learning side and the other one
        // completes the pair and is forwarded to the peer that registered
        // first. Accept either order, but check the payload matches whoever
        // actually sent it.
        a.send(&build_datagram(b"a", b"b", b"hello-a"))
            .await
            .unwrap();
        b.send(&build_datagram(b"b", b"a", b"hello-b"))
            .await
            .unwrap();
        tokio::select! {
            f = recv_one(&mut rxa) => {
                assert_eq!(f, build_datagram(b"b", b"a", b"hello-b"), "A must receive B's datagram");
            }
            f = recv_one(&mut rxb) => {
                assert_eq!(f, build_datagram(b"a", b"b", b"hello-a"), "B must receive A's datagram");
            }
        }

        // Pair is now registered on both sides: forwarding works both ways
        // deterministically.
        a.send(&build_datagram(b"a", b"b", b"hello-a2"))
            .await
            .unwrap();
        assert_eq!(
            recv_one(&mut rxb).await,
            build_datagram(b"a", b"b", b"hello-a2")
        );
        b.send(&build_datagram(b"b", b"a", b"hello-b2"))
            .await
            .unwrap();
        assert_eq!(
            recv_one(&mut rxa).await,
            build_datagram(b"b", b"a", b"hello-b2")
        );

        assert!(server.stats.forwarded.load(Ordering::Relaxed) >= 2);
        assert!(server.stats.dropped_learning.load(Ordering::Relaxed) >= 1);
        a_recv_handle.abort();
        b_recv_handle.abort();
        server.task.abort();
    }

    #[tokio::test]
    async fn quic_relay_isolates_pairs() {
        let server = QuicRelayServer::spawn("127.0.0.1:0".parse().unwrap()).expect("relay bind");
        let (txc, mut rxc) = chan::unbounded_channel::<Vec<u8>>();
        let (txd, mut rxd) = chan::unbounded_channel::<Vec<u8>>();

        let c = Arc::new(
            QuicRelayClient::dial(server.local_addr)
                .await
                .expect("dial c"),
        );
        let d = Arc::new(
            QuicRelayClient::dial(server.local_addr)
                .await
                .expect("dial d"),
        );
        let c_recv = Arc::clone(&c);
        let d_recv = Arc::clone(&d);
        let txc_clone = txc.clone();
        let txd_clone = txd.clone();
        let tc = tokio::spawn(async move {
            let _ = c_recv
                .recv_loop(move |f| {
                    let _ = txc_clone.send(f);
                })
                .await;
        });
        let td = tokio::spawn(async move {
            let _ = d_recv
                .recv_loop(move |f| {
                    let _ = txd_clone.send(f);
                })
                .await;
        });

        // C and D complete a pair with no A/B traffic present. As above, the
        // arrival order between their two independent connections decides
        // which learning datagram is dropped; accept either, then prove the
        // established pair forwards both ways inside its own mapping.
        c.send(&build_datagram(b"c", b"d", b"one")).await.unwrap();
        d.send(&build_datagram(b"d", b"c", b"two")).await.unwrap();
        tokio::select! {
            f = recv_one(&mut rxc) => {
                assert_eq!(f, build_datagram(b"d", b"c", b"two"), "C must receive D's datagram");
            }
            f = recv_one(&mut rxd) => {
                assert_eq!(f, build_datagram(b"c", b"d", b"one"), "D must receive C's datagram");
            }
        }
        c.send(&build_datagram(b"c", b"d", b"three")).await.unwrap();
        assert_eq!(
            recv_one(&mut rxd).await,
            build_datagram(b"c", b"d", b"three")
        );
        d.send(&build_datagram(b"d", b"c", b"four")).await.unwrap();
        assert_eq!(
            recv_one(&mut rxc).await,
            build_datagram(b"d", b"c", b"four")
        );

        tc.abort();
        td.abort();
        server.task.abort();
    }
}
