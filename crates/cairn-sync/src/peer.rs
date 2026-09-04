//! PeerSource — the swarm seam in hydration (ADR-0017 §7).
//!
//! Hydration consults peers BEFORE the cloud plane: LAN/direct swarm links
//! are typically 10-100× faster than object-store GETs, and they cost the
//! cloud nothing. `None` always means "try the plane instead" — a peer that
//! merely doesn't hold the block must answer `None` QUICKLY (the swarm's
//! `may_have` pre-check handles that; see the adapter in cairn-cli).

use cairn_core::hash::Hash;

/// Read-side swarm access for hydration.
#[async_trait::async_trait]
pub trait PeerSource: Send + Sync {
    /// Does any connected peer claim (bloom: maybe) to hold this hash?
    /// MUST be fast (no network round-trip) — it gates the peer path.
    fn peer_may_have(&self, h: &Hash) -> bool;

    /// Fetch one RAW block from the peer network (hash-verified by the
    /// swarm before it is returned — I2 holds across the trust boundary).
    /// `None` = not available from peers within the caller's budget.
    async fn fetch_peer_block(&self, h: &Hash) -> Option<Vec<u8>>;

    /// Tell the peer network which hashes we are about to need (the warm
    /// pre-walk: parallel scheduling starts before the sequential loop).
    async fn warm_blocks(&self, hashes: &[Hash]);
}
