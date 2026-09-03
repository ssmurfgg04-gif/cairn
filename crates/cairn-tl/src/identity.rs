//! Identity ladder + document flattening (ADR-0015 §2.2–2.3).
//!
//! The coordinate system: Stack → Track → child index. `Flat` snapshots a
//! document into that view once (O(elements)); every later lookup is a map
//! hit, never a tree walk — the ADR's "never a quadratic document diff".
//!
//! The ladder (strongest first), per element:
//! (a) `metadata.cairn.uuid`
//! (b) OTIO `name` + parent track path
//! (c) content fingerprint (media URL + in/out + kind)
//! (d) unlabeled-and-contentless → position-only (weakest; ANY structural op
//!     on a rung-(d) match escalates C10)
//!
//! Matching runs in three passes so a rung-(c) fingerprint hit can rescue a
//! rung-(b) miss (renames) — the single-key ladder bug class is structurally
//! impossible here: uuids match first, names second, fingerprints third, and
//! only the residue falls through.

use std::collections::HashMap;

use crate::model::{Element, Kind, Timeline, TrackKind};

/// A flattened track: the track element + its items (children).
#[derive(Clone, Debug)]
pub struct TrackFlat {
    pub element: Element,
    pub items: Vec<Element>,
}

/// The flat view of a timeline: stack-level tracks only (nested stacks are
/// treated as atomic items of their parent track — diffed whole-subtree).
#[derive(Clone, Debug)]
pub struct Flat {
    pub name: String,
    pub tracks: Vec<TrackFlat>,
}

/// Which identity rung matched a base↔side pair.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Rung {
    Uuid,
    NamePath,
    Fingerprint,
    Positional,
}

impl Rung {
    /// Rung (d) collapses on structural ops → C10 (ADR §3).
    pub fn is_positional(self) -> bool {
        matches!(self, Rung::Positional)
    }
}

/// One matched pair: base location ↔ side location for the SAME logical
/// element. `base: None` = the side element is NEW (insert candidate).
#[derive(Clone, Debug)]
pub struct Matched {
    /// (track index, item index) in the BASE flat doc.
    pub base: Option<(usize, usize)>,
    /// (track index, item index) in the SIDE flat doc.
    pub side: (usize, usize),
    pub rung: Rung,
}

/// Element identity key — the strongest available, used as the op locator's
/// identity half. Format: `uuid:<u>` | `name:<path>` | `fp:<fingerprint>` |
/// `pos:<track>:<index>` (rung (d) only).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ElementKey(pub String);

impl ElementKey {
    pub fn kind(&self) -> &str {
        self.0.split(':').next().unwrap_or("?")
    }
}

/// Flatten a timeline into the merge coordinate system.
pub fn flatten(tl: &Timeline) -> Flat {
    let mut tracks = Vec::new();
    for child in &tl.tracks.children {
        match &child.kind {
            Kind::Track(_) => tracks.push(TrackFlat {
                element: child.clone(),
                items: child.children.clone(),
            }),
            _ => {
                // non-track at stack level (rare): wrap as a single-item
                // pseudo-track so nothing is silently dropped
                let mut pseudo = Element::leaf(Kind::Track(TrackKind::Video), "stack-attic");
                pseudo.metadata = child.metadata.clone();
                pseudo.children = vec![child.clone()];
                tracks.push(TrackFlat {
                    element: pseudo,
                    items: vec![child.clone()],
                });
            }
        }
    }
    Flat {
        name: tl.name.clone(),
        tracks,
    }
}

/// Match results: (matches, new side elements, removed base elements).
pub type MatchOutcome = (Vec<Matched>, Vec<(usize, usize)>, Vec<(usize, usize)>);

/// Run the full ladder: base flat ↔ side flat → matches + inserts + removes.
///
/// Returns (matches, new_side_elements, removed_base). Moves are matches whose
/// base and side locations differ.
pub fn match_docs(base: &Flat, side: &Flat) -> MatchOutcome {
    let mut matches: Vec<Matched> = Vec::new();
    let mut matched_base: Vec<Vec<bool>> = base
        .tracks
        .iter()
        .map(|t| vec![false; t.items.len()])
        .collect();
    let mut matched_side: Vec<Vec<bool>> = side
        .tracks
        .iter()
        .map(|t| vec![false; t.items.len()])
        .collect();

    // ---- rung (a): uuid ----
    let mut base_by_uuid: HashMap<String, (usize, usize)> = HashMap::new();
    for (ti, t) in base.tracks.iter().enumerate() {
        for (ii, item) in t.items.iter().enumerate() {
            if let Some(u) = item.cairn_uuid() {
                base_by_uuid.insert(u.to_string(), (ti, ii));
            }
        }
    }
    for (ti, t) in side.tracks.iter().enumerate() {
        for (ii, item) in t.items.iter().enumerate() {
            if let Some(u) = item.cairn_uuid() {
                if let Some(loc) = base_by_uuid.get(u) {
                    if !matched_base[loc.0][loc.1] {
                        matches.push(Matched {
                            base: Some(*loc),
                            side: (ti, ii),
                            rung: Rung::Uuid,
                        });
                        matched_base[loc.0][loc.1] = true;
                        matched_side[ti][ii] = true;
                    }
                }
            }
        }
    }

    // ---- rung (b): name + parent track path ----
    let mut base_by_name: HashMap<(String, String), Vec<(usize, usize)>> = HashMap::new();
    for (ti, t) in base.tracks.iter().enumerate() {
        for (ii, item) in t.items.iter().enumerate() {
            if !matched_base[ti][ii] && !item.name.is_empty() {
                base_by_name
                    .entry((item.name.clone(), t.element.name.clone()))
                    .or_default()
                    .push((ti, ii));
            }
        }
    }
    for (ti, t) in side.tracks.iter().enumerate() {
        for (ii, item) in t.items.iter().enumerate() {
            if matched_side[ti][ii] || item.name.is_empty() {
                continue;
            }
            let key = (item.name.clone(), t.element.name.clone());
            if let Some(cands) = base_by_name.get_mut(&key) {
                if let Some(pos) = cands.iter().position(|&(bt, bi)| !matched_base[bt][bi]) {
                    let loc = cands[pos];
                    matches.push(Matched {
                        base: Some(loc),
                        side: (ti, ii),
                        rung: Rung::NamePath,
                    });
                    matched_base[loc.0][loc.1] = true;
                    matched_side[ti][ii] = true;
                }
            }
        }
    }

    // ---- rung (c): content fingerprint (contentless elements fall through
    // to rung (d): an empty-content fingerprint would match ANY contentless
    // element — arbitrary, exactly the collapse the ladder guards against)
    // ----
    let mut base_by_fp: HashMap<String, Vec<(usize, usize)>> = HashMap::new();
    for (ti, t) in base.tracks.iter().enumerate() {
        for (ii, item) in t.items.iter().enumerate() {
            if !matched_base[ti][ii] && item.has_content() {
                base_by_fp
                    .entry(item.content_fingerprint())
                    .or_default()
                    .push((ti, ii));
            }
        }
    }
    for (ti, t) in side.tracks.iter().enumerate() {
        for (ii, item) in t.items.iter().enumerate() {
            if matched_side[ti][ii] || !item.has_content() {
                continue;
            }
            if let Some(cands) = base_by_fp.get_mut(&item.content_fingerprint()) {
                if let Some(pos) = cands.iter().position(|&(bt, bi)| !matched_base[bt][bi]) {
                    let loc = cands[pos];
                    matches.push(Matched {
                        base: Some(loc),
                        rung: Rung::Fingerprint,
                        side: (ti, ii),
                    });
                    matched_base[loc.0][loc.1] = true;
                    matched_side[ti][ii] = true;
                }
            }
        }
    }

    // ---- rung (d): position-only for unlabeled-and-contentless residue ----
    // Only Gaps/empty items qualify; a positional match that later carries a
    // structural op escalates (C10). Match per track by relative position
    // among unmatched residue, only when names are empty and both are Gap kind.
    for (ti, t) in side.tracks.iter().enumerate() {
        // align by track matching (same-index fallback between similarly-
        // ordered tracks) — only for elements with no uuid, no name, Gap-like
        for (ii, item) in t.items.iter().enumerate() {
            if matched_side[ti][ii] || !item.name.is_empty() || !matches!(item.kind, Kind::Gap) {
                continue;
            }
            // find the base track matched to this side track
            let base_ti =
                track_of_match(&matches, ti).unwrap_or(ti.min(base.tracks.len().saturating_sub(1)));
            let bt = match base.tracks.get(base_ti) {
                Some(bt) => bt,
                None => continue,
            };
            // first unmatched, unnamed Gap in the base track from the front
            if let Some(bi) = (0..bt.items.len()).find(|&bi| {
                !matched_base[base_ti][bi]
                    && bt.items[bi].name.is_empty()
                    && matches!(bt.items[bi].kind, Kind::Gap)
            }) {
                matched_base[base_ti][bi] = true;
                matched_side[ti][ii] = true;
                matches.push(Matched {
                    base: Some((base_ti, bi)),
                    side: (ti, ii),
                    rung: Rung::Positional,
                });
            }
        }
    }

    // residue: new side elements (inserts) and removed base elements
    let mut inserts = Vec::new();
    for (ti, flags) in matched_side.iter().enumerate() {
        for (ii, &flag) in flags.iter().enumerate() {
            if !flag {
                inserts.push((ti, ii));
            }
        }
    }
    let mut removes = Vec::new();
    for (ti, flags) in matched_base.iter().enumerate() {
        for (ii, &flag) in flags.iter().enumerate() {
            if !flag {
                removes.push((ti, ii));
            }
        }
    }
    (matches, inserts, removes)
}

/// Which base track index does side track `ti` correspond to (from existing
/// matches)? Falls back to positional alignment.
fn track_of_match(matches: &[Matched], side_ti: usize) -> Option<usize> {
    matches
        .iter()
        .find(|m| m.side.0 == side_ti)
        .and_then(|m| m.base.map(|b| b.0))
}

/// The identity key for a BASE element — a pure function of the base element
/// and its location, so both sides and the apply phase compute the SAME key
/// (rung-dependent key formats would let cross-side conflicts pass as
/// disjoint — the exact bug class this design closes).
pub fn base_key(base_el: &Element, bt: usize, bi: usize) -> ElementKey {
    if let Some(u) = base_el.cairn_uuid() {
        return ElementKey(format!("uuid:{u}"));
    }
    if !base_el.name.is_empty() {
        return ElementKey(format!("name:{}", base_el.name));
    }
    if base_el.has_content() {
        return ElementKey(format!("fp:{}", base_el.content_fingerprint()));
    }
    ElementKey(format!("pos:{bt}:{bi}"))
}

/// Track-level matching (stack order): uuid → name+kind → order.
pub fn match_tracks(base: &Flat, side: &Flat) -> (Vec<Option<usize>>, Vec<usize>, Vec<usize>) {
    let mut side_to_base = vec![None; side.tracks.len()];
    let mut used = vec![false; base.tracks.len()];
    // pass 1: uuid
    let mut base_uuid: HashMap<String, usize> = HashMap::new();
    for (i, t) in base.tracks.iter().enumerate() {
        if let Some(u) = t.element.cairn_uuid() {
            base_uuid.insert(u.to_string(), i);
        }
    }
    for (i, t) in side.tracks.iter().enumerate() {
        if let Some(u) = t.element.cairn_uuid() {
            if let Some(&bi) = base_uuid.get(u) {
                if !used[bi] {
                    side_to_base[i] = Some(bi);
                    used[bi] = true;
                }
            }
        }
    }
    // pass 2: name + kind
    for (i, t) in side.tracks.iter().enumerate() {
        if side_to_base[i].is_some() {
            continue;
        }
        for (bi, bt) in base.tracks.iter().enumerate() {
            if used[bi] {
                continue;
            }
            if bt.element.name == t.element.name
                && std::mem::discriminant(&bt.element.kind)
                    == std::mem::discriminant(&t.element.kind)
            {
                side_to_base[i] = Some(bi);
                used[bi] = true;
                break;
            }
        }
    }
    // pass 3: order-preserving fallback
    let mut next = 0;
    for slot in &mut side_to_base {
        if slot.is_none() {
            while next < base.tracks.len() && used[next] {
                next += 1;
            }
            if next < base.tracks.len() {
                *slot = Some(next);
                used[next] = true;
                next += 1;
            }
        }
    }
    let new_tracks: Vec<usize> = (0..side.tracks.len())
        .filter(|&i| side_to_base[i].is_none())
        .collect();
    let removed_tracks: Vec<usize> = (0..base.tracks.len()).filter(|&i| !used[i]).collect();
    (side_to_base, new_tracks, removed_tracks)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::*;
    use crate::parse::parse_otio;
    use crate::rational::Rational;

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

    fn doc(tracks: Vec<Vec<Element>>) -> Flat {
        let track_els: Vec<Element> = tracks
            .into_iter()
            .enumerate()
            .map(|(i, items)| {
                Element::container(
                    Kind::Track(if i == 0 {
                        TrackKind::Video
                    } else {
                        TrackKind::Audio
                    }),
                    format!("T{i}"),
                    items,
                )
            })
            .collect();
        flatten(&Timeline {
            name: "x".into(),
            global_start_time: None,
            metadata: JsonMap::new(),
            tracks: Element::container(Kind::Stack, "tracks", track_els),
            extra: JsonMap::new(),
        })
    }

    #[test]
    fn uuid_rung_survives_move() {
        let mut a = clip("A", "file:///a", 0, 24);
        a.stamp_uuid("u-a");
        let mut b = clip("B", "file:///b", 24, 24);
        b.stamp_uuid("u-b");
        let base = doc(vec![vec![a.clone(), b.clone()]]);
        // side: same elements, REVERSED (moved)
        let side = doc(vec![vec![b, a]]);
        let (m, ins, rem) = match_docs(&base, &side);
        assert_eq!(ins.len(), 0);
        assert_eq!(rem.len(), 0);
        assert_eq!(m.len(), 2);
        assert!(m.iter().all(|x| x.rung == Rung::Uuid));
        // both matched despite reorder
        let moved = m.iter().filter(|x| x.base != Some(x.side)).count();
        assert_eq!(moved, 2);
    }

    #[test]
    fn rename_matched_by_fingerprint() {
        let a = clip("A", "file:///a", 0, 24);
        let base = doc(vec![vec![a]]);
        // side: renamed the same clip (no uuids stamped)
        let renamed = clip("Renamed", "file:///a", 0, 24);
        let side = doc(vec![vec![renamed]]);
        let (m, ins, rem) = match_docs(&base, &side);
        assert_eq!(ins.len(), 0, "renamed element must match, not insert");
        assert_eq!(rem.len(), 0);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].rung, Rung::Fingerprint);
    }

    #[test]
    fn name_rung_catches_retime() {
        let a = clip("A", "file:///a", 0, 24);
        let base = doc(vec![vec![a]]);
        // side: same name, different duration (fingerprint differs)
        let retimed = clip("A", "file:///a", 0, 48);
        let side = doc(vec![vec![retimed]]);
        let (m, ins, _rem) = match_docs(&base, &side);
        assert_eq!(ins.len(), 0);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].rung, Rung::NamePath);
    }

    #[test]
    fn positional_gap_matching_and_collapse() {
        // unnamed gaps with IDENTICAL content match at rung (c) fingerprint…
        let mut g1 = Element::leaf(Kind::Gap, "");
        g1.source_range = Some(TimeRange {
            start: tv(0, 24),
            duration: tv(24, 24),
        });
        let g2 = g1.clone();
        let base = doc(vec![vec![g1.clone(), g2.clone()]]);
        let side = doc(vec![vec![g1, g2]]);
        let (m, ins, rem) = match_docs(&base, &side);
        assert_eq!(ins.len(), 0);
        assert_eq!(rem.len(), 0);
        assert_eq!(m.len(), 2);
        assert!(m.iter().all(|x| !x.rung.is_positional()));

        // …while a CONTENTLESS gap (no range at all) falls to rung (d)
        let blank = Element::leaf(Kind::Gap, "");
        let blank2 = Element::leaf(Kind::Gap, "");
        let base = doc(vec![vec![blank]]);
        let side = doc(vec![vec![blank2]]);
        let (m, ins, rem) = match_docs(&base, &side);
        assert_eq!((ins.len(), rem.len()), (0, 0));
        assert_eq!(m.len(), 1);
        assert!(
            m[0].rung.is_positional(),
            "contentless gap must match positionally"
        );
    }

    #[test]
    fn insert_and_remove_detection() {
        let a = clip("A", "file:///a", 0, 24);
        let b = clip("B", "file:///b", 0, 24);
        let c = clip("C", "file:///c", 0, 24);
        let base = doc(vec![vec![a, b.clone()]]);
        let side = doc(vec![vec![b, c]]);
        let (m, ins, removes) = match_docs(&base, &side);
        // B matches (fingerprint), A removed, C inserted
        assert_eq!(removes, vec![(0, 0)]);
        assert_eq!(ins, vec![(0, 1)]);
        assert_eq!(m.len(), 1);
    }

    #[test]
    fn track_matching_add_remove() {
        let t0 = doc(vec![vec![clip("A", "a", 0, 24)]]);
        let t1 = doc(vec![
            vec![clip("A", "a", 0, 24)],
            vec![clip("N", "n", 0, 24)],
        ]);
        let (s2b, new, rem) = match_tracks(&t0, &t1);
        assert_eq!(s2b[0], Some(0));
        assert_eq!(new, vec![1]);
        assert!(rem.is_empty());
        let (s2b2, new2, rem2) = match_tracks(&t1, &t0);
        assert_eq!(rem2, vec![1]);
        assert!(new2.is_empty());
        assert_eq!(s2b2[0], Some(0));
    }

    #[test]
    fn flattens_fixture() {
        let tl = parse_otio(include_str!("../fixtures/roundtrip_base.otio")).unwrap();
        let f = flatten(&tl);
        assert_eq!(f.tracks.len(), 2);
        assert_eq!(f.tracks[0].items.len(), 3);
        assert_eq!(f.tracks[1].items.len(), 1);
        assert_eq!(f.tracks[0].element.name, "V1");
    }
}
