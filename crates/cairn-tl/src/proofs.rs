//! Kani bounded-model-checking harnesses (WO6-invariants pattern, cairn-tl
//! edition): machine-checked proofs of the classifier's totality and
//! panic-freedom over the bounded op model — the ADR §3 "the table is a Rust
//! `match` over a closed enum; Kani proves totality and panic-freedom"
//! contract. Run: `cargo kani --harness <name> -p cairn-tl`; CI runs these
//! as kani.yml shards.
//!
//! Tractability note (inherited from cairn-core's proof round): symbolic
//! Strings through Rust's allocator model do not converge — the bounded op
//! model here uses FIXED identity keys and exact-rational deltas with
//! symbolic discriminants, which CBMC handles without allocation blowup.

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

/// Build a bounded op from a symbolic discriminant (0..=9) + symbolic
/// parameters. Every op kind is reachable; element content is FIXED (no
/// symbolic strings).
#[cfg(kani)]
fn bounded_op(kind: u8, side: Side, a: i64, b: i64) -> Op {
    let key = ElementKey("uuid:kani".into());
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
            value: serde_json::json!("x"),
        },
        4 => Op::MarkerAdd {
            side,
            key,
            base,
            marker: Marker {
                schema: "Marker.2".into(),
                name: "m".into(),
                color: "RED".into(),
                comment: String::new(),
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
            },
        },
        5 => Op::MarkerRemove {
            side,
            key,
            base,
            marker_key: "m".into(),
        },
        6 => Op::Insert {
            side,
            element: Element::leaf(Kind::Clip, "kani"),
            to: TrackLoc::Base(0),
            slot: Slot::Before {
                track: 0,
                index: a.unsigned_abs() as usize,
            },
        },
        7 => Op::TrackAdd {
            side,
            ordinal: a.unsigned_abs() as usize,
            track: Element::leaf(Kind::Track(TrackKind::Video), "T"),
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
            value: serde_json::json!("x"),
        },
        _ => Op::TrackReorder {
            side,
            order: vec![a.unsigned_abs() as usize, b.unsigned_abs() as usize],
        },
    }
}

/// Totality + panic-freedom: EVERY (ours-kind × theirs-kind) pair over the
/// bounded op model classifies to a verdict with a legal class code, and
/// nothing panics. 11×11 = 121 symbolic pairs, allocation-free.
#[cfg(kani)]
#[kani::proof]
fn proof_classifier_total_and_panic_free() {
    let ours_kind: u8 = kani::any();
    let theirs_kind: u8 = kani::any();
    kani::assume(ours_kind < 11);
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

/// Interaction symmetry: interacts(a, b) == interacts(b, a) over the bounded
/// model — an asymmetric interaction test would silently drop conflicts.
#[cfg(kani)]
#[kani::proof]
fn proof_interaction_symmetry() {
    let ka: u8 = kani::any();
    let kb: u8 = kani::any();
    kani::assume(ka < 11);
    kani::assume(kb < 11);
    let a: i64 = kani::any();
    let b: i64 = kani::any();
    let ours = bounded_op(ka, Side::Ours, a, b);
    let theirs = bounded_op(kb, Side::Theirs, a, b);
    assert_eq!(interacts(&ours, &theirs), interacts(&theirs, &ours));
}
