//! Peer session protocol (ADR-0017 §4): the datagram codec + framing + block
//! fragment reassembly shared by the swarm and (stripped) by the relay.
//!
//! Wire datagram shapes (first byte discriminates):
//!
//! ```text
//! 0x01 CLEAR_HELLO  [0x01][HELLO msg]                    — plaintext bootstrap:
//!                                                            node id + X25519 pub
//! 0x02 ENC          [0x02][ctr u64 LE][ciphertext+tag]    — XChaCha20-Poly1305
//! ```
//!
//! The encrypted payload decodes to one [`PeerMsg`]. Blocks stream as
//! `META(hash, len, frags) → CHUNK(hash, idx, data) × frags → EOF(hash)`;
//! the receiver NAKs missing indices (on gap-at-EOF or idle stall — UDP
//! receive-buffer overflow during a burst is the expected trigger), and the
//! sender re-serves from its local CAS. Chunk data per fragment is capped at
//! [`MAX_FRAG_DATA`] so every encrypted datagram stays MTU-safe (~1260B).
//!
//! Replay tolerance: every receiver-side effect is idempotent (fragment insert
//! dedups, HELLO re-triggers HAVE at most once per tick, WANT re-serves) —
//! an attacker replaying recorded ciphertext cannot corrupt state, only waste
//! bandwidth. Strict replay windows (reordered UDP would false-positive) are
//! deferred; the sender-side strictly-increasing counter guarantees nonce
//! uniqueness, which is the AEAD security contract.

use cairn_core::hash::Hash;

use crate::crypto::{NodeKey, SessionKeys};

// ---- frame tags -----------------------------------------------------------
pub(crate) const FRAME_CLEAR_HELLO: u8 = 0x01;
pub(crate) const FRAME_ENC: u8 = 0x02;

// ---- nonce direction bytes (crypto.rs binds roles the same way) -----------
pub(crate) const NONCE_DIR_LO_TO_HI: u8 = 0x01;
pub(crate) const NONCE_DIR_HI_TO_LO: u8 = 0x02;

// ---- message tags ---------------------------------------------------------
const MSG_HELLO: u8 = 0x48;
const MSG_HAVE: u8 = 0x56;
const MSG_WANT: u8 = 0x57;
const MSG_META: u8 = 0x4D;
const MSG_CHUNK: u8 = 0x43;
const MSG_EOF: u8 = 0x45;
const MSG_NAK: u8 = 0x4E;
const MSG_DENY: u8 = 0x44;
/// Live-presence telemetry (ADR-0023): opaque app JSON (playhead/drag/
/// selection), strictly bounded — this is a coordination channel, NOT a data
/// channel. Oversized payloads are refused at decode.
const MSG_PRESENCE: u8 = 0x50;

/// Max block-data bytes per CHUNK fragment (keeps encrypted datagrams ≈ MTU).
pub(crate) const MAX_FRAG_DATA: usize = 1200;
/// Absolute reassembly cap — a META announcing more is refused (memory guard).
pub(crate) const MAX_REASSEMBLE_BYTES: u64 = 64 * 1024 * 1024;
/// Datagram receive bound (anything larger is malformed/abusive).
pub(crate) const MAX_DATAGRAM: usize = 70_000;

/// One peer-protocol message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PeerMsg {
    /// Identity bootstrap (encrypted re-announcement; the clear form lives in
    /// the frame layer). Receiver answers with HAVE — handshake retries are
    /// bidirectionally self-healing.
    Hello { node_id: Vec<u8>, pubkey: [u8; 32] },
    /// Bloom (serialized cairn-core Bloom) of the hashes this peer owns.
    Have { bloom: Vec<u8>, items: u32 },
    /// Request a block.
    Want { hash: [u8; 32] },
    /// Block header: total byte length + fragment count.
    Meta {
        hash: [u8; 32],
        total_len: u32,
        frags: u16,
    },
    /// One block fragment.
    Chunk {
        hash: [u8; 32],
        idx: u16,
        data: Vec<u8>,
    },
    /// Block stream terminator.
    Eof { hash: [u8; 32] },
    /// Retransmit request for specific fragment indices.
    Nak { hash: [u8; 32], idxs: Vec<u16> },
    /// "I don't have it" — the want moves to another holder.
    Deny { hash: [u8; 32] },
    /// Ephemeral live-presence telemetry (ADR-0023 §2): an opaque, bounded
    /// app payload (editor name, playhead frame, rate, action). Rides the
    /// same encrypted session frames as block traffic — direct or relay —
    /// and is NEVER persisted, reassembled, or retried. Loss-tolerant by
    /// design (the next heartbeat supersedes).
    Presence { payload: Vec<u8> },
}

impl PeerMsg {
    /// Encode to the pre-encryption payload (deterministic).
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(64);
        self.encode_into(&mut out);
        out
    }

    /// Encode into a caller-owned scratch buffer (cleared first).
    ///
    /// Cloudflare's dns-cache-memory-optimization-1111 lesson: a buffer that
    /// persists across messages amortizes to zero steady-state reallocation —
    /// a fresh `Vec` per CHUNK grew 64→…→1233 bytes, paying up to five
    /// reallocs on the hot send path. `encode` keeps the one-shot shape for
    /// bootstrap/tests; `PeerSession::seal` reuses one scratch.
    pub(crate) fn encode_into(&self, out: &mut Vec<u8>) {
        out.clear();
        match self {
            PeerMsg::Hello { node_id, pubkey } => {
                out.push(MSG_HELLO);
                out.push(u8::try_from(node_id.len()).unwrap_or(u8::MAX));
                out.extend_from_slice(node_id);
                out.extend_from_slice(pubkey);
            }
            PeerMsg::Have { bloom, items } => {
                out.push(MSG_HAVE);
                out.extend_from_slice(&items.to_be_bytes());
                out.extend_from_slice(
                    &u16::try_from(bloom.len()).unwrap_or(u16::MAX).to_be_bytes(),
                );
                out.extend_from_slice(bloom);
            }
            PeerMsg::Want { hash } => {
                out.push(MSG_WANT);
                out.extend_from_slice(hash);
            }
            PeerMsg::Meta {
                hash,
                total_len,
                frags,
            } => {
                out.push(MSG_META);
                out.extend_from_slice(hash);
                out.extend_from_slice(&total_len.to_be_bytes());
                out.extend_from_slice(&frags.to_be_bytes());
            }
            PeerMsg::Chunk { hash, idx, data } => {
                out.push(MSG_CHUNK);
                out.extend_from_slice(hash);
                out.extend_from_slice(&idx.to_be_bytes());
                out.extend_from_slice(data);
            }
            PeerMsg::Eof { hash } => {
                out.push(MSG_EOF);
                out.extend_from_slice(hash);
            }
            PeerMsg::Nak { hash, idxs } => {
                out.push(MSG_NAK);
                out.extend_from_slice(hash);
                let cap = idxs.len().min(u16::MAX as usize);
                out.extend_from_slice(&u16::try_from(cap).unwrap_or(u16::MAX).to_be_bytes());
                for i in idxs.iter().take(cap) {
                    out.extend_from_slice(&i.to_be_bytes());
                }
            }
            PeerMsg::Deny { hash } => {
                out.push(MSG_DENY);
                out.extend_from_slice(hash);
            }
            PeerMsg::Presence { payload } => {
                out.push(MSG_PRESENCE);
                // len-checked at construction AND decode: presence is small
                // telemetry; a payload near MAX_FRAG_DATA is a bug or abuse
                out.extend_from_slice(
                    &u16::try_from(payload.len())
                        .unwrap_or(u16::MAX)
                        .to_be_bytes(),
                );
                out.extend_from_slice(payload);
            }
        }
    }

    /// Decode a pre-encryption payload.
    pub(crate) fn decode(payload: &[u8]) -> Option<Self> {
        let tag = *payload.first()?;
        let p = &payload[1..];
        match tag {
            MSG_HELLO => {
                let id_len = *p.first()? as usize;
                let id = p.get(1..1 + id_len)?.to_vec();
                let pubkey: [u8; 32] = p.get(1 + id_len..1 + id_len + 32)?.try_into().ok()?;
                Some(PeerMsg::Hello {
                    node_id: id,
                    pubkey,
                })
            }
            MSG_HAVE => {
                let items = u32::from_be_bytes(p.get(0..4)?.try_into().ok()?);
                let blen = u16::from_be_bytes(p.get(4..6)?.try_into().ok()?) as usize;
                let bloom = p.get(6..6 + blen)?.to_vec();
                if bloom.len() != blen {
                    return None;
                }
                Some(PeerMsg::Have { bloom, items })
            }
            MSG_WANT => Some(PeerMsg::Want {
                hash: p.get(0..32)?.try_into().ok()?,
            }),
            MSG_META => Some(PeerMsg::Meta {
                hash: p.get(0..32)?.try_into().ok()?,
                total_len: u32::from_be_bytes(p.get(32..36)?.try_into().ok()?),
                frags: u16::from_be_bytes(p.get(36..38)?.try_into().ok()?),
            }),
            MSG_CHUNK => Some(PeerMsg::Chunk {
                hash: p.get(0..32)?.try_into().ok()?,
                idx: u16::from_be_bytes(p.get(32..34)?.try_into().ok()?),
                data: p.get(34..)?.to_vec(),
            }),
            MSG_EOF => Some(PeerMsg::Eof {
                hash: p.get(0..32)?.try_into().ok()?,
            }),
            MSG_NAK => {
                let hash: [u8; 32] = p.get(0..32)?.try_into().ok()?;
                let cnt = u16::from_be_bytes(p.get(32..34)?.try_into().ok()?) as usize;
                let mut idxs = Vec::with_capacity(cnt);
                for i in 0..cnt {
                    idxs.push(u16::from_be_bytes(
                        p.get(34 + i * 2..36 + i * 2)?.try_into().ok()?,
                    ));
                }
                Some(PeerMsg::Nak { hash, idxs })
            }
            MSG_DENY => Some(PeerMsg::Deny {
                hash: p.get(0..32)?.try_into().ok()?,
            }),
            MSG_PRESENCE => {
                let plen = u16::from_be_bytes(p.get(0..2)?.try_into().ok()?) as usize;
                // bounded by MAX_FRAG_DATA: presence is telemetry, not a data
                // channel — a fat "presence" payload is refused outright
                if plen > MAX_FRAG_DATA {
                    return None;
                }
                let payload = p.get(2..2 + plen)?.to_vec();
                if payload.len() != plen {
                    return None;
                }
                Some(PeerMsg::Presence { payload })
            }
            _ => None,
        }
    }
}

/// A live session with one peer: keys + our send counter.
pub(crate) struct PeerSession {
    keys: SessionKeys,
    send_ctr: u64,
    /// Reusable plaintext scratch (ADR-0024): encodes clear in place; a fresh
    /// Vec per seal paid up to five growth reallocs for CHUNK payloads.
    pt_scratch: Vec<u8>,
    /// Reusable ciphertext scratch (pt.len()+16, exact — never grows).
    ct_scratch: Vec<u8>,
}

impl PeerSession {
    pub(crate) fn new(keys: SessionKeys) -> Self {
        PeerSession {
            keys,
            send_ctr: 0,
            pt_scratch: Vec::new(),
            ct_scratch: Vec::new(),
        }
    }

    /// Seal one message into an ENC datagram. Nonce = our direction byte +
    /// strictly-increasing counter (never reused with the same key).
    ///
    /// Allocation shape (ADR-0024, the Cloudflare scratch lesson): the
    /// plaintext and ciphertext live in per-session scratches that persist
    /// across messages (zero steady-state reallocs); the returned datagram
    /// is the ONE fresh allocation — it is handed to the socket.
    pub(crate) fn seal(&mut self, msg: &PeerMsg) -> Vec<u8> {
        msg.encode_into(&mut self.pt_scratch);
        let pt_len = self.pt_scratch.len();
        self.ct_scratch.clear();
        self.ct_scratch.resize(pt_len + 16, 0);
        let mut frame = Vec::with_capacity(9 + self.ct_scratch.len());
        frame.push(FRAME_ENC);
        frame.extend_from_slice(&self.send_ctr.to_le_bytes());
        let nonce = frame_nonce(self.keys.nonce_dir, self.send_ctr);
        let orion_nonce =
            orion::hazardous::stream::xchacha20::Nonce::from_slice(&nonce).expect("24-byte nonce");
        orion::hazardous::aead::xchacha20poly1305::seal(
            &self.keys.send,
            &orion_nonce,
            &self.pt_scratch,
            None,
            &mut self.ct_scratch,
        )
        .expect("AEAD seal with valid key/nonce cannot fail");
        frame.extend_from_slice(&self.ct_scratch);
        self.send_ctr += 1;
        frame
    }

    /// Open an ENC datagram (their direction). `None` = auth failure / garbage.
    pub(crate) fn open(&self, frame: &[u8]) -> Option<PeerMsg> {
        if frame.len() < 9 || frame[0] != FRAME_ENC {
            return None;
        }
        let ctr = u64::from_le_bytes(frame[1..9].try_into().ok()?);
        let ct = frame.get(9..)?;
        if ct.len() < 16 {
            return None;
        }
        let nonce = frame_nonce(self.keys.peer_nonce_dir(), ctr);
        let orion_nonce =
            orion::hazardous::stream::xchacha20::Nonce::from_slice(&nonce).expect("24-byte nonce");
        let mut pt = vec![0u8; ct.len() - 16];
        orion::hazardous::aead::xchacha20poly1305::open(
            &self.keys.recv,
            &orion_nonce,
            ct,
            None,
            &mut pt,
        )
        .ok()?;
        PeerMsg::decode(&pt)
    }
}

/// 24-byte nonce: [direction][ctr u64 LE][15 zero bytes].
fn frame_nonce(dir: u8, ctr: u64) -> [u8; 24] {
    let mut n = [0u8; 24];
    n[0] = dir;
    n[1..9].copy_from_slice(&ctr.to_le_bytes());
    n
}

/// Build the plaintext bootstrap HELLO datagram for `node` (frame layer).
pub(crate) fn build_clear_hello(node: &NodeKey) -> Vec<u8> {
    let msg = PeerMsg::Hello {
        node_id: node.node_id_bytes().to_vec(),
        pubkey: node.public_bytes(),
    };
    let mut out = vec![FRAME_CLEAR_HELLO];
    out.extend_from_slice(&msg.encode());
    out
}

/// Parse a plaintext bootstrap HELLO datagram → (node id, public key).
pub(crate) fn parse_clear_hello(datagram: &[u8]) -> Option<(Vec<u8>, [u8; 32])> {
    if datagram.first() != Some(&FRAME_CLEAR_HELLO) {
        return None;
    }
    match PeerMsg::decode(&datagram[1..])? {
        PeerMsg::Hello { node_id, pubkey } => Some((node_id, pubkey)),
        _ => None,
    }
}

/// Receiver-side fragment reassembly for one block stream.
#[derive(Debug)]
pub(crate) struct Reassembly {
    pub total_len: u32,
    pub frags: u16,
    got: Vec<Option<Vec<u8>>>,
    pub last_progress: std::time::Instant,
}

impl Reassembly {
    /// Begin a stream. `None` if the announced size violates the memory guard.
    pub(crate) fn start(hash: &[u8; 32], total_len: u32, frags: u16) -> Option<Self> {
        if total_len == 0 || frags == 0 {
            return None;
        }
        // consistency: frags × MAX_FRAG_DATA must cover total_len, and the
        // memory bound must hold (a lying META cannot wedge unbounded RAM)
        let declared = u64::from(frags) * MAX_FRAG_DATA as u64;
        if u64::from(total_len) > declared || u64::from(total_len) > MAX_REASSEMBLE_BYTES {
            tracing::warn!(
                hash = &Hash(*hash).hex()[..16],
                total_len,
                frags,
                "META failed the reassembly bounds check — stream refused"
            );
            return None;
        }
        Some(Reassembly {
            total_len,
            frags,
            got: vec![None; frags as usize],
            last_progress: std::time::Instant::now(),
        })
    }

    /// Insert a fragment. Returns `true` if it was new (progress).
    pub(crate) fn insert(&mut self, idx: u16, data: Vec<u8>) -> bool {
        let slot = self.got.get_mut(idx as usize);
        if let Some(slot @ None) = slot {
            *slot = Some(data);
            self.last_progress = std::time::Instant::now();
            true
        } else {
            false // duplicate or out-of-range idx
        }
    }

    /// Missing fragment indices (bounded to keep NAKs datagram-sized).
    pub(crate) fn missing(&self) -> Vec<u16> {
        let mut m = Vec::new();
        for (i, slot) in self.got.iter().enumerate() {
            if slot.is_none() {
                m.push(u16::try_from(i).unwrap_or(u16::MAX));
            }
        }
        m
    }

    /// Assemble when complete. `None` until every fragment is present and the
    /// total length matches the header.
    pub(crate) fn assemble(&self) -> Option<Vec<u8>> {
        let have_all = self.got.iter().all(|s| s.is_some());
        if !have_all {
            return None;
        }
        let mut out = Vec::with_capacity(self.total_len as usize);
        for slot in &self.got {
            out.extend_from_slice(slot.as_ref().expect("checked Some above"));
        }
        (out.len() == self.total_len as usize).then_some(out)
    }

    pub(crate) fn is_complete(&self) -> bool {
        self.assemble().is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::derive_session;

    fn node_pair() -> (NodeKey, NodeKey) {
        (
            NodeKey::from_secret("node-a", [1u8; 32]),
            NodeKey::from_secret("node-b", [2u8; 32]),
        )
    }

    fn sessions() -> (PeerSession, PeerSession) {
        let (a, b) = node_pair();
        let ka = derive_session(&a, b.node_id_bytes(), &b.public_bytes()).unwrap();
        let kb = derive_session(&b, a.node_id_bytes(), &a.public_bytes()).unwrap();
        (PeerSession::new(ka), PeerSession::new(kb))
    }

    #[test]
    fn codec_roundtrip_every_variant() {
        let msgs = vec![
            PeerMsg::Hello {
                node_id: b"editor-nairobi".to_vec(),
                pubkey: [7u8; 32],
            },
            PeerMsg::Have {
                bloom: vec![1, 2, 3, 255],
                items: 1234,
            },
            PeerMsg::Want { hash: [9u8; 32] },
            PeerMsg::Meta {
                hash: [9u8; 32],
                total_len: 4096,
                frags: 4,
            },
            PeerMsg::Chunk {
                hash: [9u8; 32],
                idx: 3,
                data: vec![0xAB; 1200],
            },
            PeerMsg::Eof { hash: [9u8; 32] },
            PeerMsg::Nak {
                hash: [9u8; 32],
                idxs: vec![0, 1, 2, 65535],
            },
            PeerMsg::Deny { hash: [9u8; 32] },
        ];
        for m in &msgs {
            let enc = m.encode();
            assert_eq!(&PeerMsg::decode(&enc).unwrap(), m, "round-trip {m:?}");
        }
    }

    #[test]
    fn codec_rejects_truncated_and_unknown() {
        assert!(PeerMsg::decode(&[]).is_none());
        assert!(PeerMsg::decode(&[0xFF]).is_none());
        assert!(PeerMsg::decode(&[MSG_WANT]).is_none()); // hash missing
        assert!(PeerMsg::decode(&[MSG_WANT, 1, 2, 3]).is_none());
    }

    #[test]
    fn sealed_frames_roundtrip_between_sessions() {
        let (mut sa, mut sb) = sessions();
        let m = PeerMsg::Want { hash: [5u8; 32] };
        let frame = sa.seal(&m);
        assert_eq!(frame[0], FRAME_ENC);
        assert_eq!(sb.open(&frame).unwrap(), m);

        // reverse direction
        let m2 = PeerMsg::Eof { hash: [5u8; 32] };
        let f2 = sb.seal(&m2);
        assert_eq!(sa.open(&f2).unwrap(), m2);
    }

    #[test]
    fn frames_from_wrong_session_fail_auth() {
        let (mut sa, _sb) = sessions();
        let (sc, _) = {
            let (a, b) = (
                NodeKey::from_secret("node-c", [3u8; 32]),
                NodeKey::from_secret("node-d", [4u8; 32]),
            );
            let kc = derive_session(&a, b.node_id_bytes(), &b.public_bytes()).unwrap();
            let kd = derive_session(&b, a.node_id_bytes(), &a.public_bytes()).unwrap();
            (PeerSession::new(kc), PeerSession::new(kd))
        };
        let frame = sa.seal(&PeerMsg::Want { hash: [5u8; 32] });
        assert!(sc.open(&frame).is_none(), "wrong pair must fail to open");
    }
    #[test]
    fn counters_advance_and_never_reuse_nonce() {
        let (mut sa, _sb) = sessions();
        let f1 = sa.seal(&PeerMsg::Eof { hash: [1u8; 32] });
        let f2 = sa.seal(&PeerMsg::Eof { hash: [1u8; 32] });
        let c1 = u64::from_le_bytes(f1[1..9].try_into().unwrap());
        let c2 = u64::from_le_bytes(f2[1..9].try_into().unwrap());
        assert_eq!(c2, c1 + 1, "send counter strictly increases");
    }

    #[test]
    fn clear_hello_roundtrip() {
        let (node, _) = node_pair();
        let dgram = build_clear_hello(&node);
        assert_eq!(dgram[0], FRAME_CLEAR_HELLO);
        let (id, pk) = parse_clear_hello(&dgram).unwrap();
        assert_eq!(id, node.node_id_bytes());
        assert_eq!(pk, node.public_bytes());
    }

    #[test]
    fn reassembly_out_of_order_and_duplicates() {
        let hash = [9u8; 32];
        let mut r = Reassembly::start(&hash, 3, 3).unwrap();
        assert!(r.insert(2, vec![3]));
        assert!(!r.insert(2, vec![3]), "duplicate is not progress");
        assert!(r.insert(0, vec![1]));
        assert!(r.assemble().is_none());
        assert_eq!(r.missing(), vec![1]);
        assert!(r.insert(1, vec![2]));
        assert_eq!(r.assemble().unwrap(), vec![1, 2, 3]);
    }

    #[test]
    fn reassembly_rejects_lying_meta() {
        // total_len > frags × MAX_FRAG_DATA → refuse
        assert!(Reassembly::start(
            &[1u8; 32],
            u32::try_from(MAX_FRAG_DATA as u64 + 1).unwrap(),
            1
        )
        .is_none());
        // zero-sized streams refused
        assert!(Reassembly::start(&[1u8; 32], 0, 1).is_none());
        assert!(Reassembly::start(&[1u8; 32], 10, 0).is_none());
        // over the absolute memory guard
        assert!(Reassembly::start(
            &[1u8; 32],
            u32::try_from(MAX_REASSEMBLE_BYTES + 1).unwrap(),
            u16::MAX
        )
        .is_none());
    }

    #[test]
    fn assembled_length_mismatch_refused() {
        // frags cover the announced total but fragments sum differently
        let mut r = Reassembly::start(&[7u8; 32], 10, 2).unwrap();
        r.insert(0, vec![1; 5]);
        r.insert(1, vec![1; 4]); // sums to 9 ≠ 10
        assert!(r.assemble().is_none(), "short assemble must be refused");
    }
}
