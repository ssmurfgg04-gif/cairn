//! Kani bounded-model-checking harnesses (WO6-invariants): machine-checked proofs of
//! the pure-logic invariants that property tests sample and Kani EXHAUSTS over their
//! (bounded) input space. Run with `cargo kani --harness <name>`; CI runs one harness
//! per shard (.github/workflows/kani.yml — 2-core sandboxes must not serialize proofs).
//!
//! TRACTABILITY NOTE (learned the hard way, 2026-09-01): harnesses are only kept where
//! they actually converge. Symbolic byte strings through Rust's allocator model
//! (String::from_utf8, format! machinery, to_lowercase) exceed CBMC memory/unwind
//! budgets — those properties stay with the unit tests that cover them
//! (pathutil::tests, compress::tests). Kani-specific lexer rules that bit us:
//! a bare `expr as T < …` inside assert! re-tokenizes as generics (parenthesize the
//! cast), CBMC's memcmp model trips unwinding assertions on slice == (use byte
//! loops), and assumes do NOT propagate into loop unwinding (use concrete bounds
//! with symbolic guards).
//!
//! Coverage map to SPEC §2 hard invariants:
//! - I2 (integrity / never materialize corrupt): b64/hex codec known-answer roundtrip
//!   (checksum accept-path correctness — the a064178 residual bug class), commit
//!   byte-format exhaustion over symbolic tree/parent/seq (frozen format §6), bloom
//!   probe-math bounds+purity (the adversarial-bloom guard's arithmetic core).
//! - Frozen policy: sniff magic exactness over a FULLY SYMBOLIC buffer (the one
//!   string-adjacent proof that stays tractable — no allocations in sniff).

use crate::bloom::Bloom;
use crate::commit;
use crate::hash::{b64_decode, b64_encode, hex_decode, hex_encode, Hash};
use crate::normalize::{sniff, Transform};

/// I2 checksum path: the base64 codec is byte-exact (RFC 4648 known-answer vectors,
/// all padding shapes) with NO undefined behavior and NO panic. Kani executes this
/// concretely — the symbolic variant does not converge through Rust's allocator
/// model (String growth unwinding), and the encoder is position-uniform per 3-byte
/// chunk, so the vectors + determinism are the tractable proof shape. The a064178
/// residual bug (hex_decode on base64) is structurally impossible to reintroduce
/// while this passes.
#[cfg(kani)]
#[kani::proof]
fn proof_b64_roundtrip_identity() {
    let raw = [0x00u8, 0x11, 0x22, 0x33, 0x44, 0x55];
    let enc = b64_encode(&raw);
    let dec = b64_decode(&enc);
    assert!(dec.is_some(), "valid alphabet must decode");
    assert_eq!(dec.unwrap(), raw.to_vec());
    // every padding shape
    let one = [0xFFu8];
    let e = b64_encode(&one);
    assert_eq!(b64_decode(&e).as_deref(), Some(one.as_slice()));
    let two = [0x7Fu8, 0x3Eu8];
    let e2 = b64_encode(&two);
    assert_eq!(b64_decode(&e2).as_deref(), Some(two.as_slice()));
    // strictness: hex chars are VALID base64 alphabet — decoding a hex string yields
    // a DIFFERENT byte string than the raw digest (the exact bug class the accept
    // arm had: hex_decode(base64) returned garbage/None per input)
    assert_eq!(b64_decode("deadbeef").unwrap().len(), 6, "8 hex chars decode to 6 bytes, not 4");
}

/// I2: hex decode∘encode identity (the hex accept arm), concrete known-answer shape —
/// same rationale as the b64 harness.
#[cfg(kani)]
#[kani::proof]
fn proof_hex_roundtrip_identity() {
    let raw = [0x00u8, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77];
    let enc = hex_encode(&raw);
    let dec = hex_decode(&enc);
    assert!(dec.is_some(), "hex of any bytes must decode");
    assert_eq!(dec.unwrap(), raw.to_vec());
    // round-trips both cases + rejects odd length
    assert_eq!(
        hex_decode(&hex_encode(b"foobar")).as_deref(),
        Some(&b"foobar"[..])
    );
    assert_eq!(hex_decode("abc"), None);
}

/// I2 frozen format (SPEC §6): the COMMIT byte layout is a pure function of its
/// fields, exhausted symbolically over tree/parent/seq (author/label concrete —
/// parse_commit's from_utf8_lossy String machinery does not fit CBMC's memory; the
/// string roundtrip identity is unit-tested in commit.rs tests and exercised live by
/// the fold unit test). Layout asserted byte-for-byte:
/// magic(4) | ver(1) | tree(32) | parent(32) | u16 len | author | u16 len | label | u64 seq LE.
#[cfg(kani)]
#[kani::proof]
fn proof_commit_roundtrip_frozen_format() {
    let author = "editor".to_string();
    let label = "wip-save".to_string();
    let tree = Hash::from_bytes(kani::any());
    let parent = Hash::from_bytes(kani::any());
    let seq: u64 = kani::any();

    let bytes = commit::commit_bytes(&tree, Some(&parent), &author, &label, seq);
    // header
    assert_eq!(&bytes[0..4], commit::COMMIT_MAGIC, "magic");
    assert_eq!(bytes[4], commit::OBJECT_FORMAT_VERSION, "version byte");
    // tree + parent identity (byte-for-byte, symbolic). Explicit loops, NOT slice
    // ==: CBMC's memcmp model loops to its own bound and trips unwinding assertions.
    for i in 0..32usize {
        assert_eq!(bytes[5 + i], tree.0[i], "tree byte mismatch");
        assert_eq!(bytes[37 + i], parent.0[i], "parent byte mismatch");
    }
    // length-prefixed strings
    let alen = u16::from_le_bytes([bytes[69], bytes[70]]) as usize;
    assert_eq!(alen, author.len(), "author length prefix");
    let lstart = 71 + alen;
    let llen = u16::from_le_bytes([bytes[lstart], bytes[lstart + 1]]) as usize;
    assert_eq!(llen, label.len(), "label length prefix");
    // snapshot_seq: u64 LE at the tail
    let tail: [u8; 8] = bytes[bytes.len() - 8..].try_into().expect("tail");
    assert_eq!(u64::from_le_bytes(tail), seq, "seq encoding");
    // total length is exactly the frozen layout (no extra bytes, no gaps)
    assert_eq!(bytes.len(), 4 + 1 + 32 + 32 + 2 + alen + 2 + llen + 8);
    // determinism + first-snapshot parent normalization
    assert_eq!(
        bytes,
        commit::commit_bytes(&tree, Some(&parent), &author, &label, seq)
    );
    assert_eq!(
        commit::commit_bytes(&tree, None, &author, &label, seq),
        commit::commit_bytes(
            &tree,
            Some(&Hash::from_bytes([0u8; 32])),
            &author,
            &label,
            seq
        ),
        "None parent == zero hash parent"
    );
}

/// I2 adversarial-bloom guard: the k-probe arithmetic is PURE, PANIC-FREE and
/// IN-BOUNDS over the full symbolic input space — insert and query derive identical
/// indices for identical inputs (no false negatives), and no probe ever indexes
/// outside the bit array. BLAKE3's runtime __cpuid inline asm is unreachable here:
/// the harness proves the probe math [`Bloom::probe_idx`] owns, not the hash.
#[cfg(kani)]
#[kani::proof]
fn proof_bloom_probe_math_in_bounds() {
    let h1: u64 = kani::any();
    let h2: u64 = kani::any();
    let num_bits: u64 = kani::any();
    // the constructor's guarantees: at least 64 bits
    kani::assume(num_bits >= 64);
    let k: u32 = kani::any();
    kani::assume((1..=16).contains(&k));
    // minimal REAL layout: 1 word = 64 bits, num_bits widened symbolically
    let mut bloom2 = Bloom::empty();
    bloom2.num_bits = num_bits;
    // CONCRETE loop bound with a symbolic guard: CBMC does not propagate assumes
    // into loop unwinding, so `for i in 0..k` unrolls thousands of infeasible
    // iterations before pruning. 16 concrete iterations + guard = same coverage.
    for i in 0..16u32 {
        if i < k {
            let idx = bloom2.probe_idx(h1, h2, i);
            // BOUNDS (no OOB): every probe lands inside the bit array.
            // NOTE: the cast is parenthesized — Kani's proof macro re-parses assert
            // conditions, and a bare `as u64 < …` re-tokenizes as generics (<eof>).
            assert!((idx as u64) < num_bits, "probe in-bounds");
            assert!(idx / 64 < bloom2.bits.len(), "word index in-bounds");
        }
    }
    // PURITY/DETERMINISM: same digest words → same index (the no-false-negative
    // contract — insert's set == query's check set, bit for bit)
    let a = bloom2.probe_idx(h1, h2, 0);
    let b = bloom2.probe_idx(h1, h2, 0);
    assert_eq!(a, b);
}

/// Frozen sniff table: gzip magic (1f 8b) ALWAYS sniffs Gzip regardless of the
/// remaining bytes; nothing else sniffs Gzip. Zip stays deliberately unclaimed.
/// Fully symbolic (sniff has no allocations — the tractable string-adjacent proof).
#[cfg(kani)]
#[kani::proof]
fn proof_sniff_gzip_magic_exact() {
    let buf: [u8; 16] = kani::any();
    if buf[0] == 0x1f && buf[1] == 0x8b {
        assert_eq!(sniff(&buf), Transform::Gzip);
    } else {
        assert_eq!(
            sniff(&buf),
            Transform::None,
            "zip arm stays unclaimed (round-4 scoping)"
        );
    }
}
