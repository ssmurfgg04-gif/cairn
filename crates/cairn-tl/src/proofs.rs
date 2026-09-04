//! Kani bounded-model-checking harnesses (WO6-invariants pattern, cairn-tl
//! edition): machine-checked proofs of the classifier's totality and
//! panic-freedom over the bounded op model — the ADR §3 "the table is a Rust
//! `match` over a closed enum; Kani proves totality and panic-freedom"
//! contract. Run: `cargo kani --harness <name> -p cairn-tl`; CI runs these
//! as kani.yml shards.
//!
//! Round-13/14 hardening (the STATUS.md named follow-up, now landed), two
//! moves that took the pair space from >90-min-per-harness to runner-friendly:
//!
//! 1. STUBS: the original model built its fixed strings/values with real
//!    `String::from`/`serde_json::json!` construction, and CBMC spent >90 min
//!    exploring allocator/serde internals across the 121 symbolic op pairs —
//!    longer than any shared-runner preemption window (killed at 50–95 min
//!    mid-exploration, never a refutation, never completed; evidence in
//!    STATUS.md). Construction in `bounded_op` is now STUBBED
//!    (`stub_string`/`stub_value` below): allocation-free, identical op
//!    SHAPES, same symbolic discriminant surface.
//! 2. SHARDING: each proof family is split into 11 per-kind shards — the
//!    OURS kind is a CONCRETE constant per shard (the 11 harness entries
//!    `proof_classifier_shard_kind_0..10`, `proof_symmetry_shard_kind_0..10`),
//!    the THEIRS kind stays symbolic. The union of a family's 11 shards is
//!    EXACTLY the original 121-pair space (every (ours, theirs) pair appears
//!    in shard `ours`), so sharding loses no coverage — each CBMC job
//!    explores 11 pairs instead of 121, and kani.yml gives every shard its
//!    own GitHub runner.
//!
//! Stub soundness (why the proof statements are unchanged): within the
//! bounded model the classifier never branches on string or JSON CONTENT —
//! only on op shape (kinds, sides, base coords, tracks, indexes, anchors,
//! deltas) and on EQUALITY between fields the model constructs identically
//! on both sides (key == key, marker name == name, attr value == value).
//! Empty-on-both-sides preserves every equality outcome the fixed-content
//! model had ("uuid:kani" == "uuid:kani" and "" == "" are both `true`), so
//! totality, panic-freedom, class-range, verdict-closure, and interaction
//! symmetry all keep their original meaning. The real construction paths
//! (identity keys, serde values, markers) stay pinned by the 89 cairn-tl
//! tests + the 18-file real-timeline corpus gate.

// Compiled only by `cargo kani` (the cairn-core cfg(kani) pattern) — normal
// builds see none of these imports.
#[cfg(kani)]
use crate::classifier::{classify_pair, interacts};
#[cfg(kani)]
use crate::identity::ElementKey;
#[cfg(kani)]
use crate::model::{Element, JsonMap, Kind, Marker, TimeRange, TimeVal, TrackKind};
#[cfg(kani)]
use crate::ops::{AttrKind, Op, Side, Slot, TrackLoc};
#[cfg(kani)]
use crate::rational::Rational;

/// CBMC-cheap string stub: allocation-free (`String::new` never calls the
/// allocator, no memcpy, no RawVec internals). See the module doc for why
/// content may be empty without weakening the proof statements.
#[cfg(kani)]
fn stub_string() -> String {
    String::new()
}

/// CBMC-cheap attribute-value stub: keeps the `Value::String` variant the
/// real model uses (equality outcomes preserved — both sides construct it
/// identically) without the `serde_json::json!` construction internals.
#[cfg(kani)]
fn stub_value() -> serde_json::Value {
    serde_json::Value::String(stub_string())
}

/// CBMC-cheap marker stub: fixed shape, stub strings, empty maps — both
/// sides build the identical marker, so the MarkerAdd-vs-MarkerAdd
/// equality check keeps its original outcome.
#[cfg(kani)]
fn stub_marker() -> Marker {
    Marker {
        schema: stub_string(),
        name: stub_string(),
        color: stub_string(),
        comment: stub_string(),
        marked_range: TimeRange {
            start: TimeVal {
                value: Rational::ZERO,
                rate: Rational::new(1, 1).unwrap(),
            },
            duration: TimeVal {
                value: Rational::ZERO,
                rate: Rational::new(1, 1).unwrap(),
            },
        },
        metadata: JsonMap::new(),
        extra: JsonMap::new(),
    }
}

/// Build a bounded op from a symbolic discriminant (0..=10) + symbolic
/// parameters. Every op kind is reachable; element content is STUBBED
/// (empty strings/values — see the module doc); the symbolic surface is
/// otherwise unchanged from the round-12 model: kinds, sides, `a`/`b`
/// integers, and the exact-rational trim deltas.
#[cfg(kani)]
fn bounded_op(kind: u8, side: Side, a: i64, b: i64) -> Op {
    let key = ElementKey(stub_string());
    let base = (0usize, 0usize);
    let r = |n: i128| Rational::new(n, 24).unwrap_or(Rational::ZERO);
    match kind {
        0 => Op::Remove { side, key, base },
        1 => Op::Move {
            side,
            key,
            from: base,
            to: TrackLoc::Base(0),
            slot: Slot::Before {
                track: 0,
                index: a.unsigned_abs() as usize,
            },
        },
        2 => Op::Trim {
            side,
            key,
            base,
            in_delta: r(i128::from(a)),
            out_delta: r(i128::from(b)),
        },
        3 => Op::Attr {
            side,
            key,
            base,
            attr: AttrKind::Name,
            value: stub_value(),
        },
        4 => Op::MarkerAdd {
            side,
            key,
            base,
            marker: stub_marker(),
        },
        5 => Op::MarkerRemove {
            side,
            key,
            base,
            marker_key: stub_string(),
        },
        6 => Op::Insert {
            side,
            element: Element::leaf(Kind::Clip, stub_string()),
            to: TrackLoc::Base(0),
            slot: Slot::Before {
                track: 0,
                index: a.unsigned_abs() as usize,
            },
        },
        7 => Op::TrackAdd {
            side,
            ordinal: a.unsigned_abs() as usize,
            track: Element::leaf(Kind::Track(TrackKind::Video), stub_string()),
            slot: Slot::EndOf { track: 0 },
        },
        8 => Op::TrackRemove {
            side,
            track: a.unsigned_abs() as usize,
        },
        9 => Op::TrackAttr {
            side,
            track: a.unsigned_abs() as usize,
            attr: AttrKind::Name,
            value: stub_value(),
        },
        _ => Op::TrackReorder {
            side,
            order: vec![a.unsigned_abs() as usize, b.unsigned_abs() as usize],
        },
    }
}

/// Totality + panic-freedom, SHARDED: for a concrete OURS kind, EVERY
/// theirs-kind (0..=10) classifies to a verdict with a legal class code,
/// and nothing panics. The 11 `proof_classifier_shard_kind_*` harnesses
/// below pass 0..=10 as `ours_kind`; their union is the full 121-pair space
/// of the original single harness (identical assertions, identical symbolic
/// surface — only the ours-kind discriminant moved from symbolic to
/// per-shard concrete).
#[cfg(kani)]
fn classifier_body(ours_kind: u8) {
    let theirs_kind: u8 = kani::any();
    kani::assume(theirs_kind < 11);
    let a: i64 = kani::any();
    let b: i64 = kani::any();
    let ours = bounded_op(ours_kind, Side::Ours, a, b);
    let theirs = bounded_op(theirs_kind, Side::Theirs, a, b);

    // interacting pairs classify (no panic, total match)
    if interacts(&ours, &theirs) {
        let v = classify_pair(&ours, &theirs);
        // class code is in the C0–C10 wire range
        assert!(v.class <= 10, "class must be a legal C0–C10 code");
        // verdict is one of the four closed variants (exhaustive by type;
        // assert the discriminant so CBMC sees the closed set)
        let discr = match v.verdict {
            crate::classifier::Verdict::Auto => 0,
            crate::classifier::Verdict::AutoNote => 1,
            crate::classifier::Verdict::Human => 2,
            crate::classifier::Verdict::Refuse => 3,
        };
        assert!(discr <= 3);
    } else {
        // disjoint pairs never classify (C0 by definition — the caller treats
        // them as one-sided auto-apply)
    }
}

// 11 shards: ours-kind concrete 0..=10 (Remove, Move, Trim, Attr, MarkerAdd,
// MarkerRemove, Insert, TrackAdd, TrackRemove, TrackAttr, TrackReorder).
// One harness per runner — see the module doc and kani.yml.
#[cfg(kani)]
#[kani::proof]
fn proof_classifier_shard_kind_0() {
    classifier_body(0);
}

#[cfg(kani)]
#[kani::proof]
fn proof_classifier_shard_kind_1() {
    classifier_body(1);
}

#[cfg(kani)]
#[kani::proof]
fn proof_classifier_shard_kind_2() {
    classifier_body(2);
}

#[cfg(kani)]
#[kani::proof]
fn proof_classifier_shard_kind_3() {
    classifier_body(3);
}

#[cfg(kani)]
#[kani::proof]
fn proof_classifier_shard_kind_4() {
    classifier_body(4);
}

#[cfg(kani)]
#[kani::proof]
fn proof_classifier_shard_kind_5() {
    classifier_body(5);
}

#[cfg(kani)]
#[kani::proof]
fn proof_classifier_shard_kind_6() {
    classifier_body(6);
}

#[cfg(kani)]
#[kani::proof]
fn proof_classifier_shard_kind_7() {
    classifier_body(7);
}

#[cfg(kani)]
#[kani::proof]
fn proof_classifier_shard_kind_8() {
    classifier_body(8);
}

#[cfg(kani)]
#[kani::proof]
fn proof_classifier_shard_kind_9() {
    classifier_body(9);
}

#[cfg(kani)]
#[kani::proof]
fn proof_classifier_shard_kind_10() {
    classifier_body(10);
}

/// Interaction symmetry, SHARDED: for a concrete OURS kind,
/// interacts(a, b) == interacts(b, a) for every theirs-kind — an asymmetric
/// interaction test would silently drop conflicts. Union of the 11 shards
/// = the full 121-pair space of the original single harness.
#[cfg(kani)]
fn symmetry_body(ka: u8) {
    let kb: u8 = kani::any();
    kani::assume(kb < 11);
    let a: i64 = kani::any();
    let b: i64 = kani::any();
    let ours = bounded_op(ka, Side::Ours, a, b);
    let theirs = bounded_op(kb, Side::Theirs, a, b);
    assert_eq!(interacts(&ours, &theirs), interacts(&theirs, &ours));
}

#[cfg(kani)]
#[kani::proof]
fn proof_symmetry_shard_kind_0() {
    symmetry_body(0);
}

#[cfg(kani)]
#[kani::proof]
fn proof_symmetry_shard_kind_1() {
    symmetry_body(1);
}

#[cfg(kani)]
#[kani::proof]
fn proof_symmetry_shard_kind_2() {
    symmetry_body(2);
}

#[cfg(kani)]
#[kani::proof]
fn proof_symmetry_shard_kind_3() {
    symmetry_body(3);
}

#[cfg(kani)]
#[kani::proof]
fn proof_symmetry_shard_kind_4() {
    symmetry_body(4);
}

#[cfg(kani)]
#[kani::proof]
fn proof_symmetry_shard_kind_5() {
    symmetry_body(5);
}

#[cfg(kani)]
#[kani::proof]
fn proof_symmetry_shard_kind_6() {
    symmetry_body(6);
}

#[cfg(kani)]
#[kani::proof]
fn proof_symmetry_shard_kind_7() {
    symmetry_body(7);
}

#[cfg(kani)]
#[kani::proof]
fn proof_symmetry_shard_kind_8() {
    symmetry_body(8);
}

#[cfg(kani)]
#[kani::proof]
fn proof_symmetry_shard_kind_9() {
    symmetry_body(9);
}

#[cfg(kani)]
#[kani::proof]
fn proof_symmetry_shard_kind_10() {
    symmetry_body(10);
}
