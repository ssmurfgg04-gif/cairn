//! The total conflict classifier (ADR-0015 §3): every interacting op pair maps
//! to exactly one verdict — auto-apply, auto-with-note, or human escalation.
//! No silent loss (I2). The table is a closed `match`; the compiler enforces
//! totality, and the Kani harness (proofs.rs) proves panic-freedom over the
//! bounded op model.
//!
//! Interaction = the two ops share an element key, an insertion anchor, or a
//! containing track. Disjoint op pairs are C0 (auto) by definition and never
//! reach this table.

use crate::ops::{Op, Side};

/// Conflict classes C0–C10 (ADR §3). The numbering is wire-stable (reports,
/// telemetry histograms, docs).
pub mod class {
    pub const C0: u8 = 0; // one-sided: auto-apply
    pub const C1: u8 = 1; // MARKER_ADD both: auto (union)
    pub const C2: u8 = 2; // ATTR different keys: auto (both)
    pub const C3: u8 = 3; // same creative parameter, different values: HUMAN
    pub const C4: u8 = 4; // MOVE vs MOVE different targets: HUMAN
    pub const C5: u8 = 5; // MOVE vs TRIM: auto (move, then trim)
    pub const C6: u8 = 6; // REMOVE both: auto (remove once)
    pub const C7: u8 = 7; // REMOVE vs edit: HUMAN (deletion-wins is unsafe)
    pub const C8: u8 = 8; // INSERT both, same slot: auto-with-note
    pub const C9: u8 = 9; // TRACK_REMOVE vs ops inside: HUMAN
    pub const C10: u8 = 10; // structural mismatch: REFUSE
                            // Round 20 (ADR-0023), semantic policy ONLY: frame-disjoint re-cuts of
                            // the same element auto-merge with a note. Distinct from C3 (which stays
                            // HUMAN in the default conservative policy) so telemetry can tell the two
                            // apart without ambiguity.
    pub const C11: u8 = 11; // semantic: different edges of the same clip: auto-with-note
}

/// Merge policy (ADR-0023): which interacting pairs escalate to a human.
/// The policy NEVER defaults to semantic — every editor opts in themselves
/// (`--semantic` on tl-merge, or the per-device `semantic_merge` flag on the
/// daemon surface). Conservative is bit-for-bit the Round-19 behavior.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum Policy {
    /// Default. Any same-creative-parameter pair (C3) is withheld for a human.
    #[default]
    Conservative,
    /// Zero-touch semantic: frame-disjoint edits to the same element
    /// (a head-only re-cut vs a tail-only re-cut) auto-merge as C11.
    /// Same-edge edits are still C3/HUMAN — "one cut at 00:01:23, the other at
    /// 00:01:24" still interrupts the editor.
    Semantic,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Verdict {
    /// Apply (both sides where relevant), no note.
    Auto,
    /// Apply, record a note in the report.
    AutoNote,
    /// Withhold BOTH ops; a human decides. Base state is kept.
    Human,
    /// Refuse the whole merge (C10).
    Refuse,
}

#[derive(Clone, Debug)]
pub struct PairVerdict {
    pub class: u8,
    pub verdict: Verdict,
    pub note: String,
    /// Identical/duplicate pair: apply OURS only (the theirs op is deduped).
    /// Essential where double-apply is destructive (trims double-delta,
    /// inserts duplicate, markers double-add).
    pub once: bool,
}

/// Do these two ops interact (share element key, anchor, or track)?
pub fn interacts(ours: &Op, theirs: &Op) -> bool {
    if let (Some(ok), Some(tk)) = (ours.element_key(), theirs.element_key()) {
        if ok == tk {
            return true;
        }
    }
    if let (Some(oa), Some(ta)) = (ours.anchor(), theirs.anchor()) {
        if anchors_collide(&oa, &ta) {
            return true;
        }
    }
    // track containment: TrackRemove vs item ops inside that track (and
    // TrackAttr edits ON the track itself)
    if let (Op::TrackRemove { track: bt, .. }, op) = (ours, theirs) {
        if op_in_base_track(op, *bt) {
            return true;
        }
    }
    if let (op, Op::TrackRemove { track: bt, .. }) = (ours, theirs) {
        if op_in_base_track(op, *bt) {
            return true;
        }
    }
    // matched-track attr edits interact when they target the SAME base track
    if let (Op::TrackAttr { track: t1, .. }, Op::TrackAttr { track: t2, .. }) = (ours, theirs) {
        return t1 == t2;
    }
    // reorder vs reorder
    if let (Op::TrackReorder { .. }, Op::TrackReorder { .. }) = (ours, theirs) {
        return true;
    }
    // same new-track creation (duplicate track add)
    if let (Op::TrackAdd { .. }, Op::TrackAdd { .. }) = (ours, theirs) {
        return true;
    }
    false
}

fn anchors_collide(
    a: &(crate::ops::TrackLoc, crate::ops::Slot),
    b: &(crate::ops::TrackLoc, crate::ops::Slot),
) -> bool {
    // same track + "adjacent" slot semantics: same Before target, or both
    // target the same end-of-track.
    let same_track = match (a.0, b.0) {
        (crate::ops::TrackLoc::Base(x), crate::ops::TrackLoc::Base(y)) => x == y,
        (
            crate::ops::TrackLoc::New {
                side: s1,
                ordinal: o1,
            },
            crate::ops::TrackLoc::New {
                side: _s2,
                ordinal: o2,
            },
        ) => {
            // same side never pairs ours/theirs; cross-side new tracks collide
            // when the sides created the "same" ordinal slot — rare; treat
            // equal-ordinal as interacting
            let _ = s1;
            o1 == o2
        }
        _ => false,
    };
    if !same_track {
        return false;
    }
    match (a.1, b.1) {
        (
            crate::ops::Slot::Before {
                track: t1,
                index: i1,
            },
            crate::ops::Slot::Before {
                track: t2,
                index: i2,
            },
        ) => t1 == t2 && i1 == i2,
        (crate::ops::Slot::EndOf { track: t1 }, crate::ops::Slot::EndOf { track: t2 }) => t1 == t2,
        (
            crate::ops::Slot::NewTrackOrdinal { ordinal: o1 },
            crate::ops::Slot::NewTrackOrdinal { ordinal: o2 },
        ) => o1 == o2,
        _ => false,
    }
}

fn op_in_base_track(op: &Op, base_track: usize) -> bool {
    match op {
        Op::Insert { to, slot, .. } => {
            if let crate::ops::TrackLoc::Base(t) = to {
                if *t == base_track {
                    return true;
                }
            }
            matches!(
                slot,
                crate::ops::Slot::Before { track, .. } | crate::ops::Slot::EndOf { track } if *track == base_track
            )
        }
        Op::Move { from, to, slot, .. } => {
            if *from == (base_track, from.1) || from.0 == base_track {
                return true;
            }
            if let crate::ops::TrackLoc::Base(t) = to {
                if *t == base_track {
                    return true;
                }
            }
            matches!(
                slot,
                crate::ops::Slot::Before { track, .. } | crate::ops::Slot::EndOf { track } if *track == base_track
            )
        }
        Op::Remove { base, .. }
        | Op::Trim { base, .. }
        | Op::Attr { base, .. }
        | Op::MarkerAdd { base, .. }
        | Op::MarkerRemove { base, .. } => base.0 == base_track,
        Op::TrackAttr { track, .. } => *track == base_track,
        _ => false,
    }
}

/// Classify an INTERACTING pair under a [`Policy`]. Total by construction:
/// every arm is explicit and the final arm is a guarded C0 for genuinely
/// disjoint shapes (the interaction precondition makes reaching it a bug —
/// the tests assert the reachable arm set exactly).
pub fn classify_pair(ours: &Op, theirs: &Op, policy: Policy) -> PairVerdict {
    use Verdict::{Auto, AutoNote, Human};
    let (o, t) = (kind_of(ours), kind_of(theirs));
    let pair = (o, t);
    match pair {
        // ---- REMOVE families ----
        (K::Remove, K::Remove) => PairVerdict {
            class: class::C6,
            verdict: Auto,
            note: "both sides removed — removed once".into(),
            once: false,
        },
        (K::Remove, K::Trim) | (K::Trim, K::Remove)
        | (K::Remove, K::Attr) | (K::Attr, K::Remove)
        | (K::Remove, K::Move) | (K::Move, K::Remove)
        | (K::Remove, K::MarkerAdd) | (K::MarkerAdd, K::Remove)
        | (K::Remove, K::MarkerRemove) | (K::MarkerRemove, K::Remove) => PairVerdict {
            class: class::C7,
            verdict: Human,
            note: "one side deleted what the other edited — deletion-wins is not safe for creative work".into(),
            once: false,
        },
        // ---- MOVE families ----
        (K::Move, K::Move) => {
            if same_target(ours, theirs) {
                PairVerdict { class: class::C4, verdict: Auto, note: "both moved to the same place — moved once".into(), once: true }
            } else {
                PairVerdict {
                    class: class::C4,
                    verdict: Human,
                    note: "both sides moved the same element to different targets".into(),
            once: false,
                }
            }
        }
        (K::Move, K::Trim) | (K::Trim, K::Move) => PairVerdict {
            class: class::C5,
            verdict: Auto,
            note: "move then trim — commutes exactly".into(),
            once: false,
        },
        (K::Move, K::Attr) | (K::Attr, K::Move)
        | (K::Move, K::MarkerAdd) | (K::MarkerAdd, K::Move)
        | (K::Move, K::MarkerRemove) | (K::MarkerRemove, K::Move) => PairVerdict {
            class: class::C2,
            verdict: Auto,
            note: "move + attribute change — attribute applies by identity".into(),
            once: false,
        },
        // ---- TRIM vs TRIM ----
        (K::Trim, K::Trim) => {
            if identical(ours, theirs) {
                PairVerdict { class: class::C6, verdict: Auto, note: "identical trims — applied once".into(), once: true }
            } else if policy == Policy::Semantic && frame_disjoint_edges(ours, theirs) {
                // ADR-0023: ours re-cut the head, theirs re-cut the tail (or
                // vice versa) — different frames of the same clip, both edits
                // are mechanically composable. AutoNote so the report still
                // shows exactly what the machine decided on the editor's behalf.
                PairVerdict {
                    class: class::C11,
                    verdict: AutoNote,
                    note: "frame-disjoint re-cuts (different edges of the same clip) — both apply, semantic policy".into(),
                    once: false,
                }
            } else {
                PairVerdict {
                    class: class::C3,
                    verdict: Human,
                    note: "both sides re-cut the same element — same creative parameter, different values".into(),
            once: false,
                }
            }
        }
        (K::Trim, K::Attr) | (K::Attr, K::Trim)
        | (K::Trim, K::MarkerAdd) | (K::MarkerAdd, K::Trim)
        | (K::Trim, K::MarkerRemove) | (K::MarkerRemove, K::Trim) => PairVerdict {
            class: class::C2,
            verdict: Auto,
            note: "trim + attribute/marker — independent aspects".into(),
            once: false,
        },
        // ---- ATTR vs ATTR ----
        (K::Attr, K::Attr) => match (ours, theirs) {
            (
                Op::Attr { attr: a1, value: v1, .. },
                Op::Attr { attr: a2, value: v2, .. },
            ) => {
                if a1 != a2 {
                    PairVerdict {
                        class: class::C2,
                        verdict: Auto,
                        note: "different attributes — both apply".into(),
            once: false,
                    }
                } else if v1 == v2 {
                    PairVerdict {
                        class: class::C6,
                        verdict: Auto,
                        note: "identical attribute change — applied once".into(),
                        once: true,
                    }
                } else {
                    PairVerdict {
                        class: class::C3,
                        verdict: Human,
                        note: format!("same attribute ({}), different values — no last-write-wins on creative parameters", a1.as_str()),
            once: false,
                    }
                }
            }
            _ => unreachable!("kinds are Attr"),
        },
        // ---- MARKER families ----
        (K::MarkerAdd, K::MarkerAdd) => {
            let (m1, m2) = match (ours, theirs) {
                (Op::MarkerAdd { marker: m1, .. }, Op::MarkerAdd { marker: m2, .. }) => (m1, m2),
                _ => unreachable!("kinds are MarkerAdd"),
            };
            let same = m1.name == m2.name && m1.comment == m2.comment && m1.marked_range == m2.marked_range;
            if same {
                PairVerdict {
                    class: class::C6,
                    verdict: Auto,
                    note: "identical markers — added once".into(),
                    once: true,
                }
            } else {
                PairVerdict {
                    class: class::C1,
                    verdict: Auto,
                    note: "marker union — ours order then theirs".into(),
                    once: false,
                }
            }
        },
        (K::MarkerRemove, K::MarkerRemove) => PairVerdict {
            class: class::C6,
            verdict: Auto,
            note: "marker removed on both sides — removed once".into(),
            once: false,
        },
        (K::MarkerAdd, K::MarkerRemove) | (K::MarkerRemove, K::MarkerAdd) => PairVerdict {
            class: class::C3,
            verdict: Human,
            note: "one side adds the marker the other removed".into(),
            once: false,
        },
        (K::Attr, K::MarkerAdd) | (K::MarkerAdd, K::Attr)
        | (K::Attr, K::MarkerRemove) | (K::MarkerRemove, K::Attr) => PairVerdict {
            class: class::C2,
            verdict: Auto,
            note: "attribute + marker — independent".into(),
            once: false,
        },
        // ---- INSERT vs INSERT (same slot — the interaction precondition) ----
        (K::Insert, K::Insert) => {
            if identical(ours, theirs) {
                PairVerdict { class: class::C6, verdict: Auto, note: "identical inserts — inserted once".into(), once: true }
            } else {
                PairVerdict {
                    class: class::C8,
                    verdict: AutoNote,
                    note: "both sides inserted at the same slot — ours first, theirs immediately after".into(),
            once: false,
                }
            }
        }
        (K::Insert, _) | (_, K::Insert) => PairVerdict {
            class: class::C0,
            verdict: Auto,
            note: "insert alongside another edit — disjoint by anchor".into(),
            once: false,
        },
        // ---- TRACK families ----
        (K::TrackRemove, K::TrackRemove) => PairVerdict {
            class: class::C6,
            verdict: Auto,
            note: "track removed on both sides — removed once".into(),
            once: false,
        },
        (K::TrackRemove, _) | (_, K::TrackRemove) => PairVerdict {
            class: class::C9,
            verdict: Human,
            note: "track removal vs edits inside it — human decides".into(),
            once: false,
        },
        // ---- TRACK ATTR (matched-track field edits; interaction = same track) ----
        (K::TrackAttr, K::TrackAttr) => match (ours, theirs) {
            (
                Op::TrackAttr { attr: a1, value: v1, .. },
                Op::TrackAttr { attr: a2, value: v2, .. },
            ) => {
                if a1 != a2 {
                    PairVerdict {
                        class: class::C2,
                        verdict: Auto,
                        note: "different track attributes — both apply".into(),
                        once: false,
                    }
                } else if v1 == v2 {
                    PairVerdict {
                        class: class::C6,
                        verdict: Auto,
                        note: "identical track attribute change — applied once".into(),
                        once: true,
                    }
                } else {
                    PairVerdict {
                        class: class::C3,
                        verdict: Human,
                        note: format!(
                            "same track attribute ({}), different values — no last-write-wins on creative parameters",
                            a1.as_str()
                        ),
                        once: false,
                    }
                }
            }
            _ => unreachable!("kinds are TrackAttr"),
        },
        // track attrs are independent of item-level edits inside the track
        (K::TrackAttr, _) | (_, K::TrackAttr) => PairVerdict {
            class: class::C0,
            verdict: Auto,
            note: "track attribute alongside item edits — independent aspects".into(),
            once: false,
        },
        (K::TrackAdd, K::TrackAdd) => PairVerdict {
            class: class::C0,
            verdict: Auto,
            note: "both sides added tracks — both land (ours first)".into(),
            once: false,
        },
        (K::TrackReorder, K::TrackReorder) => {
            if identical(ours, theirs) {
                PairVerdict { class: class::C6, verdict: Auto, note: "identical reorder — applied once".into(), once: true }
            } else {
                PairVerdict {
                    class: class::C4,
                    verdict: Human,
                    note: "both sides reordered tracks differently".into(),
            once: false,
                }
            }
        }
        (K::TrackReorder, _) | (_, K::TrackReorder) => PairVerdict {
            class: class::C0,
            verdict: Auto,
            note: "reorder alongside non-track edit".into(),
            once: false,
        },
        (K::TrackAdd, _) | (_, K::TrackAdd) => PairVerdict {
            class: class::C0,
            verdict: Auto,
            note: "new track alongside other edits".into(),
            once: false,
        },
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum K {
    Insert,
    Remove,
    Move,
    Trim,
    Attr,
    MarkerAdd,
    MarkerRemove,
    TrackAdd,
    TrackRemove,
    TrackAttr,
    TrackReorder,
}

fn kind_of(op: &Op) -> K {
    match op {
        Op::Insert { .. } => K::Insert,
        Op::Remove { .. } => K::Remove,
        Op::Move { .. } => K::Move,
        Op::Trim { .. } => K::Trim,
        Op::Attr { .. } => K::Attr,
        Op::MarkerAdd { .. } => K::MarkerAdd,
        Op::MarkerRemove { .. } => K::MarkerRemove,
        Op::TrackAdd { .. } => K::TrackAdd,
        Op::TrackRemove { .. } => K::TrackRemove,
        Op::TrackAttr { .. } => K::TrackAttr,
        Op::TrackReorder { .. } => K::TrackReorder,
    }
}

/// Frame-disjoint trims: one side touched ONLY the in-edge (head), the other
/// ONLY the out-edge (tail). The deltas are declarative and base-free, so
/// this is exact — no silent-loss hole. Both edges changed by one side, or
/// the SAME edge changed by both, is NOT disjoint (that is C3).
fn frame_disjoint_edges(ours: &Op, theirs: &Op) -> bool {
    match (ours, theirs) {
        (
            Op::Trim {
                in_delta: i1,
                out_delta: o1,
                ..
            },
            Op::Trim {
                in_delta: i2,
                out_delta: o2,
                ..
            },
        ) => {
            let ours_head_only = !i1.is_zero() && o1.is_zero();
            let theirs_tail_only = i2.is_zero() && !o2.is_zero();
            let ours_tail_only = i1.is_zero() && !o1.is_zero();
            let theirs_head_only = !i2.is_zero() && o2.is_zero();
            (ours_head_only && theirs_tail_only) || (ours_tail_only && theirs_head_only)
        }
        _ => false,
    }
}

fn same_target(ours: &Op, theirs: &Op) -> bool {
    match (ours, theirs) {
        (
            Op::Move {
                to: t1, slot: s1, ..
            },
            Op::Move {
                to: t2, slot: s2, ..
            },
        ) => t1 == t2 && s1 == s2,
        _ => false,
    }
}

fn identical(ours: &Op, theirs: &Op) -> bool {
    match (ours, theirs) {
        (
            Op::Trim {
                in_delta: i1,
                out_delta: o1,
                ..
            },
            Op::Trim {
                in_delta: i2,
                out_delta: o2,
                ..
            },
        ) => i1 == i2 && o1 == o2,
        (Op::Insert { element: e1, .. }, Op::Insert { element: e2, .. }) => {
            // content-equal (uuid stamps differ between sides) — compare
            // identity-relevant content, not raw equality
            e1.content_fingerprint() == e2.content_fingerprint() && e1.name == e2.name
        }
        (Op::TrackReorder { order: o1, .. }, Op::TrackReorder { order: o2, .. }) => o1 == o2,
        _ => false,
    }
}

/// Do the two ops come from opposite sides (the only legal pairing input)?
pub fn cross_side(ours: &Op, theirs: &Op) -> bool {
    ours.side() != theirs.side()
}

/// The side an op list belongs to (sanity for report rendering).
pub fn side_name(side: Side) -> &'static str {
    match side {
        Side::Ours => "ours",
        Side::Theirs => "theirs",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::ElementKey;
    use crate::model::*;
    use crate::ops::AttrKind;
    use crate::ops::{Slot, TrackLoc};
    use crate::rational::Rational;

    fn k() -> ElementKey {
        ElementKey("uuid:k".into())
    }
    fn loc() -> (usize, usize) {
        (0, 0)
    }

    fn trim(side: Side, i: i128, o: i128) -> Op {
        Op::Trim {
            side,
            key: k(),
            base: loc(),
            in_delta: Rational::new(i, 24).unwrap(),
            out_delta: Rational::new(o, 24).unwrap(),
        }
    }
    fn attr(side: Side, kind: AttrKind, v: &str) -> Op {
        Op::Attr {
            side,
            key: k(),
            base: loc(),
            attr: kind,
            value: serde_json::json!(v),
        }
    }
    fn remove(side: Side) -> Op {
        Op::Remove {
            side,
            key: k(),
            base: loc(),
        }
    }
    fn insert(side: Side, name: &str) -> Op {
        Op::Insert {
            side,
            element: Element::leaf(Kind::Clip, name),
            to: TrackLoc::Base(0),
            slot: Slot::Before { track: 0, index: 1 },
        }
    }

    #[test]
    fn table_total_and_correct() {
        // every existing call pins the CONSERVATIVE policy: Round-19
        // behavior must not drift by one bit when the policy lands.
        // C6: remove both
        let v = classify_pair(
            &remove(Side::Ours),
            &remove(Side::Theirs),
            Policy::Conservative,
        );
        assert_eq!((v.class, v.verdict), (class::C6, Verdict::Auto));
        // C7: remove vs trim
        let v = classify_pair(
            &remove(Side::Ours),
            &trim(Side::Theirs, 4, 0),
            Policy::Conservative,
        );
        assert_eq!((v.class, v.verdict), (class::C7, Verdict::Human));
        // C3: trim vs trim different (frame-disjoint edges — still HUMAN
        // under the default policy; this is the opt-in line)
        let v = classify_pair(
            &trim(Side::Ours, 4, 0),
            &trim(Side::Theirs, 0, 4),
            Policy::Conservative,
        );
        assert_eq!((v.class, v.verdict), (class::C3, Verdict::Human));
        // trim vs trim identical → auto-once
        let v = classify_pair(
            &trim(Side::Ours, 4, 0),
            &trim(Side::Theirs, 4, 0),
            Policy::Conservative,
        );
        assert_eq!((v.class, v.verdict), (class::C6, Verdict::Auto));
        // C2: different attr keys
        let v = classify_pair(
            &attr(Side::Ours, AttrKind::Name, "a"),
            &attr(Side::Theirs, AttrKind::Enabled, "false"),
            Policy::Conservative,
        );
        assert_eq!((v.class, v.verdict), (class::C2, Verdict::Auto));
        // C3: same key different values
        let v = classify_pair(
            &attr(Side::Ours, AttrKind::Name, "a"),
            &attr(Side::Theirs, AttrKind::Name, "b"),
            Policy::Conservative,
        );
        assert_eq!((v.class, v.verdict), (class::C3, Verdict::Human));
        // same key same value → once
        let v = classify_pair(
            &attr(Side::Ours, AttrKind::Name, "a"),
            &attr(Side::Theirs, AttrKind::Name, "a"),
            Policy::Conservative,
        );
        assert_eq!((v.class, v.verdict), (class::C6, Verdict::Auto));
        // C8: insert vs insert same slot
        let v = classify_pair(
            &insert(Side::Ours, "X"),
            &insert(Side::Theirs, "Y"),
            Policy::Conservative,
        );
        assert_eq!((v.class, v.verdict), (class::C8, Verdict::AutoNote));
        // identical inserts → once
        let v = classify_pair(
            &insert(Side::Ours, "X"),
            &insert(Side::Theirs, "X"),
            Policy::Conservative,
        );
        assert_eq!((v.class, v.verdict), (class::C6, Verdict::Auto));
        // C5: move vs trim
        let mv = |side: Side| Op::Move {
            side,
            key: k(),
            from: loc(),
            to: TrackLoc::Base(0),
            slot: Slot::EndOf { track: 0 },
        };
        let v = classify_pair(
            &mv(Side::Ours),
            &trim(Side::Theirs, 4, 0),
            Policy::Conservative,
        );
        assert_eq!((v.class, v.verdict), (class::C5, Verdict::Auto));
        // C4: move vs move same target
        let v = classify_pair(&mv(Side::Ours), &mv(Side::Theirs), Policy::Conservative);
        assert_eq!((v.class, v.verdict), (class::C4, Verdict::Auto));
        // C4: move vs move different target
        let mv2 = Op::Move {
            side: Side::Theirs,
            key: k(),
            from: loc(),
            to: TrackLoc::Base(0),
            slot: Slot::Before { track: 0, index: 0 },
        };
        let v = classify_pair(&mv(Side::Ours), &mv2, Policy::Conservative);
        assert_eq!((v.class, v.verdict), (class::C4, Verdict::Human));
    }

    #[test]
    fn semantic_policy_auto_merges_frame_disjoint_trims() {
        // the 100x case (ADR-0023): ours re-cuts the head, theirs re-cuts
        // the tail of the SAME clip — both apply, no human, but a note.
        let v = classify_pair(
            &trim(Side::Ours, 4, 0),
            &trim(Side::Theirs, 0, 4),
            Policy::Semantic,
        );
        assert_eq!((v.class, v.verdict), (class::C11, Verdict::AutoNote));
        assert!(!v.once, "both edges apply — never deduped");
        // mirrored direction
        let v = classify_pair(
            &trim(Side::Ours, 0, 9),
            &trim(Side::Theirs, 2, 0),
            Policy::Semantic,
        );
        assert_eq!((v.class, v.verdict), (class::C11, Verdict::AutoNote));
    }

    #[test]
    fn semantic_policy_still_escalates_same_edge_and_mixed_trims() {
        // "one cut at 00:01:23, the other cut at 00:01:24": both sides
        // re-cut the SAME edge — HUMAN under every policy.
        let v = classify_pair(
            &trim(Side::Ours, 4, 0),
            &trim(Side::Theirs, 2, 0),
            Policy::Semantic,
        );
        assert_eq!((v.class, v.verdict), (class::C3, Verdict::Human));
        let v = classify_pair(
            &trim(Side::Ours, 0, 4),
            &trim(Side::Theirs, 0, 2),
            Policy::Semantic,
        );
        assert_eq!((v.class, v.verdict), (class::C3, Verdict::Human));
        // one side touched BOTH edges → not disjoint, still HUMAN
        let v = classify_pair(
            &trim(Side::Ours, 4, 4),
            &trim(Side::Theirs, 0, 2),
            Policy::Semantic,
        );
        assert_eq!((v.class, v.verdict), (class::C3, Verdict::Human));
        // identical trims remain C6-once under the semantic policy too
        let v = classify_pair(
            &trim(Side::Ours, 4, 0),
            &trim(Side::Theirs, 4, 0),
            Policy::Semantic,
        );
        assert_eq!((v.class, v.verdict), (class::C6, Verdict::Auto));
        // C3 attr conflicts are NOT relaxed by the semantic policy
        let v = classify_pair(
            &attr(Side::Ours, AttrKind::Name, "a"),
            &attr(Side::Theirs, AttrKind::Name, "b"),
            Policy::Semantic,
        );
        assert_eq!((v.class, v.verdict), (class::C3, Verdict::Human));
        // C7 remove-vs-edit is NOT relaxed either
        let v = classify_pair(
            &remove(Side::Ours),
            &trim(Side::Theirs, 0, 2),
            Policy::Semantic,
        );
        assert_eq!((v.class, v.verdict), (class::C7, Verdict::Human));
    }

    #[test]
    fn interacts_by_key_and_anchor() {
        assert!(interacts(&remove(Side::Ours), &trim(Side::Theirs, 1, 1)));
        assert!(interacts(
            &insert(Side::Ours, "X"),
            &insert(Side::Theirs, "Y")
        ));
        let other = Op::Remove {
            side: Side::Theirs,
            key: ElementKey("uuid:other".into()),
            base: (0, 2),
        };
        assert!(!interacts(&trim(Side::Ours, 1, 1), &other));
    }

    #[test]
    fn track_remove_contains_items() {
        let tr = Op::TrackRemove {
            side: Side::Ours,
            track: 1,
        };
        let inside = Op::Trim {
            side: Side::Theirs,
            key: k(),
            base: (1, 0),
            in_delta: Rational::ZERO,
            out_delta: Rational::new(1, 24).unwrap(),
        };
        assert!(interacts(&tr, &inside));
        let v = classify_pair(&tr, &inside, Policy::Conservative);
        assert_eq!((v.class, v.verdict), (class::C9, Verdict::Human));
    }
}
