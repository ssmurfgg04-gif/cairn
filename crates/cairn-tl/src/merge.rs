//! Three-way merge driver + apply engine (ADR-0015 §2.4–2.6).
//!
//! Apply model — the two properties the whole design protects:
//! 1. **No position drift**: structural ops never mutate positions
//!    incrementally. They are collected into a plan and MATERIALIZED once per
//!    track: base items survive in base order (minus removed/moved-out),
//!    anchored inserts/moves-in land before their base anchor (ours first,
//!    then theirs, each in op order). Non-structural ops (Trim/Attr/Marker)
//!    resolve their target by (identity key, base origin) — an op follows its
//!    element even when the OTHER side moved it.
//! 2. **No double-apply**: identical duplicate ops from both sides dedupe to
//!    ours (a trim applied twice would double the delta; an insert twice
//!    would duplicate a clip). Conflicting pairs are withheld ENTIRELY (both
//!    sides) — the base state is kept for the human. Never last-write-wins on
//!    creative parameters (C3), never deletion-wins (C7).
//!
//! Determinism (§2.6): merge(base, ours, theirs) is pure; op sort +
//! ours-before-theirs at every anchor make the output a function of inputs.

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::classifier::{self, Verdict};
use crate::identity::{self, ElementKey, Flat};
use crate::model::{marker_uuid, Element, JsonMap, Kind, Marker, TimeRange, TimeVal, Timeline};
use crate::ops::{self, AttrKind, Op, Side, Slot, TrackLoc};
use crate::rational::Rational;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Clean,
    Notes,
    Conflicts,
}

#[derive(Debug, Clone)]
pub struct VerdictRecord {
    pub class: u8,
    pub verdict: Verdict,
    pub ours: String,
    pub theirs: Option<String>,
    pub note: String,
}

#[derive(Debug, Clone, Default)]
pub struct MergeStats {
    pub ops_ours: usize,
    pub ops_theirs: usize,
    pub applied: usize,
    pub withheld: usize,
    pub deduped: usize,
}

#[derive(Debug, Clone)]
pub struct MergeReport {
    pub outcome: Outcome,
    pub histogram: BTreeMap<u8, usize>,
    pub verdicts: Vec<VerdictRecord>,
    pub stats: MergeStats,
    /// The policy this merge ran under (ADR-0023). A C11 verdict can only
    /// appear when this is `true`; the JSON form records it explicitly so a
    /// merge artifact is self-describing.
    pub semantic: bool,
}

impl MergeReport {
    /// Machine-readable report JSON (`.cairn-timeline/reports/<seq>.json`).
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "policy": if self.semantic { "semantic" } else { "conservative" },
            "outcome": match self.outcome {
                Outcome::Clean => "clean",
                Outcome::Notes => "notes",
                Outcome::Conflicts => "conflicts",
            },
            "histogram": self.histogram.iter().map(|(k, v)| (format!("C{k}"), *v)).collect::<BTreeMap<_, _>>(),
            "stats": {
                "ops_ours": self.stats.ops_ours,
                "ops_theirs": self.stats.ops_theirs,
                "applied": self.stats.applied,
                "withheld": self.stats.withheld,
                "deduped": self.stats.deduped,
            },
            "verdicts": self.verdicts.iter().map(|v| serde_json::json!({
                "class": format!("C{}", v.class),
                "verdict": format!("{:?}", v.verdict),
                "ours": v.ours,
                "theirs": v.theirs,
                "note": v.note,
            })).collect::<Vec<_>>(),
        })
    }
}

/// C10 refusal: the merge refuses; artifacts + report hand off to a human.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeRefusal(pub String);

/// Merge policy knobs (ADR-0023). Defaults are bit-for-bit the Round-19
/// behavior: conservative, zero-touch OFF. Every editor opts into semantic
/// themselves — never a project-wide or role-wide default.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MergeOptions {
    /// Zero-touch semantic merge: frame-disjoint edits to the same element
    /// (a head-only vs a tail-only re-cut) auto-merge (C11) instead of
    /// escalating C3. `false` by default.
    pub semantic: bool,
}

impl MergeOptions {
    fn policy(&self) -> classifier::Policy {
        if self.semantic {
            classifier::Policy::Semantic
        } else {
            classifier::Policy::Conservative
        }
    }
}

/// Merge three timelines under the default (conservative) policy.
/// Which side is "ours" is decided by the CALLER (fencing-token policy,
/// SPEC §8 — the save under the surviving fence).
pub fn merge(
    base: &Timeline,
    ours: &Timeline,
    theirs: &Timeline,
) -> Result<(Timeline, MergeReport), MergeRefusal> {
    merge_with(base, ours, theirs, &MergeOptions::default())
}

/// Merge three timelines with an explicit policy (ADR-0023). The report
/// records the policy so a merge artifact is self-describing: a C11 verdict
/// can only exist under `"policy": "semantic"`.
pub fn merge_with(
    base: &Timeline,
    ours: &Timeline,
    theirs: &Timeline,
    options: &MergeOptions,
) -> Result<(Timeline, MergeReport), MergeRefusal> {
    let base_f = identity::flatten(base);
    let ours_f = identity::flatten(ours);
    let theirs_f = identity::flatten(theirs);

    let ours_ops = ops::extract_ops(Side::Ours, &base_f, &ours_f)
        .map_err(|e| MergeRefusal(format!("C10: {}", e.0)))?;
    let theirs_ops = ops::extract_ops(Side::Theirs, &base_f, &theirs_f)
        .map_err(|e| MergeRefusal(format!("C10: {}", e.0)))?;

    let mut report = MergeReport {
        outcome: Outcome::Clean,
        histogram: BTreeMap::new(),
        verdicts: Vec::new(),
        semantic: options.semantic,
        stats: MergeStats {
            ops_ours: ours_ops.len(),
            ops_theirs: theirs_ops.len(),
            ..MergeStats::default()
        },
    };

    // ---- cross-classification ----
    let mut withheld_ours: HashSet<usize> = HashSet::new();
    let mut withheld_theirs: HashSet<usize> = HashSet::new();
    let mut deduped_theirs: HashSet<usize> = HashSet::new();
    for (oi, o) in ours_ops.iter().enumerate() {
        for (ti, t) in theirs_ops.iter().enumerate() {
            if !classifier::interacts(o, t) {
                continue;
            }
            let v = classifier::classify_pair(o, t, options.policy());
            *report.histogram.entry(v.class).or_insert(0) += 1;
            report.verdicts.push(VerdictRecord {
                class: v.class,
                verdict: v.verdict,
                ours: o.summary(),
                theirs: Some(t.summary()),
                note: v.note.clone(),
            });
            match v.verdict {
                Verdict::Auto => {
                    if v.once {
                        deduped_theirs.insert(ti);
                    }
                }
                Verdict::AutoNote => {}
                Verdict::Human => {
                    withheld_ours.insert(oi);
                    withheld_theirs.insert(ti);
                }
                Verdict::Refuse => return Err(MergeRefusal(format!("C10: {}", v.note))),
            }
        }
    }

    let eff_ours: Vec<Op> = ours_ops
        .iter()
        .enumerate()
        .filter(|(i, _)| !withheld_ours.contains(i))
        .map(|(_, o)| o.clone())
        .collect();
    let eff_theirs: Vec<Op> = theirs_ops
        .iter()
        .enumerate()
        .filter(|(i, _)| !withheld_theirs.contains(i) && !deduped_theirs.contains(i))
        .map(|(_, t)| t.clone())
        .collect();

    // ---- apply (pure, functional) ----
    let merged = {
        let mut working = Working::new(&base_f);
        working.apply_track_adds(&eff_ours, &eff_theirs);
        working.collect_intents(&eff_ours, &eff_theirs);
        working.materialize();
        working.apply_edits(&eff_ours, &eff_theirs, &mut report);
        working.assemble()
    };

    let merged = merge_timeline_fields(base, ours, theirs, &mut report, merged);

    report.outcome = if report.verdicts.iter().any(|v| v.verdict == Verdict::Human) {
        Outcome::Conflicts
    } else if report
        .verdicts
        .iter()
        .any(|v| matches!(v.verdict, Verdict::AutoNote))
    {
        Outcome::Notes
    } else {
        Outcome::Clean
    };
    report.stats.applied = eff_ours.len() + eff_theirs.len();
    report.stats.withheld = withheld_ours.len() + withheld_theirs.len();
    // dedup counts only ops that were ACTUALLY skipped (an op can be both
    // withheld and dedup-flagged; withholding wins and owns the count)
    report.stats.deduped = deduped_theirs.difference(&withheld_theirs).count();
    Ok((merged, report))
}

fn merge_timeline_fields(
    base: &Timeline,
    ours: &Timeline,
    theirs: &Timeline,
    report: &mut MergeReport,
    mut merged: Timeline,
) -> Timeline {
    let out = &mut merged;
    let conflict = |note: String, report: &mut MergeReport| {
        *report.histogram.entry(classifier::class::C3).or_insert(0) += 1;
        report.verdicts.push(VerdictRecord {
            class: classifier::class::C3,
            verdict: Verdict::Human,
            ours: "timeline field".into(),
            theirs: Some("timeline field".into()),
            note,
        });
    };
    if ours.name != base.name && theirs.name != base.name {
        if ours.name == theirs.name {
            out.name = ours.name.clone();
        } else {
            conflict(
                format!(
                    "timeline name: {:?} vs {:?} — base kept",
                    ours.name, theirs.name
                ),
                report,
            );
        }
    } else if ours.name != base.name {
        out.name = ours.name.clone();
    } else if theirs.name != base.name {
        out.name = theirs.name.clone();
    }
    if ours.global_start_time != base.global_start_time
        && theirs.global_start_time != base.global_start_time
    {
        if ours.global_start_time == theirs.global_start_time {
            out.global_start_time = ours.global_start_time;
        } else {
            conflict(
                "timeline global_start_time changed differently — base kept".into(),
                report,
            );
        }
    } else if ours.global_start_time != base.global_start_time {
        out.global_start_time = ours.global_start_time;
    } else if theirs.global_start_time != base.global_start_time {
        out.global_start_time = theirs.global_start_time;
    }
    merged
}

/// One materialized item: element + merge identity + base origin (None for
/// inserted elements). Origin survives moves — phase-C ops resolve by it.
#[derive(Clone, Debug)]
struct WorkItem {
    element: Element,
    key: ElementKey,
    origin: Option<(usize, usize)>,
}

#[derive(Clone, Debug)]
struct WorkTrack {
    element: Element,
    items: Vec<WorkItem>,
    /// (side, ordinal) for tracks created by TrackAdd ops.
    new_id: Option<(Side, usize)>,
}

/// An anchored placement collected during the intent pass.
#[derive(Clone, Debug)]
struct Anchored {
    element: Element,
    key: ElementKey,
    origin: Option<(usize, usize)>,
    /// Before base item at this index (None = end of track).
    anchor_index: Option<usize>,
}

/// The working document.
struct Working {
    base_track_count: usize,
    tracks: Vec<WorkTrack>,
    removed_tracks: HashSet<usize>,
    reorder: Option<Vec<usize>>,
    removed_origins: HashSet<(usize, usize)>,
    /// base origin → (target track, anchor) for moved elements.
    moved: HashMap<(usize, usize), (TrackLoc, Slot)>,
    /// anchored placements per BASE track (ours appended before theirs by
    /// the [ours, theirs] iteration order — preserved by Vec push order).
    anchored: HashMap<usize, Vec<Anchored>>,
    /// items destined for new tracks, by (side, ordinal) → (position, item).
    new_items: HashMap<(Side, usize), Vec<(usize, WorkItem)>>,
}

impl Working {
    fn new(base: &Flat) -> Working {
        let mut tracks = Vec::new();
        for (bt, t) in base.tracks.iter().enumerate() {
            let mut items = Vec::new();
            for (bi, item) in t.items.iter().enumerate() {
                let key = identity::base_key(item, bt, bi);
                items.push(WorkItem {
                    element: item.clone(),
                    key,
                    origin: Some((bt, bi)),
                });
            }
            tracks.push(WorkTrack {
                element: t.element.clone(),
                items,
                new_id: None,
            });
        }
        Working {
            base_track_count: tracks.len(),
            tracks,
            removed_tracks: HashSet::new(),
            reorder: None,
            removed_origins: HashSet::new(),
            moved: HashMap::new(),
            anchored: HashMap::new(),
            new_items: HashMap::new(),
        }
    }

    /// (a) TrackAdd creates empty tracks (children arrive as Insert/Move ops);
    /// TrackRemove and TrackReorder are recorded.
    fn apply_track_adds(&mut self, eff_ours: &[Op], eff_theirs: &[Op]) {
        for op_list in [eff_ours, eff_theirs] {
            for op in op_list {
                match op {
                    Op::TrackAdd { ordinal, track, .. } => {
                        let mut el = track.clone();
                        el.children.clear();
                        let side = op.side();
                        self.tracks.push(WorkTrack {
                            element: el,
                            items: Vec::new(),
                            new_id: Some((side, *ordinal)),
                        });
                    }
                    Op::TrackRemove { track, .. } => {
                        self.removed_tracks.insert(*track);
                    }
                    Op::TrackReorder { order, .. } => {
                        self.reorder = Some(order.clone());
                    }
                    _ => {}
                }
            }
        }
    }

    /// (b-intent) collect removes / moves / inserts into the plan. Iteration
    /// [ours, theirs] with Vec push order = ours-before-theirs per anchor.
    fn collect_intents(&mut self, eff_ours: &[Op], eff_theirs: &[Op]) {
        for op_list in [eff_ours, eff_theirs] {
            for op in op_list {
                match op {
                    Op::Remove { base, .. } => {
                        self.removed_origins.insert(*base);
                    }
                    Op::Move { from, to, slot, .. } => {
                        self.moved.insert(*from, (*to, *slot));
                    }
                    Op::Insert {
                        element, to, slot, ..
                    } => {
                        let key = ElementKey(format!(
                            "ins:{}:{}",
                            match op.side() {
                                Side::Ours => "ours",
                                Side::Theirs => "theirs",
                            },
                            element
                                .cairn_uuid()
                                .map(str::to_string)
                                .unwrap_or_else(|| format!(
                                    "{}|{}",
                                    element.name,
                                    element.content_fingerprint()
                                ))
                        ));
                        self.place(
                            WorkItem {
                                element: element.clone(),
                                key,
                                origin: None,
                            },
                            *to,
                            *slot,
                        );
                    }
                    _ => {}
                }
            }
        }
        // resolve moves: source items are found in the (still-base-shaped)
        // working tracks; they leave their origin and are placed at target.
        let moved = self.moved.clone();
        for (origin, (to, slot)) in moved {
            let Some(src) = self.find_item(origin) else {
                continue;
            };
            let item = WorkItem {
                element: src.0,
                key: src.1,
                origin: Some(origin), // origin SURVIVES the move (phase C)
            };
            self.place(item, to, slot);
        }
    }

    /// Place an item at its target: base track anchor, or new-track position.
    fn place(&mut self, item: WorkItem, to: TrackLoc, slot: Slot) {
        match to {
            TrackLoc::Base(bt) => {
                if self.removed_tracks.contains(&bt) {
                    // C9-withheld TrackRemove vs move-in cannot co-occur in the
                    // effective lists; defensive: drop to end of the LAST track
                    let last = self.base_track_count.saturating_sub(1);
                    self.anchored.entry(last).or_default().push(Anchored {
                        element: item.element,
                        key: item.key,
                        origin: item.origin,
                        anchor_index: None,
                    });
                    return;
                }
                let anchor_index = match slot {
                    Slot::Before { index, .. } => Some(index),
                    _ => None,
                };
                self.anchored.entry(bt).or_default().push(Anchored {
                    element: item.element,
                    key: item.key,
                    origin: item.origin,
                    anchor_index,
                });
            }
            TrackLoc::New { side, ordinal } => {
                let pos = match slot {
                    Slot::NewTrackOrdinal { ordinal } => ordinal,
                    _ => 0,
                };
                self.new_items
                    .entry((side, ordinal))
                    .or_default()
                    .push((pos, item));
            }
        }
    }

    /// Find the (element, key) of a base-origin item (for moves).
    fn find_item(&self, origin: (usize, usize)) -> Option<(Element, ElementKey)> {
        self.tracks
            .get(origin.0)?
            .items
            .iter()
            .find(|it| it.origin == Some(origin))
            .map(|it| (it.element.clone(), it.key.clone()))
    }

    /// (b-materialize) rebuild every track's item list functionally.
    fn materialize(&mut self) {
        // base tracks: walk base-origin items in order; anchored placements
        // emit before their anchor (ours first — Vec order); removed/moved
        // items skip; end-anchored items append after.
        for bt in 0..self.base_track_count {
            if self.removed_tracks.contains(&bt) {
                continue;
            }
            let mut by_index: HashMap<usize, Vec<Anchored>> = HashMap::new();
            let mut end_list: Vec<Anchored> = Vec::new();
            if let Some(list) = self.anchored.remove(&bt) {
                for a in list {
                    match a.anchor_index {
                        Some(i) => by_index.entry(i).or_default().push(a),
                        None => end_list.push(a),
                    }
                }
            }
            let current = std::mem::take(&mut self.tracks[bt].items);
            let mut new_items: Vec<WorkItem> = Vec::new();
            for item in current {
                if let Some(orig) = item.origin {
                    if self.removed_origins.contains(&orig) {
                        continue;
                    }
                    if self.moved.contains_key(&orig) {
                        continue;
                    }
                    if let Some(anchored_here) = by_index.remove(&orig.1) {
                        for a in anchored_here {
                            new_items.push(WorkItem {
                                element: a.element,
                                key: a.key,
                                origin: a.origin,
                            });
                        }
                    }
                }
                new_items.push(item);
            }
            // any leftover anchored records whose anchor item was removed /
            // moved: they still belong in this track — emit in position order
            // at the point their anchor WOULD have been: after the last
            // surviving item with a smaller base index.
            let mut leftovers: Vec<(usize, Anchored)> = Vec::new();
            for (idx, list) in by_index {
                for a in list {
                    leftovers.push((idx, a));
                }
            }
            if !leftovers.is_empty() {
                leftovers.sort_by_key(|(idx, _)| *idx);
                // merge leftovers into new_items by scanning: insert each
                // leftover before the first item whose origin index > its idx
                for (idx, a) in leftovers {
                    let pos = new_items
                        .iter()
                        .position(|it| matches!(it.origin, Some((_, oi)) if oi > idx))
                        .unwrap_or(new_items.len());
                    new_items.insert(
                        pos,
                        WorkItem {
                            element: a.element,
                            key: a.key,
                            origin: a.origin,
                        },
                    );
                }
            }
            for a in end_list {
                new_items.push(WorkItem {
                    element: a.element,
                    key: a.key,
                    origin: a.origin,
                });
            }
            self.tracks[bt].items = new_items;
        }

        // new tracks: attach their items by side position
        let pending = std::mem::take(&mut self.new_items);
        for (key, mut items) in pending {
            items.sort_by_key(|(pos, _)| *pos);
            let work = self.tracks.iter_mut().find(|t| t.new_id == Some(key));
            if let Some(wt) = work {
                for (_, item) in items {
                    wt.items.push(item);
                }
            }
        }
    }

    /// (c) Trim/Attr/Marker ops resolved by (key, origin) — identity that
    /// FOLLOWS moves. TrackAttr applies to the matched track's own element
    /// (base index — matched tracks are stable across sides; a removed track
    /// keeps base state, which is exactly the C9-withheld contract).
    fn apply_edits(&mut self, eff_ours: &[Op], eff_theirs: &[Op], report: &mut MergeReport) {
        for op_list in [eff_ours, eff_theirs] {
            for op in op_list {
                match op {
                    Op::TrackAttr {
                        track, attr, value, ..
                    } => {
                        if *track < self.base_track_count && !self.removed_tracks.contains(track) {
                            if let Some(wt) = self.tracks.get_mut(*track) {
                                apply_attr(&mut wt.element, attr, value);
                            }
                        }
                    }
                    Op::Trim {
                        key,
                        base,
                        in_delta,
                        out_delta,
                        ..
                    } => {
                        if let Some(item) = self.resolve(key, *base) {
                            if let Some(sr) = item.element.source_range.clone() {
                                match apply_trim(&sr, *in_delta, *out_delta) {
                                    Some(new_range) => item.element.source_range = Some(new_range),
                                    None => note_degenerate_trim(report, key),
                                }
                            }
                        }
                    }
                    Op::Attr {
                        key,
                        base,
                        attr,
                        value,
                        ..
                    } => {
                        if let Some(item) = self.resolve(key, *base) {
                            apply_attr(&mut item.element, attr, value);
                        }
                    }
                    Op::MarkerAdd {
                        key, base, marker, ..
                    } => {
                        if let Some(item) = self.resolve(key, *base) {
                            let id = marker_identity(marker);
                            if !item
                                .element
                                .markers
                                .iter()
                                .any(|m| marker_identity(m) == id)
                            {
                                item.element.markers.push(marker.clone());
                            }
                        }
                    }
                    Op::MarkerRemove {
                        key,
                        base,
                        marker_key,
                        ..
                    } => {
                        if let Some(item) = self.resolve(key, *base) {
                            let id = marker_key.clone();
                            item.element.markers.retain(|m| marker_identity(m) != id);
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    fn resolve(&mut self, key: &ElementKey, origin: (usize, usize)) -> Option<&mut WorkItem> {
        // exact (key, origin): the op's own base element, wherever it now
        // lives (moved elements keep their origin)
        for (ti, wt) in self.tracks.iter().enumerate() {
            if self.removed_tracks.contains(&ti) {
                continue;
            }
            for (ii, item) in wt.items.iter().enumerate() {
                if item.key == *key && item.origin == Some(origin) {
                    return self.tracks.get_mut(ti)?.items.get_mut(ii);
                }
            }
        }
        // same key alone (defensive: dedup edge shapes)
        for (ti, wt) in self.tracks.iter().enumerate() {
            if self.removed_tracks.contains(&ti) {
                continue;
            }
            for (ii, item) in wt.items.iter().enumerate() {
                if item.key == *key {
                    return self.tracks.get_mut(ti)?.items.get_mut(ii);
                }
            }
        }
        None
    }

    /// Assemble the final Timeline: base tracks in base order (minus
    /// removed, reorder applied if auto), new tracks in creation order.
    fn assemble(&self) -> Timeline {
        let mut base_els: Vec<Element> = Vec::new();
        for (i, wt) in self.tracks.iter().enumerate() {
            if i >= self.base_track_count {
                break; // new tracks handled below
            }
            if self.removed_tracks.contains(&i) {
                continue;
            }
            base_els.push(self.track_element(wt));
        }
        if let Some(order) = &self.reorder {
            let mut arranged: Vec<Element> = Vec::new();
            let mut by_idx: Vec<(usize, Element)> = (0..self.base_track_count)
                .filter(|i| !self.removed_tracks.contains(i))
                .zip(base_els)
                .collect();
            for &want in order {
                if let Some(pos) = by_idx.iter().position(|(i, _)| *i == want) {
                    arranged.push(by_idx.remove(pos).1);
                }
            }
            arranged.extend(by_idx.into_iter().map(|(_, e)| e));
            base_els = arranged;
        }
        let mut new_els: Vec<Element> = self
            .tracks
            .iter()
            .skip(self.base_track_count)
            .map(|wt| self.track_element(wt))
            .collect();
        base_els.append(&mut new_els);
        Timeline {
            name: String::new(),
            global_start_time: None,
            metadata: JsonMap::new(),
            tracks: Element::container(Kind::Stack, "tracks", base_els),
            extra: JsonMap::new(),
        }
    }

    fn track_element(&self, wt: &WorkTrack) -> Element {
        let mut el = wt.element.clone();
        el.children = wt.items.iter().map(|it| it.element.clone()).collect();
        el
    }
}

fn apply_trim(sr: &TimeRange, in_delta: Rational, out_delta: Rational) -> Option<TimeRange> {
    let start = sr.start.seconds().ok()?;
    let dur = sr.duration.seconds().ok()?;
    let new_start = start.checked_add(in_delta).ok()?;
    let new_dur = dur
        .checked_sub(in_delta)
        .and_then(|d| d.checked_sub(out_delta))
        .ok()?;
    if new_dur.num <= 0 {
        return None; // degenerate trim — noted, never silently applied
    }
    Some(TimeRange {
        start: TimeVal::from_seconds(new_start, sr.start.rate).ok()?,
        duration: TimeVal::from_seconds(new_dur, sr.start.rate).ok()?,
    })
}

fn note_degenerate_trim(report: &mut MergeReport, key: &ElementKey) {
    *report.histogram.entry(classifier::class::C10).or_insert(0) += 1;
    report.verdicts.push(VerdictRecord {
        class: classifier::class::C10,
        verdict: Verdict::AutoNote,
        ours: format!("trim {}", key.0),
        theirs: None,
        note: "trim arithmetic degenerate (zero/negative duration) — base range kept".into(),
    });
}

fn apply_attr(el: &mut Element, attr: &AttrKind, value: &serde_json::Value) {
    match attr {
        AttrKind::Name => {
            if let Some(s) = value.as_str() {
                el.name = s.to_string();
            }
        }
        AttrKind::Enabled => {
            if let Some(b) = value.as_bool() {
                el.enabled = b;
            }
        }
        AttrKind::MediaTarget => {
            if let Some(s) = value.as_str() {
                if let Some(media) = &mut el.media {
                    if let Some(entry) = media.references.get_mut(&media.active_key) {
                        entry.target_url = Some(s.to_string());
                    }
                    media.target_url = Some(s.to_string());
                }
            }
        }
        AttrKind::Effects => {
            if let Some(list) = crate::parse::effects_from_value(value) {
                el.effects = list;
            }
        }
        AttrKind::Raw(path) => match path.as_str() {
            "source_range" => {
                if value.is_null() {
                    el.source_range = None;
                } else if let Some(r) = crate::parse::range_from_value(value) {
                    el.source_range = Some(r);
                }
            }
            "metadata" => {
                if let Some(m) = crate::parse::map_from_value(value) {
                    el.metadata = m;
                }
            }
            "extra" => {
                if let Some(m) = crate::parse::map_from_value(value) {
                    el.extra = m;
                }
            }
            _ => {}
        },
    }
}

fn marker_identity(m: &Marker) -> String {
    marker_uuid(m).unwrap_or_else(|| format!("{}|{}", m.name, m.comment))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        Element, JsonMap, Kind, Marker, MediaKind, MediaRef, TimeRange, TimeVal, Timeline,
        TrackKind,
    };
    use crate::rational::Rational;

    fn tv(v: i128, r: i128) -> TimeVal {
        TimeVal {
            value: Rational::new(v, 1).unwrap(),
            rate: Rational::new(r, 1).unwrap(),
        }
    }

    pub(crate) fn clip(name: &str, url: &str, start: i128, dur: i128) -> Element {
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

    pub(crate) fn doc(items: Vec<Element>) -> Timeline {
        Timeline {
            name: "tl".into(),
            global_start_time: None,
            metadata: JsonMap::new(),
            tracks: Element::container(
                Kind::Stack,
                "tracks",
                vec![Element::container(
                    Kind::Track(TrackKind::Video),
                    "V1",
                    items,
                )],
            ),
            extra: JsonMap::new(),
        }
    }

    fn stamp(tl: &mut Timeline) {
        tl.walk_mut(|e| {
            if e.cairn_uuid().is_none() {
                let id = uuid::Uuid::now_v7().to_string();
                e.stamp_uuid(&id);
            }
        });
    }

    #[test]
    fn disjoint_edits_merge_cleanly() {
        // ours: rename clip 0. theirs: trim clip 1. Both must land.
        let mut base = doc(vec![clip("A", "a", 0, 24), clip("B", "b", 0, 24)]);
        stamp(&mut base);
        let mut ours = base.clone();
        let mut theirs = base.clone();
        ours.tracks.children[0].children[0].name = "A2".into();
        theirs.tracks.children[0].children[1].source_range = Some(TimeRange {
            start: tv(0, 24),
            duration: tv(12, 24),
        });
        let (merged, report) = merge(&base, &ours, &theirs).unwrap();
        assert_eq!(report.outcome, Outcome::Clean);
        let items = &merged.tracks.children[0].children;
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].name, "A2");
        assert_eq!(
            items[1].source_range.as_ref().unwrap().duration.value.num,
            12
        );
    }

    #[test]
    fn both_sides_same_edit_applies_once() {
        let mut base = doc(vec![clip("A", "a", 0, 24)]);
        stamp(&mut base);
        let mut ours = base.clone();
        let mut theirs = base.clone();
        // identical trims: 12 frames off the tail
        for side in [&mut ours, &mut theirs] {
            side.tracks.children[0].children[0].source_range = Some(TimeRange {
                start: tv(0, 24),
                duration: tv(12, 24),
            });
        }
        let (merged, report) = merge(&base, &ours, &theirs).unwrap();
        assert_eq!(report.outcome, Outcome::Clean);
        let dur = merged.tracks.children[0].children[0]
            .source_range
            .as_ref()
            .unwrap()
            .duration
            .value
            .num;
        assert_eq!(dur, 12, "identical trim must apply ONCE (not double)");
    }

    #[test]
    fn c3_conflicting_attr_withholds_both() {
        let mut base = doc(vec![clip("A", "a", 0, 24)]);
        stamp(&mut base);
        let mut ours = base.clone();
        let mut theirs = base.clone();
        ours.tracks.children[0].children[0].name = "OursName".into();
        theirs.tracks.children[0].children[0].name = "TheirsName".into();
        let (merged, report) = merge(&base, &ours, &theirs).unwrap();
        assert_eq!(report.outcome, Outcome::Conflicts);
        assert_eq!(report.histogram.get(&classifier::class::C3), Some(&1));
        // base value kept — never last-write-wins
        assert_eq!(merged.tracks.children[0].children[0].name, "A");
    }

    #[test]
    fn c6_remove_both_and_c7_remove_vs_edit() {
        let mut base = doc(vec![clip("A", "a", 0, 24), clip("B", "b", 0, 24)]);
        stamp(&mut base);
        // C6: both remove A
        let mut ours = base.clone();
        let mut theirs = base.clone();
        ours.tracks.children[0].children.remove(0);
        theirs.tracks.children[0].children.remove(0);
        let (merged, report) = merge(&base, &ours, &theirs).unwrap();
        assert_eq!(report.outcome, Outcome::Clean);
        assert_eq!(merged.tracks.children[0].children.len(), 1);
        assert_eq!(report.histogram.get(&classifier::class::C6), Some(&1));

        // C7: ours removes B, theirs trims B → withheld both, base kept
        let mut ours = base.clone();
        let mut theirs = base.clone();
        ours.tracks.children[0].children.remove(1);
        theirs.tracks.children[0].children[1].source_range = Some(TimeRange {
            start: tv(0, 24),
            duration: tv(12, 24),
        });
        let (merged, report) = merge(&base, &ours, &theirs).unwrap();
        assert_eq!(report.outcome, Outcome::Conflicts);
        assert_eq!(report.histogram.get(&classifier::class::C7), Some(&1));
        assert_eq!(
            merged.tracks.children[0].children.len(),
            2,
            "conflicted removal withheld — B survives"
        );
    }

    #[test]
    fn c8_same_slot_inserts_both_land_ours_first() {
        let mut base = doc(vec![clip("A", "a", 0, 24), clip("B", "b", 0, 24)]);
        stamp(&mut base);
        let mut ours = base.clone();
        let mut theirs = base.clone();
        // both insert a NEW clip before B (different content)
        ours.tracks.children[0].children.insert(1, {
            let mut c = clip("O", "o", 0, 24);
            c.stamp_uuid("u-o");
            c
        });
        theirs.tracks.children[0].children.insert(1, {
            let mut c = clip("T", "t", 0, 24);
            c.stamp_uuid("u-t");
            c
        });
        let (merged, report) = merge(&base, &ours, &theirs).unwrap();
        assert_eq!(report.outcome, Outcome::Notes);
        assert_eq!(report.histogram.get(&classifier::class::C8), Some(&1));
        let names: Vec<&str> = merged.tracks.children[0]
            .children
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(
            names,
            ["A", "O", "T", "B"],
            "ours first, theirs immediately after"
        );
    }

    #[test]
    fn c5_move_then_trim_follows_identity() {
        let mut base = doc(vec![clip("A", "a", 0, 24), clip("B", "b", 0, 24)]);
        stamp(&mut base);
        // ours: move B before A. theirs: trim B. Verdict C5 auto (move, then trim).
        let mut ours = base.clone();
        let mut theirs = base.clone();
        let b = ours.tracks.children[0].children.remove(1);
        ours.tracks.children[0].children.insert(0, b);
        theirs.tracks.children[0].children[1].source_range = Some(TimeRange {
            start: tv(0, 24),
            duration: tv(12, 24),
        });
        let (merged, report) = merge(&base, &ours, &theirs).unwrap();
        assert_eq!(report.outcome, Outcome::Clean);
        assert_eq!(report.histogram.get(&classifier::class::C5), Some(&1));
        let items = &merged.tracks.children[0].children;
        let names: Vec<&str> = items.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["B", "A"], "moved first");
        assert_eq!(
            items[0].source_range.as_ref().unwrap().duration.value.num,
            12,
            "trim followed the element through the move"
        );
    }

    #[test]
    fn new_track_with_moved_element() {
        let mut base = doc(vec![clip("A", "a", 0, 24), clip("B", "b", 0, 24)]);
        stamp(&mut base);
        let mut ours = base.clone();
        // ours: new track V2, B moved into it
        let b = ours.tracks.children[0].children.remove(1);
        let mut v2 = Element::container(Kind::Track(TrackKind::Video), "V2", vec![b]);
        v2.stamp_uuid("t-v2");
        ours.tracks.children.push(v2);
        let (merged, report) = merge(&base, &ours, &base.clone()).unwrap();
        assert_eq!(report.outcome, Outcome::Clean);
        assert_eq!(merged.tracks.children.len(), 2);
        let v1_names: Vec<&str> = merged.tracks.children[0]
            .children
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        let v2_names: Vec<&str> = merged.tracks.children[1]
            .children
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(v1_names, ["A"]);
        assert_eq!(v2_names, ["B"], "moved element landed in the NEW track");
    }

    #[test]
    fn inserts_from_both_sides_at_different_slots() {
        let mut base = doc(vec![clip("A", "a", 0, 24)]);
        stamp(&mut base);
        let mut ours = base.clone();
        let mut theirs = base.clone();
        ours.tracks.children[0].children.push({
            let mut c = clip("O", "o", 0, 24);
            c.stamp_uuid("u-o");
            c
        });
        theirs.tracks.children[0].children.push({
            let mut c = clip("T", "t", 0, 24);
            c.stamp_uuid("u-t");
            c
        });
        let (merged, report) = merge(&base, &ours, &theirs).unwrap();
        // both end-anchored at the same slot: C8 note, ours then theirs
        assert_eq!(report.outcome, Outcome::Notes);
        let names: Vec<&str> = merged.tracks.children[0]
            .children
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(names, ["A", "O", "T"]);
    }

    #[test]
    fn markers_union_and_removal() {
        let mut base = doc(vec![clip("A", "a", 0, 24)]);
        stamp(&mut base);
        let mk = |name: &str| Marker {
            schema: "Marker.2".into(),
            name: name.into(),
            color: "RED".into(),
            comment: String::new(),
            marked_range: TimeRange {
                start: tv(0, 24),
                duration: tv(0, 24),
            },
            metadata: JsonMap::new(),
            extra: JsonMap::new(),
        };
        // base has M0; ours adds M1; theirs adds M2 → union
        base.tracks.children[0].children[0].markers.push(mk("M0"));
        let mut ours = base.clone();
        let mut theirs = base.clone();
        ours.tracks.children[0].children[0].markers.push(mk("M1"));
        theirs.tracks.children[0].children[0].markers.push(mk("M2"));
        let (merged, report) = merge(&base, &ours, &theirs).unwrap();
        assert_eq!(report.outcome, Outcome::Clean);
        let m: Vec<&str> = merged.tracks.children[0].children[0]
            .markers
            .iter()
            .map(|x| x.name.as_str())
            .collect();
        assert_eq!(m, ["M0", "M1", "M2"], "C1 union: ours order then theirs");

        // one side removes, other untouched → removal lands (C0)
        let mut ours = base.clone();
        ours.tracks.children[0].children[0].markers.clear();
        let (merged, report) = merge(&base, &ours, &base.clone()).unwrap();
        assert_eq!(report.outcome, Outcome::Clean);
        assert!(merged.tracks.children[0].children[0].markers.is_empty());
    }

    #[test]
    fn rename_and_move_cross_side_same_element_keys_align() {
        // ours renames A→A2 (rung b key from BASE name "A"); theirs moves A.
        // Keys must MATCH so the pair classifies (C4-same-target check etc.)
        let mut base = doc(vec![clip("A", "a", 0, 24), clip("B", "b", 0, 24)]);
        stamp(&mut base);
        let mut ours = base.clone();
        let mut theirs = base.clone();
        ours.tracks.children[0].children[0].name = "A2".into();
        // theirs moves A after B
        let a = theirs.tracks.children[0].children.remove(0);
        theirs.tracks.children[0].children.push(a);
        let (merged, _report) = merge(&base, &ours, &theirs).unwrap();
        let names: Vec<&str> = merged.tracks.children[0]
            .children
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        // move + rename: element moved AND carries the rename
        assert_eq!(names, ["B", "A2"]);
    }
}
