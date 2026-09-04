//! cairn-p2p — the swarm transport layer (ADR-0017).
//!
//! Direct, encrypted, peer-to-peer block sync between cairn devices, layered
//! ON TOP of the cloud control plane (never replacing it):
//!
//! 1. **Rendezvous** ([`signal`]): a lightweight UDP directory. Nodes drop off a
//!    "business card" (identity + public key + candidate addresses, HMAC-bound
//!    to the cluster key) and receive the other members' cards. The server never
//!    sees media bytes — only addressing metadata.
//!    Admission is **join-code gated** ([`join`]): the host generates (or picks)
//!    a 144-bit join code, shares it with the people who may join, and everyone
//!    else is dropped silently — they never enter the member list, so no peer
//!    will ever establish a session with them.
//! 2. **NAT traversal** ([`stun`] + swarm punch loop): the signal server swaps
//!    observed public addresses; both peers fire simultaneous probe datagrams
//!    ("punching"), which opens mappings in home-router NATs so a direct link
//!    can establish. STUN (RFC 5389 binding) discovers the reflexive public
//!    address beforehand.
//! 3. **Relay fallback** ([`relay`]): when a strict firewall eats every punch
//!    (corporate nets), traffic temporarily routes through an encrypted
//!    pass-through relay. The relay forwards opaque authenticated ciphertext —
//!    it can route but never read block content.
//! 4. **Sessions** ([`session`]): every datagram after the public-key bootstrap
//!    HELLO is XChaCha20-Poly1305 sealed under keys derived from an X25519
//!    agreement over the two node identities. Blocks stream as
//!    `META → CHUNK[0..n] → EOF` fragments with NAK-based retransmission.
//! 5. **Swarm** ([`swarm`]): manifest-style block-hash exchange. Each node
//!    advertises a Bloom of its owned chunk hashes (HAVE); wants are scheduled
//!    rarest-first with load balancing across holders; completed blocks are
//!    re-announced so later joiners pull from MANY holders, not one — the
//!    BitTorrent mesh effect (download speed rises as nodes join).
//!
//! Every fetched block is BLAKE3-verified before it is handed to the caller
//! (invariant I2 — a corrupt or malicious peer can never poison the CAS).
//!
//! Zero new external code: `orion` (X25519, XChaCha20-Poly1305) was already in
//! the dependency tree via pasetors; `blake3`, `hmac`, `sha2`, `tokio` are
//! workspace staples (THIRD_PARTY.md).

#![forbid(unsafe_code)]

pub mod crypto;
pub mod join;
pub mod mdns;
pub mod relay;
pub mod session;
pub mod signal;
pub mod stun;
pub mod swarm;

pub use crypto::NodeKey;
pub use join::{JoinCode, JoinCodeError};
pub use swarm::{ServeBlocks, Swarm, SwarmConfig, SwarmStats};
