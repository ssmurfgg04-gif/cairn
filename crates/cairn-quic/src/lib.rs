//! Cairn QUIC swarm transport (priority #2, WAN resilience).
//!
//! Why QUIC instead of raw TCP/UDP hole-punch for the data plane:
//! - No head-of-line blocking: one lost packet stalls only its own stream,
//!   not every chunk behind it (TCP stalls all).
//! - Connection migration is native: Wi-Fi -> Ethernet mid-sync keeps the
//!   session (same connection ID, new path validated in 1 RTT).
//! - 0/1-RTT handshakes + TLS 1.3 always on: no separate DTLS layer.
//! - Datagrams (unreliable) available for presence heartbeats later.
//!
//! The signaling/rendezvous stays as-is (join-code gated); QUIC carries
//! chunk bytes once peers know each other's addresses. TURN relay remains
//! the fallback where UDP is fully blocked.

#![forbid(unsafe_code)]

pub mod endpoint;
pub mod relay;
pub mod transport;
pub mod trust;

pub use endpoint::{QuicEndpoint, ServerIdentity};
pub use relay::{QuicRelayClient, QuicRelayServer, RelayStats as QuicRelayStats};
pub use transport::{ChunkFrame, QuicTransport};

pub fn ensure_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}
