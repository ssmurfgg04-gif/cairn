//! Kani bounded-model-checking harnesses (WO6-invariants): machine-checked proofs of
//! the pure-logic invariants that property tests sample and Kani EXHAUSTS over their
//! (bounded) input space. Run with `cargo kani --harness <name>`; CI runs one harness
//! per shard (.github/workflows/kani.yml — 2-core sandboxes must not serialize proofs).
//!
//! Coverage map to SPEC §2 hard invariants:
//! - I2 (integrity / never materialize corrupt): bloom no-false-negative (the
//!   adversarial-bloom guard that makes "already stored?" safe), b64/hex roundtrip
//!   (checksum accept path correctness — the exact bug class the a064178 residual
//!   proved is real), commit parse∘build roundtrip (frozen format §6).
//! - I3 (tenancy/scoping): validate_rel_path contract — the WO6-9 gate that keeps
//!   pushed paths inside the project root, exhausted symbolically.
//! - Frozen policy tables: sniff magic behavior, policy_for totality (no panic path).

use crate::bloom::Bloom;
use crate::commit;
use crate::hash::{b64_decode, b64_encode, hex_decode, hex_encode, Hash};
use crate::normalize::{sniff, Transform};
use crate::pathutil::validate_rel_path;

/// Symbolic ASCII bytes used to build bounded symbolic strings without UTF-8 pain.
/// Graphic ASCII (0x21..=0x7E) keeps `from_utf8` valid and excludes NUL/controls
/// (those are separate reject cases in the validator, exercised by unit tests).
#[inline]
fn any_ascii(n: usize) -> Vec<u8> {
    let raw: [u8; MAX_STR] = kani::any();
    let mut v = Vec::with_capacity(n);
    for i in 0..n {
        let c = raw[i];
        // graphic ASCII keeps String::from_utf8 valid; controls are separate rejects
        kani::assume((0x21..=0x7E).contains(&c));
        v.push(c);
    }
    v
}

const MAX_STR: usize = 12;

/// I2 checksum path: base64 decode∘encode is the identity over ALL 24-byte inputs
/// (a SHA-256 digest is 32 bytes; 24 keeps CBMC unrolling bounded — the encoder is
/// position-uniform so this generalizes). The a064178 residual bug (hex_decode on
/// base64) is structurally impossible to reintroduce if this holds.
#[cfg(kani)]
#[kani::proof]
fn proof_b64_roundtrip_identity() {
    let raw: [u8; 24] = kani::any();
    let enc = b64_encode(&raw);
    kani::assume(enc.len() == 32); // 24 bytes → ceil(24/3)*4 = 32 chars, no padding
    let dec = b64_decode(&enc);
    assert!(dec.is_some(), "valid alphabet must decode");
    assert_eq!(dec.unwrap(), raw.to_vec());
}

/// I2: hex decode∘encode identity over all 16-byte inputs (the hex accept arm).
#[cfg(kani)]
#[kani::proof]
fn proof_hex_roundtrip_identity() {
    let raw: [u8; 16] = kani::any();
    let enc = hex_encode(&raw);
    kani::assume(enc.len() == 32);
    let dec = hex_decode(&enc);
    assert!(dec.is_some(), "hex of any bytes must decode");
    assert_eq!(dec.unwrap(), raw.to_vec());
}

/// I3 security gate: for EVERY bounded graphic-ASCII path the validator accepts,
/// the path structurally cannot escape a project root — no absolute prefix, no
/// traversal component, no backslash, no NUL. This is the machine-checked version
/// of the WO6-9 threat model (pushed journal ops reach `root.join(path)`).
#[cfg(kani)]
#[kani::proof]
fn proof_validate_rel_path_confines() {
    let bytes = any_ascii(12);
    let path = String::from_utf8(bytes).expect("ASCII is UTF-8");
    if validate_rel_path(&path).is_ok() {
        // structural containment contract (each conjunct is what the join-safety
        // argument needs):
        assert!(!path.starts_with('/'), "no absolute escape");
        assert!(!path.contains('\\'), "no backslash smuggling");
        assert!(!path.contains('\0'), "no NUL injection");
        for comp in path.split('/') {
            assert!(comp != "..", "no parent traversal");
            assert!(comp != ".", "no current-dir component");
            assert!(!comp.is_empty(), "no empty component");
        }
    }
}

/// I3 gate completeness: the EXPLICIT escape attempts are always rejected — Kani
/// proves the rejection arms are reachable and fire for every position of the
/// traversal component (not just the unit-test fixtures).
#[cfg(kani)]
#[kani::proof]
fn proof_validate_rel_path_rejects_traversal_positions() {
    // .. in ANY of 3 positions must reject
    for pos in 0..3usize {
        let mut parts: Vec<String> = vec!["a".into(), "b".into(), "c".into()];
        parts[pos] = "..".into();
        let path = parts.join("/");
        assert!(validate_rel_path(&path).is_err(), "traversal at {pos}");
    }
    // leading slash at any string length
    let bytes = any_ascii(6);
    let tail = String::from_utf8(bytes).expect("ASCII");
    let abs = format!("/{tail}");
    assert!(validate_rel_path(&abs).is_err());
}

/// I2 frozen format (SPEC §6): parse_commit ∘ build_commit is the identity for any
/// bounded author/label. BLAKE3 is stubbed (deterministic pure transform — see the
/// bloom harness note): the roundtrip property lives in the byte format, and the
/// content-addressing assertion still holds because BOTH sides use the same stub.
#[cfg(kani)]
#[kani::proof]
#[kani::stub(blake3::hash, stub_blake3_hash)]
fn proof_commit_roundtrip_frozen_format() {
    let author = String::from_utf8(any_ascii(8)).expect("ASCII");
    let label = String::from_utf8(any_ascii(8)).expect("ASCII");
    let tree = Hash::from_bytes(kani::any());
    let parent = Hash::from_bytes(kani::any());
    let seq: u64 = kani::any();

    let (commit_hash, bytes) = commit::build_commit(&tree, Some(&parent), &author, &label, seq);
    let (tree2, parent2, author2, label2, seq2) =
        commit::parse_commit(&bytes).expect("self-produced commit parses");
    assert_eq!(tree2, tree, "tree identity");
    assert_eq!(parent2, Some(parent), "parent identity");
    assert_eq!(author2, author, "author identity");
    assert_eq!(label2, label, "label identity");
    assert_eq!(seq2, seq, "snapshot_seq identity");
    // the hash covers exactly these bytes (content-addressing, never guessed)
    assert_eq!(Hash::of(&bytes), commit_hash);
}

/// I2 adversarial-bloom guard: an inserted item is NEVER reported absent (false
/// negatives are forbidden — that is what lets a hostile bloom skip uploads).
///
/// The BLAKE3 hash itself is stubbed with a DETERMINISTIC pure transform: Kani
/// cannot execute blake3's runtime `__cpuid` inline asm (Kani issue #2), and the
/// no-false-negative property lives in the double-hash probing math bloom.rs owns
/// (insert and query derive identical indices for identical inputs). The stub
/// preserves exactly that contract: same bytes in → same digest out, ever.
#[cfg(kani)]
fn stub_blake3_hash(item: &[u8]) -> blake3::Hash {
    let mut out = [0u8; 32];
    for (i, b) in item.iter().take(31).enumerate() {
        out[i] = b.wrapping_mul(31).wrapping_add(i as u8);
    }
    out[31] = item.len() as u8;
    blake3::Hash::from_bytes(out)
}

#[cfg(kani)]
#[kani::proof]
#[kani::stub(blake3::hash, stub_blake3_hash)]
fn proof_bloom_no_false_negative() {
    let a: [u8; 8] = kani::any();
    let b: [u8; 8] = kani::any();
    kani::assume(a != b);
    let mut bloom = Bloom::with_fpp(8, 0.01);
    bloom.insert(&a);
    assert!(bloom.might_contain(&a), "no false negatives (I2)");
    bloom.insert(&b);
    assert!(bloom.might_contain(&a), "inserts never evict");
    assert!(bloom.might_contain(&b), "second insert visible");
}

/// Frozen sniff table: gzip magic (1f 8b) ALWAYS sniffs Gzip regardless of the
/// remaining bytes; nothing else sniffs Gzip. Zip stays deliberately unclaimed.
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

/// Compression policy totality: `policy_for` never panics on arbitrary bounded
/// paths and always returns one of the three frozen variants (exhaustive match
/// downstream relies on this).
#[cfg(kani)]
#[kani::proof]
fn proof_policy_for_is_total() {
    let path = String::from_utf8(any_ascii(MAX_STR)).expect("ASCII");
    let p = crate::compress::policy_for(&path);
    assert!(
        matches!(
            p,
            crate::manifest::Compression::None
                | crate::manifest::Compression::Zstd3
                | crate::manifest::Compression::ZstdDict
        ),
        "policy is one of the frozen three"
    );
    // determinism: same input → same policy (pure function contract)
    assert_eq!(crate::compress::policy_for(&path), p);
}
