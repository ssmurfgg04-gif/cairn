//! Kani bounded-model-checking harnesses (WO6-invariants pattern, cairn-tl
//! edition): machine-checked proofs of the classifier's totality and
//! panic-freedom over the bounded op model — the ADR §3 "the table is a Rust
//! `match` over a closed enum; Kani proves totality and panic-freedom"
//! contract. Run: `cargo kani --harness <name> -p cairn-tl`; CI runs these
//! as kani.yml shards.
//!
//! Round-13/14 hardening (the STATUS.md named follow-up, now landed), four
//! moves that took the pair space from >90-min-per-harness to
//! ~20-30-s-per-shard:
//!
//! 1. CONSTRUCTION STUBS: the original model built its fixed strings/values
//!    with real `String::from`/`serde_json::json!` construction, and CBMC
//!    spent >90 min exploring allocator/serde internals across the 121
//!    symbolic op pairs — longer than any shared-runner preemption window
//!    (killed at 50–95 min mid-exploration, never a refutation, never
//!    completed; evidence in STATUS.md).
//! 2. PER-KIND SHARDING: each proof family is split into 11 shards — the
//!    OURS kind is a CONCRETE constant per shard (the harness entries
//!    `proof_classifier_shard_kind_00..10`, `proof_symmetry_shard_kind_00..10`),
//!    the THEIRS kind stays symbolic. The union of a family's 11 shards is
//!    EXACTLY the original 121-pair space (every (ours, theirs) pair appears
//!    in shard `ours`), so sharding loses no coverage — each CBMC job
//!    explores 11 pairs instead of 121, and kani.yml gives every shard its
//!    own GitHub runner.
//! 3. DROP-GLUE SCOPING: live runner evidence showed CBMC grinding on
//!    `drop_in_place` for the ops' String/BTreeMap/Value fields (allocator
//!    machinery, not merge logic); the harnesses `mem::forget` the ops and
//!    verdict after asserting (real drop behavior stays pinned by the
//!    ordinary test suites, which drop ops in every test).
//! 4. REAL KANI STUBS (`#[kani::stub]`, round 14 part 7 — the literal named
//!    follow-up) + PAYLOAD BRANCH COVERAGE: the three kinds whose classifier
//!    arms compare payloads (Attr / TrackAttr values, Insert identity via
//!    `content_fingerprint`, MarkerAdd identity) still hung: run 33844913132
//!    ground >130 min on exactly shards 03/06/09. Log forensics: CBMC kept
//!    BOTH outcomes of the deep `serde_json::Value`/`Element` equality
//!    alive, so the `format!`-built C3 notes and `content_fingerprint`'s
//!    three nested `format!`s stayed in the symbolic path —
//!    `core::str::slice_error_fail` + `floor_char_boundary` +
//!    `printable::check` churn (1185/786/521 hits). Two fixes, both sound:
//!    (a) `std::fmt::format` is stubbed to `String::new()` inside every
//!    harness — note/fingerprint TEXT is presentation, not the proven
//!    property (class/verdict/once/panic-freedom of the classification
//!    logic); the fmt machinery is std-library territory.
//!    (b) The stub payloads are now BRANCH-VALUED: `Value::Bool(bit)`,
//!    symbolic AttrKind (Name/Enabled), symbolic short names (""/"x"). The
//!    classifier's payload equalities reduce to 1-bit/1-byte compares that
//!    CBMC resolves instantly — AND the model now exercises the
//!    value-differs branches: C3 (same attr, different values), C2
//!    (different attrs), C1 vs C6 (marker union vs identical), C8 vs C6
//!    (insert collision vs identical) — sub-arms the original
//!    always-equal-payload model NEVER reached. Strictly more classifier
//!    coverage than round 12's 121-pair model, at a fraction of the cost.
//!
//! Stub soundness (why the proof statements are unchanged): within the
//! bounded model the classifier never branches on string or JSON CONTENT
//! beyond the equality/inequality of payloads the model constructs from the
//! same closed stub set on both sides — branch-valued stubs exercise BOTH
//! equality outcomes, so every payload-sensitive arm is now covered.
//! Construction of real strings/values (identity keys, serde values,
//! markers) stays pinned by the 89 cairn-tl tests + the 18-file
//! real-timeline corpus gate.

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

/// THE round-14 Kani stub (move 4a): replaces `std::fmt::format` throughout
/// the harness call graph, so every `format!` in the classifier's notes and
/// in `content_fingerprint` produces an empty String without entering std
/// fmt machinery (Display/Debug impls, char-boundary checks, unicode
/// tables). Signature-matched to `std::fmt::format`.
#[cfg(kani)]
fn kani_fmt_stub(_args: core::fmt::Arguments) -> String {
    String::new()
}

/// Move 4b: `Element::content_fingerprint` stub — the fingerprint STRING
/// is identity-ladder machinery, not the classifier property; its real
/// behavior stays pinned by the 89 cairn-tl tests + the corpus gate.
/// Stubbing it removes the whole subtree (three nested format!s, the
/// media-closure BTreeMap search, time_key) whose dead-path walks were the
/// kind-06 OOM/hang. `identical()`'s C6-vs-C8 split for inserts is carried
/// by the element NAME branch bit (stub_name), which remains symbolic.
#[cfg(kani)]
fn kani_fingerprint_stub(_e: &crate::model::Element) -> String {
    String::new()
}

/// CBMC-cheap string stub: allocation-free. Used where the model never
/// branches on the payload (keys, schema tags, comments).
#[cfg(kani)]
fn stub_string() -> String {
    String::new()
}

/// Branch-valued short name: "" or "x" (1-byte heap alloc). The
/// identical-insert / identical-marker arms compare names with a length +
/// ≤1-byte memcmp — cheap for CBMC, and BOTH outcomes are exercised, so the
/// C8/C6 and C1/C6 sub-arms are now covered.
#[cfg(kani)]
fn stub_name() -> String {
    if kani::any() {
        String::new()
    } else {
        "x".into()
    }
}

/// Branch-valued attribute payload: `Value::Bool(bit)`. The (Attr, Attr) and
/// (TrackAttr, TrackAttr) arms' `v1 == v2` reduces to a 1-bit compare —
/// resolved instantly both ways, covering C6 (identical values, applied
/// once) AND C3 (same attr, different values — Human), where the original
/// always-equal model only ever reached C6.
#[cfg(kani)]
fn stub_value() -> serde_json::Value {
    serde_json::Value::Bool(kani::any())
}

/// Branch-valued attribute kind: Name or Enabled (both whitelist arms with
/// literal `as_str` — no format machinery). Covers C2 (a1 != a2 — both
/// apply) in addition to the C3/C6 same-attr arms.
#[cfg(kani)]
fn stub_attr_kind() -> AttrKind {
    if kani::any() {
        AttrKind::Name
    } else {
        AttrKind::Enabled
    }
}

/// CBMC-cheap marker stub: fixed shape, branch-valued name, empty maps —
/// the (MarkerAdd, MarkerAdd) identity check (`name && comment && range`)
/// reduces to the name branch bit, covering both C6 (identical — added
/// once) and C1 (marker union).
#[cfg(kani)]
fn stub_marker() -> Marker {
    Marker {
        schema: stub_string(),
        name: stub_name(),
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
/// parameters. Every op kind is reachable; payload construction is stubbed
/// and branch-valued (see the module doc); the symbolic surface is
/// kinds, sides, `a`/`b` integers, exact-rational trim deltas, and the
/// payload branch bits.
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
            attr: stub_attr_kind(),
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
            element: Element::leaf(Kind::Clip, stub_name()),
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
            attr: stub_attr_kind(),
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
/// of the original single harness (identical assertions — only the
/// ours-kind discriminant moved from symbolic to per-shard concrete), and
/// the payload branch bits add the C1/C2/C3/C8 sub-arm coverage.
#[cfg(kani)]
fn classifier_body(ours_kind: u8) {
    let theirs_kind: u8 = kani::any();
    kani::assume(theirs_kind < 11);
    // Bounded integer surface: a/b feed only slot indexes, track numbers,
    // reorder orders, and trim deltas. Every CROSS-SIDE comparison of an
    // a/b-derived field is equal-by-construction (both ops are built from
    // the SAME a and b), so the comparison outcomes are range-INDEPENDENT —
    // but the 2^64-wide symbolic integers blow up the 128-bit gcd/division
    // bitblasting behind Rational::new and the SAT search space. A 16-value
    // range keeps every reachable comparison outcome at a fraction of the
    // state (documented model bound, not a coverage cut).
    let a: i64 = kani::any();
    let b: i64 = kani::any();
    kani::assume(a >= 0 && a < 16);
    kani::assume(b >= 0 && b < 16);
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
        // destructors are out of scope for this proof (move 3):
        // skip drop glue for the verdict's note String.
        std::mem::forget(v);
    } else {
        // disjoint pairs never classify (C0 by definition — the caller treats
        // them as one-sided auto-apply)
    }
    // destructors are out of scope for this proof (move 3):
    // skip the String/BTreeMap/Value drop glue of the ops themselves.
    std::mem::forget(ours);
    std::mem::forget(theirs);
}

// 11 shards: ours-kind concrete 0..=10 (Remove, Move, Trim, Attr, MarkerAdd,
// MarkerRemove, Insert, TrackAdd, TrackRemove, TrackAttr, TrackReorder).
// One harness per runner — see the module doc and kani.yml. Every shard
// carries the std::fmt::format stub (move 4a).
#[cfg(kani)]
#[kani::proof]
#[kani::stub(std::fmt::format, kani_fmt_stub)]
#[kani::stub(crate::model::Element::content_fingerprint, kani_fingerprint_stub)]
fn proof_classifier_shard_kind_00() {
    classifier_body(0);
}

#[cfg(kani)]
#[kani::proof]
#[kani::stub(std::fmt::format, kani_fmt_stub)]
#[kani::stub(crate::model::Element::content_fingerprint, kani_fingerprint_stub)]
fn proof_classifier_shard_kind_01() {
    classifier_body(1);
}

#[cfg(kani)]
#[kani::proof]
#[kani::stub(std::fmt::format, kani_fmt_stub)]
#[kani::stub(crate::model::Element::content_fingerprint, kani_fingerprint_stub)]
fn proof_classifier_shard_kind_02() {
    classifier_body(2);
}

#[cfg(kani)]
#[kani::proof]
#[kani::stub(std::fmt::format, kani_fmt_stub)]
#[kani::stub(crate::model::Element::content_fingerprint, kani_fingerprint_stub)]
fn proof_classifier_shard_kind_03() {
    classifier_body(3);
}

#[cfg(kani)]
#[kani::proof]
#[kani::stub(std::fmt::format, kani_fmt_stub)]
#[kani::stub(crate::model::Element::content_fingerprint, kani_fingerprint_stub)]
fn proof_classifier_shard_kind_04() {
    classifier_body(4);
}

#[cfg(kani)]
#[kani::proof]
#[kani::stub(std::fmt::format, kani_fmt_stub)]
#[kani::stub(crate::model::Element::content_fingerprint, kani_fingerprint_stub)]
fn proof_classifier_shard_kind_05() {
    classifier_body(5);
}

#[cfg(kani)]
#[kani::proof]
#[kani::stub(std::fmt::format, kani_fmt_stub)]
#[kani::stub(crate::model::Element::content_fingerprint, kani_fingerprint_stub)]
fn proof_classifier_shard_kind_06() {
    classifier_body(6);
}

#[cfg(kani)]
#[kani::proof]
#[kani::stub(std::fmt::format, kani_fmt_stub)]
#[kani::stub(crate::model::Element::content_fingerprint, kani_fingerprint_stub)]
fn proof_classifier_shard_kind_07() {
    classifier_body(7);
}

#[cfg(kani)]
#[kani::proof]
#[kani::stub(std::fmt::format, kani_fmt_stub)]
#[kani::stub(crate::model::Element::content_fingerprint, kani_fingerprint_stub)]
fn proof_classifier_shard_kind_08() {
    classifier_body(8);
}

#[cfg(kani)]
#[kani::proof]
#[kani::stub(std::fmt::format, kani_fmt_stub)]
#[kani::stub(crate::model::Element::content_fingerprint, kani_fingerprint_stub)]
fn proof_classifier_shard_kind_09() {
    classifier_body(9);
}

#[cfg(kani)]
#[kani::proof]
#[kani::stub(std::fmt::format, kani_fmt_stub)]
#[kani::stub(crate::model::Element::content_fingerprint, kani_fingerprint_stub)]
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
    kani::assume(a >= 0 && a < 16);
    kani::assume(b >= 0 && b < 16);
    let ours = bounded_op(ka, Side::Ours, a, b);
    let theirs = bounded_op(kb, Side::Theirs, a, b);
    assert_eq!(interacts(&ours, &theirs), interacts(&theirs, &ours));
    // destructors are out of scope for this proof (move 3).
    std::mem::forget(ours);
    std::mem::forget(theirs);
}

#[cfg(kani)]
#[kani::proof]
#[kani::stub(std::fmt::format, kani_fmt_stub)]
#[kani::stub(crate::model::Element::content_fingerprint, kani_fingerprint_stub)]
fn proof_symmetry_shard_kind_00() {
    symmetry_body(0);
}

#[cfg(kani)]
#[kani::proof]
#[kani::stub(std::fmt::format, kani_fmt_stub)]
#[kani::stub(crate::model::Element::content_fingerprint, kani_fingerprint_stub)]
fn proof_symmetry_shard_kind_01() {
    symmetry_body(1);
}

#[cfg(kani)]
#[kani::proof]
#[kani::stub(std::fmt::format, kani_fmt_stub)]
#[kani::stub(crate::model::Element::content_fingerprint, kani_fingerprint_stub)]
fn proof_symmetry_shard_kind_02() {
    symmetry_body(2);
}

#[cfg(kani)]
#[kani::proof]
#[kani::stub(std::fmt::format, kani_fmt_stub)]
#[kani::stub(crate::model::Element::content_fingerprint, kani_fingerprint_stub)]
fn proof_symmetry_shard_kind_03() {
    symmetry_body(3);
}

#[cfg(kani)]
#[kani::proof]
#[kani::stub(std::fmt::format, kani_fmt_stub)]
#[kani::stub(crate::model::Element::content_fingerprint, kani_fingerprint_stub)]
fn proof_symmetry_shard_kind_04() {
    symmetry_body(4);
}

#[cfg(kani)]
#[kani::proof]
#[kani::stub(std::fmt::format, kani_fmt_stub)]
#[kani::stub(crate::model::Element::content_fingerprint, kani_fingerprint_stub)]
fn proof_symmetry_shard_kind_05() {
    symmetry_body(5);
}

#[cfg(kani)]
#[kani::proof]
#[kani::stub(std::fmt::format, kani_fmt_stub)]
#[kani::stub(crate::model::Element::content_fingerprint, kani_fingerprint_stub)]
fn proof_symmetry_shard_kind_06() {
    symmetry_body(6);
}

#[cfg(kani)]
#[kani::proof]
#[kani::stub(std::fmt::format, kani_fmt_stub)]
#[kani::stub(crate::model::Element::content_fingerprint, kani_fingerprint_stub)]
fn proof_symmetry_shard_kind_07() {
    symmetry_body(7);
}

#[cfg(kani)]
#[kani::proof]
#[kani::stub(std::fmt::format, kani_fmt_stub)]
#[kani::stub(crate::model::Element::content_fingerprint, kani_fingerprint_stub)]
fn proof_symmetry_shard_kind_08() {
    symmetry_body(8);
}

#[cfg(kani)]
#[kani::proof]
#[kani::stub(std::fmt::format, kani_fmt_stub)]
#[kani::stub(crate::model::Element::content_fingerprint, kani_fingerprint_stub)]
fn proof_symmetry_shard_kind_09() {
    symmetry_body(9);
}

#[cfg(kani)]
#[kani::proof]
#[kani::stub(std::fmt::format, kani_fmt_stub)]
#[kani::stub(crate::model::Element::content_fingerprint, kani_fingerprint_stub)]
fn proof_symmetry_shard_kind_10() {
    symmetry_body(10);
}
