//! Typed op extraction (ADR-0015 §2.3): each side diffs against base into
//! ops carrying **base-coordinate locators** + identity keys, so the apply
//! phase never recomputes identity (the silent-loss risk) and every op's
//! position survives the other side's structural edits.
//!
//! Extraction rules that keep the op set honest:
//! - `Move` = one identity, different location within ONE side (remove+
//!   insert of the same key would double-count).
//! - Elements moved INTO a new side track are reclassified as `Move` ops
//!   targeting [`TrackLoc::New`] — the `TrackAdd` carries only genuinely
//!   new elements (the bug class where moves hide inside TrackAdd).
//! - Diffs on fields the model does not structurally interpret (effects,
//!   metadata, extra) surface as `Attr` ops with a raw path — never dropped.
//! - A rung-(d) positional match that MOVED escalates immediately (C10).

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use crate::identity::{self, ElementKey, Flat};
use crate::model::{marker_uuid, Element, Kind, Marker, Timeline};
use crate::rational::Rational;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Side {
    Ours,
    Theirs,
}

/// Where an op's track target lives: a base track, or a NEW track created by
/// the op list itself (`ordinal` = the n-th NEW track of that side, in op
/// order).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TrackLoc {
    Base(usize),
    New { side: Side, ordinal: usize },
}

/// Insertion anchor in base coordinates: before the base item at
/// (track, index), or end-of-track. For new tracks: ordinal position.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Slot {
    Before { track: usize, index: usize },
    EndOf { track: usize },
    NewTrackOrdinal { ordinal: usize },
}

/// Whitelisted attribute table (ADR §3 C2/C3) + raw passthrough paths.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum AttrKind {
    Name,
    Enabled,
    /// Clip relink (active media target changed).
    MediaTarget,
    /// Effects-list change (speed/opacity/filters — raw JSON).
    Effects,
    /// Raw path (metadata / extra / anything preserved verbatim).
    Raw(String),
}

impl AttrKind {
    pub fn as_str(&self) -> String {
        match self {
            AttrKind::Name => "name".into(),
            AttrKind::Enabled => "enabled".into(),
            AttrKind::MediaTarget => "media_target".into(),
            AttrKind::Effects => "effects".into(),
            AttrKind::Raw(p) => format!("raw:{p}"),
        }
    }
}

#[derive(Clone, Debug)]
pub enum Op {
    Insert {
        side: Side,
        element: Element,
        to: TrackLoc,
        slot: Slot,
    },
    Remove {
        side: Side,
        key: ElementKey,
        base: (usize, usize),
    },
    Move {
        side: Side,
        key: ElementKey,
        from: (usize, usize),
        to: TrackLoc,
        slot: Slot,
    },
    Trim {
        side: Side,
        key: ElementKey,
        base: (usize, usize),
        in_delta: Rational,
        out_delta: Rational,
    },
    Attr {
        side: Side,
        key: ElementKey,
        base: (usize, usize),
        attr: AttrKind,
        value: Value,
    },
    MarkerAdd {
        side: Side,
        key: ElementKey,
        base: (usize, usize),
        marker: Marker,
    },
    MarkerRemove {
        side: Side,
        key: ElementKey,
        base: (usize, usize),
        marker_key: String,
    },
    TrackAdd {
        side: Side,
        ordinal: usize,
        track: Element,
        slot: Slot,
    },
    TrackRemove {
        side: Side,
        track: usize,
    },
    /// Attribute edit on a MATCHED TRACK itself (name/enabled/metadata/extra).
    /// Round 13 real-corpus catch: track-level edits were previously invisible
    /// to the diff — both sides' track renames silently vanished. The op is
    /// keyed by the BASE track index (matched tracks are stable across sides).
    TrackAttr {
        side: Side,
        track: usize,
        attr: AttrKind,
        value: Value,
    },
    /// The side's full ordering of matched base tracks (reorder), base indices.
    TrackReorder {
        side: Side,
        order: Vec<usize>,
    },
}

impl Op {
    pub fn side(&self) -> Side {
        match self {
            Op::Insert { side, .. }
            | Op::Remove { side, .. }
            | Op::Move { side, .. }
            | Op::Trim { side, .. }
            | Op::Attr { side, .. }
            | Op::MarkerAdd { side, .. }
            | Op::MarkerRemove { side, .. }
            | Op::TrackAdd { side, .. }
            | Op::TrackRemove { side, .. }
            | Op::TrackAttr { side, .. }
            | Op::TrackReorder { side, .. } => *side,
        }
    }

    /// The element identity this op touches (None for track-level structural
    /// ops and inserts, which are keyed by their anchor instead).
    pub fn element_key(&self) -> Option<&ElementKey> {
        match self {
            Op::Remove { key, .. }
            | Op::Move { key, .. }
            | Op::Trim { key, .. }
            | Op::Attr { key, .. }
            | Op::MarkerAdd { key, .. }
            | Op::MarkerRemove { key, .. } => Some(key),
            Op::Insert { .. }
            | Op::TrackAdd { .. }
            | Op::TrackRemove { .. }
            | Op::TrackAttr { .. }
            | Op::TrackReorder { .. } => None,
        }
    }

    /// Structural ops are the ones rung (d) cannot carry (C10 escalation).
    pub fn is_structural(&self) -> bool {
        matches!(
            self,
            Op::Insert { .. }
                | Op::Remove { .. }
                | Op::Move { .. }
                | Op::TrackAdd { .. }
                | Op::TrackRemove { .. }
        )
    }

    /// Insertion target (anchor) for insert/move/track-add ops.
    pub fn anchor(&self) -> Option<(TrackLoc, Slot)> {
        match self {
            Op::Insert { to, slot, .. } => Some((*to, *slot)),
            Op::Move { to, slot, .. } => Some((*to, *slot)),
            Op::TrackAdd { slot, .. } => Some((TrackLoc::Base(usize::MAX), *slot)),
            _ => None,
        }
    }

    /// Human-readable one-line summary for the report.
    pub fn summary(&self) -> String {
        match self {
            Op::Insert { element, .. } => format!("insert {}", el_name(element)),
            Op::Remove { key, .. } => format!("remove {}", key.0),
            Op::Move { key, .. } => format!("move {}", key.0),
            Op::Trim {
                key,
                in_delta,
                out_delta,
                ..
            } => {
                format!(
                    "trim {} in={}s out={}s",
                    key.0,
                    in_delta.to_f64_approx(),
                    out_delta.to_f64_approx()
                )
            }
            Op::Attr { key, attr, .. } => format!("attr {} {}", key.0, attr.as_str()),
            Op::MarkerAdd { key, marker, .. } => format!("marker+ {} on {}", marker.name, key.0),
            Op::MarkerRemove {
                key, marker_key, ..
            } => format!("marker- {marker_key} on {}", key.0),
            Op::TrackAdd { track, .. } => format!("track+ {}", el_name(track)),
            Op::TrackRemove { track, .. } => format!("track- {track}"),
            Op::TrackAttr { track, attr, .. } => {
                format!("track-attr #{track} {}", attr.as_str())
            }
            Op::TrackReorder { order, .. } => format!("reorder {order:?}"),
        }
    }
}

fn el_name(e: &Element) -> String {
    let kind = match &e.kind {
        Kind::Clip => "clip",
        Kind::Gap => "gap",
        Kind::Stack => "stack",
        Kind::Track(_) => "track",
        Kind::Transition => "transition",
        Kind::Unknown(t) => return format!("?{t}? {}", e.name),
    };
    format!("{kind} {}", e.name)
}

/// C10 escalation out of extraction (ladder collapse on structural moves).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Escalate(pub String);

/// Extract one side's ops vs base.
pub fn extract_ops(side: Side, base: &Flat, side_flat: &Flat) -> Result<Vec<Op>, Escalate> {
    let mut ops = Vec::new();
    let (side_to_base, new_tracks, removed_tracks) = identity::match_tracks(base, side_flat);
    let (matches, inserts, _removes) = identity::match_docs(base, side_flat);

    // ---- track-level ops ----
    // new tracks (in side stack order); ordinal counts per side
    let mut new_ord: HashMap<usize, usize> = HashMap::new();
    for (n, &side_ti) in new_tracks.iter().enumerate() {
        let track_el = &side_flat.tracks[side_ti].element;
        let mut stripped = track_el.clone();
        stripped.children.clear(); // children become Insert/Move ops
                                   // stack slot: before the base track that follows the new one
        let slot = track_stack_slot(side_flat, side_ti, &side_to_base);
        new_ord.insert(side_ti, n);
        ops.push(Op::TrackAdd {
            side,
            ordinal: n,
            track: stripped,
            slot,
        });
    }
    for &base_ti in &removed_tracks {
        ops.push(Op::TrackRemove {
            side,
            track: base_ti,
        });
    }
    // reorder: side's ordering of matched base tracks vs identity order
    let side_order: Vec<usize> = (0..side_flat.tracks.len())
        .filter_map(|i| side_to_base[i])
        .collect();
    let base_order: Vec<usize> = (0..base.tracks.len()).collect();
    if side_order != base_order && !side_order.is_empty() {
        ops.push(Op::TrackReorder {
            side,
            order: side_order,
        });
    }

    // ---- matched-track attrs (Round 13 real-corpus catch) ----
    // Track-level edits (rename a track, disable it, touch its metadata)
    // previously produced NO ops — both sides' edits silently vanished. Now
    // every matched track pair diffs its own fields, keyed by base index.
    for (side_ti, base_ti) in side_to_base.iter().enumerate() {
        let Some(bt) = base_ti else { continue };
        let base_el = &base.tracks[*bt].element;
        let side_el = &side_flat.tracks[side_ti].element;
        if side_el.name != base_el.name {
            ops.push(Op::TrackAttr {
                side,
                track: *bt,
                attr: AttrKind::Name,
                value: Value::String(side_el.name.clone()),
            });
        }
        if side_el.enabled != base_el.enabled {
            ops.push(Op::TrackAttr {
                side,
                track: *bt,
                attr: AttrKind::Enabled,
                value: Value::Bool(side_el.enabled),
            });
        }
        if side_el.metadata != base_el.metadata {
            ops.push(Op::TrackAttr {
                side,
                track: *bt,
                attr: AttrKind::Raw("metadata".into()),
                value: serde_json::to_value(&side_el.metadata).unwrap_or(Value::Null),
            });
        }
        if side_el.extra != base_el.extra {
            ops.push(Op::TrackAttr {
                side,
                track: *bt,
                attr: AttrKind::Raw("extra".into()),
                value: serde_json::to_value(&side_el.extra).unwrap_or(Value::Null),
            });
        }
    }

    // ---- item-level: matched pairs ----
    // Move detection is SEQUENCE-AWARE: an element "moved" only if its
    // relative order among matched siblings changed (LIS of base indices in
    // side order), or it crossed tracks. Insert-shifted indices are NOT
    // moves — the bug class where [A,B] + insert-before-B yields a phantom
    // Move(B) and a duplicated element at apply time.
    for m in &matches {
        let (Some((bt, bi)), (st, si)) = (m.base, m.side) else {
            continue;
        };
        let side_el = &side_flat.tracks[st].items[si];
        let base_el = &base.tracks[bt].items[bi];
        let key = identity::base_key(base_el, bt, bi);
        let same_track = side_to_base.get(st).copied().flatten() == Some(bt);
        let moved = if same_track {
            // per-track sequence check via LIS
            let moved_set = track_moves(base, side_flat, &matches, st, bt);
            moved_set.contains(&bi)
        } else {
            true // cross-track relocation is always a move
        };
        if moved {
            if m.rung.is_positional() {
                return Err(Escalate(format!(
                    "positional-identity element moved ({}) — ladder collapse, refusing",
                    key.0
                )));
            }
            let (to, slot) = if new_tracks.contains(&st) {
                let ordinal = new_ord[&st];
                let slot = new_track_slot(side_flat, st, si, &matches);
                (TrackLoc::New { side, ordinal }, slot)
            } else {
                let to = TrackLoc::Base(bt_of(&side_to_base, st, bt));
                let slot = item_slot(side_flat, st, si, &matches);
                (to, slot)
            };
            ops.push(Op::Move {
                side,
                key: key.clone(),
                from: (bt, bi),
                to,
                slot,
            });
        }
        // content diffs run for EVERY matched pair (moved or stationary):
        // a moved element can also be trimmed/relabeled
        ops.extend(item_diff_ops(side, key, (bt, bi), base_el, side_el));
    }

    // ---- item-level: inserts ----
    for &(st, si) in &inserts {
        let element = side_flat.tracks[st].items[si].clone();
        if new_tracks.contains(&st) {
            let ordinal = new_ord[&st];
            let slot = new_track_slot(side_flat, st, si, &matches);
            ops.push(Op::Insert {
                side,
                element,
                to: TrackLoc::New { side, ordinal },
                slot,
            });
        } else {
            let (to, slot) = item_insert_loc(side_flat, st, si, &side_to_base, &matches);
            ops.push(Op::Insert {
                side,
                element,
                to,
                slot,
            });
        }
    }

    // ---- item-level: removes ----
    for (bt, t) in base.tracks.iter().enumerate() {
        if removed_tracks.contains(&bt) {
            continue; // whole track is going away
        }
        for bi in 0..t.items.len() {
            let matched = matches.iter().any(|m| m.base == Some((bt, bi)));
            if !matched {
                let base_el = &t.items[bi];
                let key = identity::base_key(base_el, bt, bi);
                ops.push(Op::Remove {
                    side,
                    key,
                    base: (bt, bi),
                });
            }
        }
    }

    // deterministic order: TrackAdd/TrackRemove/TrackReorder first (already),
    // then item ops in base-coordinate order, inserts/moves anchored by base order
    ops.sort_by_key(|op| match op {
        Op::TrackAdd { .. } | Op::TrackRemove { .. } | Op::TrackReorder { .. } => {
            (usize::MAX, usize::MAX)
        }
        Op::TrackAttr { track, .. } => (usize::MAX - 1, *track),
        Op::Insert { slot, .. } | Op::Move { slot, .. } => slot_key(slot),
        Op::Remove { base, .. }
        | Op::Trim { base, .. }
        | Op::Attr { base, .. }
        | Op::MarkerAdd { base, .. }
        | Op::MarkerRemove { base, .. } => *base,
    });
    Ok(ops)
}

/// Base-item indices that CHANGED relative order within one track pair —
/// the complement of the longest increasing subsequence of base indices in
/// side order (patience method, O(n log n); matched pairs are 1:1).
fn track_moves(
    base: &Flat,
    side_flat: &Flat,
    matches: &[identity::Matched],
    side_ti: usize,
    base_ti: usize,
) -> HashSet<usize> {
    // matched base indices in SIDE order for this track pair
    let mut seq: Vec<usize> = Vec::new();
    for (si, _) in side_flat.tracks[side_ti].items.iter().enumerate() {
        if let Some(m) = matches.iter().find(|m| m.side == (side_ti, si)) {
            if m.base.is_some_and(|(bt, _)| bt == base_ti) {
                seq.push(m.base.unwrap().1);
            }
        }
    }
    let _ = base;
    // LIS over seq (strictly increasing base indices = stationary)
    let mut tails: Vec<usize> = Vec::new(); // tails[k] = smallest tail of an LIS of length k+1
    for &x in &seq {
        let pos = tails.partition_point(|&t| t < x);
        if pos == tails.len() {
            tails.push(x);
        } else {
            tails[pos] = x;
        }
    }
    // stationary set = the values in the final tails
    let stationary: HashSet<usize> = tails.iter().copied().collect();
    seq.into_iter()
        .filter(|bi| !stationary.contains(bi))
        .collect()
}

fn slot_key(slot: &Slot) -> (usize, usize) {
    match slot {
        Slot::Before { track, index } => (*track, *index),
        Slot::EndOf { track } => (*track, usize::MAX - 1),
        Slot::NewTrackOrdinal { ordinal } => (usize::MAX - 2 - *ordinal, 0),
    }
}

fn bt_of(side_to_base: &[Option<usize>], side_ti: usize, fallback: usize) -> usize {
    side_to_base
        .get(side_ti)
        .copied()
        .flatten()
        .unwrap_or(fallback)
}

/// Content diff of a matched, same-location pair → Trim/Attr/Marker ops.
fn item_diff_ops(
    side: Side,
    key: ElementKey,
    base_loc: (usize, usize),
    base_el: &Element,
    side_el: &Element,
) -> Vec<Op> {
    let mut ops = Vec::new();
    // name
    if base_el.name != side_el.name {
        ops.push(Op::Attr {
            side,
            key: key.clone(),
            base: base_loc,
            attr: AttrKind::Name,
            value: Value::String(side_el.name.clone()),
        });
    }
    // enabled
    if base_el.enabled != side_el.enabled {
        ops.push(Op::Attr {
            side,
            key: key.clone(),
            base: base_loc,
            attr: AttrKind::Enabled,
            value: Value::Bool(side_el.enabled),
        });
    }
    // media relink
    if base_el.active_media_url() != side_el.active_media_url() {
        ops.push(Op::Attr {
            side,
            key: key.clone(),
            base: base_loc,
            attr: AttrKind::MediaTarget,
            value: Value::String(side_el.active_media_url().unwrap_or_default()),
        });
    }
    // trim (source_range deltas — exact)
    if let Some(diff) = trim_diff(base_el, side_el) {
        let (in_delta, out_delta) = diff;
        ops.push(Op::Trim {
            side,
            key: key.clone(),
            base: base_loc,
            in_delta,
            out_delta,
        });
    } else if base_el.source_range != side_el.source_range {
        // appear/disappear or rate change: raw attr carrying the SIDE's range
        ops.push(Op::Attr {
            side,
            key: key.clone(),
            base: base_loc,
            attr: AttrKind::Raw("source_range".into()),
            value: side_el
                .source_range
                .as_ref()
                .map(crate::canon::range_value)
                .unwrap_or(Value::Null),
        });
    }
    // effects list — the side's full value (apply sets it verbatim)
    if base_el.effects != side_el.effects {
        ops.push(Op::Attr {
            side,
            key: key.clone(),
            base: base_loc,
            attr: AttrKind::Effects,
            value: crate::canon::effects_value(&side_el.effects),
        });
    }
    // metadata diff (excluding cairn identity stamps)
    let base_meta = sans_cairn(&base_el.metadata);
    let side_meta = sans_cairn(&side_el.metadata);
    if base_meta != side_meta {
        ops.push(Op::Attr {
            side,
            key: key.clone(),
            base: base_loc,
            attr: AttrKind::Raw("metadata".into()),
            value: crate::canon::map_value(&side_meta),
        });
    }
    // extra diff
    if base_el.extra != side_el.extra {
        ops.push(Op::Attr {
            side,
            key: key.clone(),
            base: base_loc,
            attr: AttrKind::Raw("extra".into()),
            value: crate::canon::map_value(&side_el.extra),
        });
    }
    // markers
    ops.extend(marker_ops(side, &key, base_loc, base_el, side_el));
    ops
}

fn sans_cairn(map: &crate::model::JsonMap) -> crate::model::JsonMap {
    let mut out = map.clone();
    out.remove("cairn");
    out
}

fn trim_diff(base_el: &Element, side_el: &Element) -> Option<(Rational, Rational)> {
    let (b, s) = (
        base_el.source_range.as_ref()?,
        side_el.source_range.as_ref()?,
    );
    let b_start = b.start.seconds().ok()?;
    let b_dur = b.duration.seconds().ok()?;
    let s_start = s.start.seconds().ok()?;
    let s_dur = s.duration.seconds().ok()?;
    let in_delta = s_start.checked_sub(b_start).ok()?;
    let b_end = b_start.checked_add(b_dur).ok()?;
    let s_end = s_start.checked_add(s_dur).ok()?;
    let out_delta = b_end.checked_sub(s_end).ok()?;
    if in_delta.is_zero() && out_delta.is_zero() {
        return None; // identical times: no trim
    }
    Some((in_delta, out_delta))
}

fn marker_ops(
    side: Side,
    key: &ElementKey,
    base_loc: (usize, usize),
    base_el: &Element,
    side_el: &Element,
) -> Vec<Op> {
    let mut ops = Vec::new();
    let ident = |m: &Marker| marker_uuid(m).unwrap_or_else(|| format!("{}|{}", m.name, m.comment));
    let base_ids: Vec<String> = base_el.markers.iter().map(ident).collect();
    let side_ids: Vec<String> = side_el.markers.iter().map(ident).collect();
    for m in &side_el.markers {
        let id = ident(m);
        if !base_ids.contains(&id) {
            ops.push(Op::MarkerAdd {
                side,
                key: key.clone(),
                base: base_loc,
                marker: m.clone(),
            });
        }
    }
    for m in &base_el.markers {
        let id = ident(m);
        if !side_ids.contains(&id) {
            ops.push(Op::MarkerRemove {
                side,
                key: key.clone(),
                base: base_loc,
                marker_key: id,
            });
        }
    }
    ops
}

/// Anchor for a MOVE of the element at side (st, si): the first MATCHED side
/// item STRICTLY AFTER si (the moved element must not anchor at itself —
/// self-anchored moves are no-ops at apply time).
fn item_slot(side_flat: &Flat, st: usize, si: usize, matches: &[identity::Matched]) -> Slot {
    let track = &side_flat.tracks[st];
    for j in (si + 1)..track.items.len() {
        if let Some(m) = matches.iter().find(|m| m.side == (st, j)) {
            if let Some((bt, bi)) = m.base {
                return Slot::Before {
                    track: bt,
                    index: bi,
                };
            }
        }
    }
    Slot::EndOf { track: st }
}

/// Insert location: track (base-mapped) + anchor slot.
fn item_insert_loc(
    side_flat: &Flat,
    st: usize,
    si: usize,
    side_to_base: &[Option<usize>],
    matches: &[identity::Matched],
) -> (TrackLoc, Slot) {
    let base_ti = side_to_base.get(st).copied().flatten();
    let to = match base_ti {
        Some(bt) => TrackLoc::Base(bt),
        None => TrackLoc::Base(st), // defensive: unmatched track
    };
    // anchor: first matched side item at/after si → its base coords
    let track = &side_flat.tracks[st];
    for j in si..track.items.len() {
        if let Some(m) = matches.iter().find(|m| m.side == (st, j)) {
            if let Some((bt, bi)) = m.base {
                return (
                    to,
                    Slot::Before {
                        track: bt,
                        index: bi,
                    },
                );
            }
        }
    }
    match base_ti {
        Some(bt) => (to, Slot::EndOf { track: bt }),
        None => (to, Slot::EndOf { track: st }),
    }
}

/// Slot inside a NEW track: ordinal position in the side's new-track item list.
fn new_track_slot(
    _side_flat: &Flat,
    _st: usize,
    si: usize,
    _matches: &[identity::Matched],
) -> Slot {
    Slot::NewTrackOrdinal { ordinal: si }
}

/// Stack-level slot for a new track: before the base track that the side's
/// NEXT matched track maps to.
fn track_stack_slot(_side_flat: &Flat, side_ti: usize, side_to_base: &[Option<usize>]) -> Slot {
    side_to_base
        .iter()
        .skip(side_ti + 1)
        .flatten()
        .next()
        .map_or(Slot::EndOf { track: usize::MAX }, |bt| Slot::Before {
            track: *bt,
            index: usize::MAX,
        })
}

/// Convenience: extract from `Timeline`s (flatten + extract).
pub fn extract_from(side: Side, base: &Timeline, side_tl: &Timeline) -> Result<Vec<Op>, Escalate> {
    let base_flat = identity::flatten(base);
    let side_flat = identity::flatten(side_tl);
    extract_ops(side, &base_flat, &side_flat)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::flatten;
    use crate::model::*;

    fn tv(v: i128, r: i128) -> TimeVal {
        TimeVal {
            value: Rational::new(v, 1).unwrap(),
            rate: Rational::new(r, 1).unwrap(),
        }
    }

    fn clip(name: &str, url: &str, start: i128, dur: i128) -> Element {
        let mut c = Element::leaf(Kind::Clip, name);
        c.media = Some(MediaRef::single(
            MediaKind::External,
            String::new(),
            Some(url.into()),
        ));
        c.source_range = Some(TimeRange {
            start: tv(start, 24),
            duration: tv(dur, 24),
        });
        c
    }

    fn doc(tracks: Vec<(String, Vec<Element>)>) -> Timeline {
        let track_els: Vec<Element> = tracks
            .into_iter()
            .map(|(name, items)| Element::container(Kind::Track(TrackKind::Video), name, items))
            .collect();
        Timeline {
            name: "x".into(),
            global_start_time: None,
            metadata: JsonMap::new(),
            tracks: Element::container(Kind::Stack, "tracks", track_els),
            extra: JsonMap::new(),
        }
    }

    fn stamp_all_uuid(tl: &mut Timeline) {
        tl.walk_mut(|e| {
            if e.cairn_uuid().is_none() {
                let id = uuid::Uuid::now_v7().to_string();
                e.stamp_uuid(&id);
            }
        });
    }

    #[test]
    fn insert_and_remove_extraction() {
        let mut base = doc(vec![(
            "V1".into(),
            vec![clip("A", "a", 0, 24), clip("B", "b", 0, 24)],
        )]);
        stamp_all_uuid(&mut base);
        let mut side = base.clone();
        side.tracks.children[0].children.remove(0); // remove A
        side.tracks.children[0].children.push(clip("C", "c", 0, 24)); // insert C
        stamp_all_uuid(&mut side);

        let ops = extract_from(Side::Theirs, &base, &side).unwrap();
        let inserts = ops
            .iter()
            .filter(|o| matches!(o, Op::Insert { .. }))
            .count();
        let removes = ops
            .iter()
            .filter(|o| matches!(o, Op::Remove { .. }))
            .count();
        assert_eq!(inserts, 1);
        assert_eq!(removes, 1);
        // insert anchored at END (appended after B)
        match ops.iter().find(|o| matches!(o, Op::Insert { .. })) {
            Some(Op::Insert { slot, .. }) => assert!(matches!(slot, Slot::EndOf { .. })),
            _ => panic!("no insert"),
        }
    }

    #[test]
    fn move_extraction() {
        let mut base = doc(vec![(
            "V1".into(),
            vec![clip("A", "a", 0, 24), clip("B", "b", 0, 24)],
        )]);
        stamp_all_uuid(&mut base);
        let mut side = base.clone();
        let items = &mut side.tracks.children[0].children;
        let a = items.remove(0);
        items.insert(1, a); // A moved after B
        let ops = extract_from(Side::Ours, &base, &side).unwrap();
        assert_eq!(
            ops.iter().filter(|o| matches!(o, Op::Move { .. })).count(),
            1
        );
        assert_eq!(
            ops.iter()
                .filter(|o| matches!(o, Op::Insert { .. }))
                .count(),
            0
        );
        assert_eq!(
            ops.iter()
                .filter(|o| matches!(o, Op::Remove { .. }))
                .count(),
            0
        );
    }

    #[test]
    fn move_into_new_track_is_move_not_trackadd_children() {
        let mut base = doc(vec![(
            "V1".into(),
            vec![clip("A", "a", 0, 24), clip("B", "b", 0, 24)],
        )]);
        stamp_all_uuid(&mut base);
        let mut side = base.clone();
        // move B into a NEW track V2
        let b = side.tracks.children[0].children.remove(1);
        let mut v2 = Element::container(Kind::Track(TrackKind::Video), "V2", vec![b]);
        let v2_uuid = v2.cairn_uuid().unwrap_or("t-v2").to_string();
        v2.stamp_uuid(&v2_uuid);
        side.tracks.children.push(v2);
        let ops = extract_from(Side::Ours, &base, &side).unwrap();
        let track_adds: Vec<&Op> = ops
            .iter()
            .filter(|o| matches!(o, Op::TrackAdd { .. }))
            .collect();
        assert_eq!(track_adds.len(), 1);
        match track_adds[0] {
            Op::TrackAdd { track, .. } => {
                assert!(track.children.is_empty(), "TrackAdd must carry no children");
            }
            _ => panic!(),
        }
        assert_eq!(
            ops.iter().filter(|o| matches!(o, Op::Move { .. })).count(),
            1,
            "B must be a Move into the new track"
        );
    }

    #[test]
    fn trim_extraction_exact() {
        let mut base = doc(vec![("V1".into(), vec![clip("A", "a", 24, 96)])]);
        stamp_all_uuid(&mut base);
        let mut side = base.clone();
        // side: in-point later by 12 frames, out-point earlier by 12 frames
        side.tracks.children[0].children[0].source_range = Some(TimeRange {
            start: tv(36, 24),
            duration: tv(72, 24),
        });
        let ops = extract_from(Side::Theirs, &base, &side).unwrap();
        match ops.iter().find(|o| matches!(o, Op::Trim { .. })) {
            Some(Op::Trim {
                in_delta,
                out_delta,
                ..
            }) => {
                assert_eq!(*in_delta, Rational::new(1, 2).unwrap()); // 12/24 s
                assert_eq!(*out_delta, Rational::new(1, 2).unwrap());
            }
            _ => panic!("no trim op"),
        }
    }

    #[test]
    fn attr_and_marker_extraction() {
        let mut base = doc(vec![("V1".into(), vec![clip("A", "a", 0, 24)])]);
        stamp_all_uuid(&mut base);
        let mut side = base.clone();
        side.tracks.children[0].children[0].name = "Renamed".into();
        side.tracks.children[0].children[0].enabled = false;
        side.tracks.children[0].children[0].markers.push(Marker {
            schema: "Marker.2".into(),
            name: "flag".into(),
            color: "RED".into(),
            comment: "x".into(),
            marked_range: TimeRange {
                start: tv(0, 24),
                duration: tv(0, 24),
            },
            metadata: JsonMap::new(),
            extra: JsonMap::new(),
        });
        let ops = extract_from(Side::Ours, &base, &side).unwrap();
        assert_eq!(
            ops.iter()
                .filter(|o| matches!(
                    o,
                    Op::Attr {
                        attr: AttrKind::Name,
                        ..
                    }
                ))
                .count(),
            1
        );
        assert_eq!(
            ops.iter()
                .filter(|o| matches!(
                    o,
                    Op::Attr {
                        attr: AttrKind::Enabled,
                        ..
                    }
                ))
                .count(),
            1
        );
        assert_eq!(
            ops.iter()
                .filter(|o| matches!(o, Op::MarkerAdd { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn positional_gap_move_escalates() {
        // a CONTENTLESS gap (no uuid/name/range) matches on rung (d) only;
        // moving it is a structural op on positional identity → C10.
        let blank = Element::leaf(Kind::Gap, "");
        let mut x = clip("X", "x", 0, 24);
        x.stamp_uuid("u-x");
        // X FIRST: the LIS then picks the blank as the mover (a positional
        // element carrying a structural op → C10). With blank first, the LIS
        // picks X (uuid-matched) as the mover — also correct, no escalation.
        let base = doc(vec![("V1".into(), vec![x, blank])]);
        let mut side = base.clone();
        let items = &mut side.tracks.children[0].children;
        let one = items.remove(0);
        items.push(one); // positional match but MOVED → must escalate
        let result = extract_from(Side::Ours, &base, &side);
        assert!(result.is_err(), "positional move must escalate C10");
    }

    #[test]
    fn fixture_extracts_cleanly() {
        let base =
            crate::parse::parse_otio(include_str!("../fixtures/roundtrip_base.otio")).unwrap();
        let flat = flatten(&base);
        let ops = extract_ops(Side::Ours, &flat, &flat).unwrap();
        assert!(ops.is_empty(), "identity diff must produce zero ops");
    }
}
