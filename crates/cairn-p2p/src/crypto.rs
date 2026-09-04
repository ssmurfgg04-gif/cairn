//! Node identity and session-key derivation (ADR-0017 §3).
//!
//! A node's long-term transport identity is an X25519 key pair. Public keys
//! travel in the clear inside the bootstrap HELLO (and, HMAC-covered, via the
//! signal server) — they are public by design. Session keys for a pair are
//! derived with BLAKE3's official `derive_key` KDF mode over:
//!
//! ```text
//! IKM  = X25519(my_sk, peer_pk) || id_lo || id_hi     (ids sorted — both
//!                                                       sides derive the same IKM)
//! K_lo = blake3::derive_key("cairn-p2p-session/lo", IKM)
//! K_hi = blake3::derive_key("cairn-p2p-session/hi", IKM)
//! ```
//!
//! The lexicographically-lower node id seals with `K_lo` and opens with `K_hi`;
//! the higher node does the mirror. Role binding lives in the KDF context, so
//! a mirrored derivation can never produce a cross-direction key collision.
//! Nonces are 24 bytes: `[direction byte][u64 LE counter][15 zero bytes]` —
//! the counter is strictly increasing per sender, which is the nonce-uniqueness
//! contract XChaCha20-Poly1305 requires.

use std::cmp::Ordering;

use orion::errors::UnknownCryptoError;
use orion::hazardous::aead::xchacha20poly1305::SecretKey;
use orion::hazardous::ecc::x25519;

use crate::session::{NONCE_DIR_HI_TO_LO, NONCE_DIR_LO_TO_HI};

const KDF_CONTEXT_LO: &str = "cairn-p2p-session/lo";
const KDF_CONTEXT_HI: &str = "cairn-p2p-session/hi";

/// A node's transport identity: node id + X25519 key pair.
/// (orion secret types are deliberately non-`Clone` — this type moves, never copies.)
pub struct NodeKey {
    node_id: Vec<u8>,
    sk: x25519::PrivateKey,
    pk: [u8; 32],
}

impl std::fmt::Debug for NodeKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // never render private key material in logs (finish_non_exhaustive:
        // the skipped `sk` field is deliberate, and says so)
        f.debug_struct("NodeKey")
            .field("node_id", &String::from_utf8_lossy(&self.node_id))
            .field("pk", &"<pub>")
            .finish_non_exhaustive()
    }
}

impl NodeKey {
    /// Generate a fresh random identity for `node_id` (CSPRNG-backed).
    #[must_use]
    pub fn generate(node_id: &str) -> Self {
        let sk = x25519::PrivateKey::generate();
        let pk = x25519::PublicKey::try_from(&sk)
            .expect("valid private key always derives a public key")
            .to_bytes();
        NodeKey {
            node_id: node_id.as_bytes().to_vec(),
            sk,
            pk,
        }
    }

    /// Deterministic identity from a fixed secret (tests only — never production).
    #[must_use]
    pub fn from_secret(node_id: &str, secret: [u8; 32]) -> Self {
        // clamp-free: orion applies X25519 clamping internally during agreement
        let sk = x25519::PrivateKey::from_slice(&secret).expect("32-byte secret");
        let pk = x25519::PublicKey::try_from(&sk)
            .expect("valid private key always derives a public key")
            .to_bytes();
        NodeKey {
            node_id: node_id.as_bytes().to_vec(),
            sk,
            pk,
        }
    }

    #[must_use]
    pub fn node_id(&self) -> &str {
        std::str::from_utf8(&self.node_id).unwrap_or("<bad-utf8>")
    }

    /// Raw node-id bytes (wire format).
    #[must_use]
    pub fn node_id_bytes(&self) -> &[u8] {
        &self.node_id
    }

    /// X25519 public key bytes — safe to publish.
    #[must_use]
    pub fn public_bytes(&self) -> [u8; 32] {
        self.pk
    }
}

/// Per-pair session keys + which direction this node seals in.
pub(crate) struct SessionKeys {
    /// Key this node seals with.
    pub(crate) send: SecretKey,
    /// Key this node opens with (peer's send key).
    pub(crate) recv: SecretKey,
    /// Nonce direction byte for our sealed frames.
    pub(crate) nonce_dir: u8,
}

impl SessionKeys {
    /// Open-direction nonce byte (the peer's sealing direction).
    pub(crate) fn peer_nonce_dir(&self) -> u8 {
        if self.nonce_dir == NONCE_DIR_LO_TO_HI {
            NONCE_DIR_HI_TO_LO
        } else {
            NONCE_DIR_LO_TO_HI
        }
    }
}

/// Derive the session keys for the pair `(my node, peer)`.
///
/// # Errors
/// - `UnknownCryptoError` if the peer public key is malformed (wrong length) or
///   the agreement fails (e.g. all-zero output — low-order point).
pub(crate) fn derive_session(
    my: &NodeKey,
    peer_id: &[u8],
    peer_pk: &[u8; 32],
) -> Result<SessionKeys, UnknownCryptoError> {
    let peer_pub = x25519::PublicKey::from_slice(peer_pk)?;
    let shared = x25519::key_agreement(&my.sk, &peer_pub)?;
    let (lo, hi) = order_ids(my.node_id_bytes(), peer_id);

    // IKM binds the shared secret to the ordered pair identity: both sides
    // construct identical bytes, and no third pair reuses the derivation.
    let mut ikm = Vec::with_capacity(32 + lo.len() + hi.len());
    ikm.extend_from_slice(shared.unprotected_as_bytes());
    ikm.extend_from_slice(lo);
    ikm.extend_from_slice(hi);

    let k_lo = blake3::derive_key(KDF_CONTEXT_LO, &ikm);
    let k_hi = blake3::derive_key(KDF_CONTEXT_HI, &ikm);

    let (send, recv, nonce_dir) = if my.node_id_bytes() == lo {
        (
            SecretKey::from_slice(&k_lo).expect("32 bytes"),
            SecretKey::from_slice(&k_hi).expect("32 bytes"),
            NONCE_DIR_LO_TO_HI,
        )
    } else {
        (
            SecretKey::from_slice(&k_hi).expect("32 bytes"),
            SecretKey::from_slice(&k_lo).expect("32 bytes"),
            NONCE_DIR_HI_TO_LO,
        )
    };
    Ok(SessionKeys {
        send,
        recv,
        nonce_dir,
    })
}

/// Deterministic pair ordering: byte-lexicographic, ties broken by length.
fn order_ids<'a>(a: &'a [u8], b: &'a [u8]) -> (&'a [u8], &'a [u8]) {
    match a.cmp(b) {
        Ordering::Less | Ordering::Equal => (a, b),
        Ordering::Greater => (b, a),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_sides_derive_mirror_keys() {
        let a = NodeKey::from_secret("node-a", [7u8; 32]);
        let b = NodeKey::from_secret("node-b", [9u8; 32]);
        let ka = derive_session(&a, b.node_id_bytes(), &b.public_bytes()).unwrap();
        let kb = derive_session(&b, a.node_id_bytes(), &a.public_bytes()).unwrap();

        // a seals with the key b opens with, and vice versa — round-trip proof
        let (msg, nonce) = (b"ping".to_vec(), [0u8; 24]);
        let n_a = orion::hazardous::stream::xchacha20::Nonce::from_slice(&nonce).unwrap();
        let n_b = orion::hazardous::stream::xchacha20::Nonce::from_slice(&nonce).unwrap();
        let mut ct = vec![0u8; msg.len() + 16];
        orion::hazardous::aead::xchacha20poly1305::seal(&ka.send, &n_a, &msg, None, &mut ct)
            .unwrap();
        let mut pt = vec![0u8; ct.len() - 16];
        orion::hazardous::aead::xchacha20poly1305::open(&kb.recv, &n_b, &ct, None, &mut pt)
            .unwrap();
        assert_eq!(pt, msg);

        // direction bytes are mirrored
        assert_eq!(ka.nonce_dir, crate::session::NONCE_DIR_LO_TO_HI);
        assert_eq!(kb.nonce_dir, crate::session::NONCE_DIR_HI_TO_LO);
        assert_eq!(ka.peer_nonce_dir(), kb.nonce_dir);
        assert_eq!(kb.peer_nonce_dir(), ka.nonce_dir);
    }

    #[test]
    fn identical_ids_still_derive_consistently() {
        // pathological pair (same id): lo == hi; the lower-id branch is taken by
        // byte-equality — both sides must agree on the SAME branch
        let a = NodeKey::from_secret("same", [1u8; 32]);
        let b = NodeKey::from_secret("same", [2u8; 32]);
        let ka = derive_session(&a, b.node_id_bytes(), &b.public_bytes()).unwrap();
        let kb = derive_session(&b, a.node_id_bytes(), &a.public_bytes()).unwrap();
        assert_eq!(ka.nonce_dir, kb.nonce_dir, "equal ids pick one branch");
    }

    #[test]
    fn tampered_ciphertext_fails_to_open() {
        let a = NodeKey::from_secret("node-a", [7u8; 32]);
        let b = NodeKey::from_secret("node-b", [9u8; 32]);
        let ka = derive_session(&a, b.node_id_bytes(), &b.public_bytes()).unwrap();
        let kb = derive_session(&b, a.node_id_bytes(), &a.public_bytes()).unwrap();

        let mut ct = vec![0u8; 4 + 16];
        let nonce = orion::hazardous::stream::xchacha20::Nonce::from_slice(&[0u8; 24]).unwrap();
        orion::hazardous::aead::xchacha20poly1305::seal(&ka.send, &nonce, b"ping", None, &mut ct)
            .unwrap();
        ct[0] ^= 0x40; // flip one bit
        let mut pt = vec![0u8; 4];
        assert!(
            orion::hazardous::aead::xchacha20poly1305::open(&kb.recv, &nonce, &ct, None, &mut pt)
                .is_err(),
            "bit-flipped ciphertext must fail authentication"
        );
    }

    #[test]
    fn different_peer_keys_derive_different_sessions() {
        // wrong-key confusion resistance: session keys must not collide just
        // because ids match — the X25519 output feeds the KDF
        let a = NodeKey::generate("node-a");
        let b1 = NodeKey::generate("node-b");
        let b2 = NodeKey::generate("node-b");
        let k1 = derive_session(&a, b1.node_id_bytes(), &b1.public_bytes()).unwrap();
        let k2 = derive_session(&a, b2.node_id_bytes(), &b2.public_bytes()).unwrap();
        // seal with k1's send key must FAIL to open under k2's recv key
        let nonce = orion::hazardous::stream::xchacha20::Nonce::from_slice(&[0u8; 24]).unwrap();
        let mut ct = vec![0u8; 8 + 16];
        orion::hazardous::aead::xchacha20poly1305::seal(
            &k1.send,
            &nonce,
            b"payload!",
            None,
            &mut ct,
        )
        .unwrap();
        let mut pt = vec![0u8; 8];
        assert!(
            orion::hazardous::aead::xchacha20poly1305::open(&k2.recv, &nonce, &ct, None, &mut pt)
                .is_err(),
            "sessions with different peer keys must not cross-decrypt"
        );
    }
}
