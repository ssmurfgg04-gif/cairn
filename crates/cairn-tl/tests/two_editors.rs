//! Two-editor simulation (ADR §7): full concurrent-session scenarios — the
//! "Google Docs for video editing" acceptance surface, including the fence
//! policy (ours = the save under the surviving fence) and the round-trip
//! through canonical serialization.

use cairn_tl::merge::{merge, Outcome};
use cairn_tl::model::*;
use cairn_tl::rational::Rational;

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

fn doc(tracks: Vec<(&str, Vec<Element>)>) -> Timeline {
    let track_els: Vec<Element> = tracks
        .into_iter()
        .map(|(name, items)| Element::container(Kind::Track(TrackKind::Video), name, items))
        .collect();
    Timeline {
        name: "session".into(),
        global_start_time: None,
        metadata: JsonMap::new(),
        tracks: Element::container(Kind::Stack, "tracks", track_els),
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

/// The headline scenario from the brief: "Editor A trimmed clip X by 5 frames,
/// Editor B adjusted its color (effect) — both apply cleanly."
#[test]
fn two_editors_trim_plus_grade_both_apply() {
    let mut base = doc(vec![(
        "V1",
        vec![clip("Hero", "hero", 0, 96), clip("Broll", "b", 0, 48)],
    )]);
    stamp(&mut base);
    let mut editor_a = base.clone();
    let mut editor_b = base.clone();

    // Editor A: trim Hero by 5 frames at the head (in-point later)
    editor_a.tracks.children[0].children[0].source_range = Some(TimeRange {
        start: tv(5, 24),
        duration: tv(91, 24),
    });
    // Editor B: adds a color effect (raw effects list) on Hero
    editor_b.tracks.children[0].children[0].effects = vec![Effect {
        schema: "Effect.1".into(),
        name: "warm-grade".into(),
        effect_name: "org.color.warm".into(),
        enabled: true,
        metadata: JsonMap::new(),
        extra: JsonMap::new(),
    }];

    let (merged, report) = merge(&base, &editor_a, &editor_b).unwrap();
    assert_eq!(
        report.outcome,
        Outcome::Clean,
        "trim + grade must merge automatically"
    );
    let hero = &merged.tracks.children[0].children[0];
    // exact trim arithmetic: 5-frame in delta, 96 -> 91 duration
    assert_eq!(hero.source_range.as_ref().unwrap().start.value.num, 5);
    assert_eq!(hero.source_range.as_ref().unwrap().duration.value.num, 91);
    // the grade applied
    assert_eq!(hero.effects.len(), 1);
    assert_eq!(hero.effects[0].name, "warm-grade");
}

/// Both editors cut the same scene differently → conflict copy semantics:
/// exit code 2 (Conflicts), base state preserved, report machine-readable.
#[test]
fn two_editors_same_cut_conflict_escalates() {
    let mut base = doc(vec![("V1", vec![clip("A", "a", 0, 48)])]);
    stamp(&mut base);
    let mut a = base.clone();
    let mut b = base.clone();
    a.tracks.children[0].children[0].source_range = Some(TimeRange {
        start: tv(0, 24),
        duration: tv(24, 24),
    });
    b.tracks.children[0].children[0].source_range = Some(TimeRange {
        start: tv(12, 24),
        duration: tv(36, 24),
    });
    let (merged, report) = merge(&base, &a, &b).unwrap();
    assert_eq!(report.outcome, Outcome::Conflicts);
    // base kept: 48 frames, untouched
    assert_eq!(
        merged.tracks.children[0].children[0]
            .source_range
            .as_ref()
            .unwrap()
            .duration
            .value
            .num,
        48
    );
    let json = report.to_json();
    assert_eq!(json["outcome"], "conflicts");
    assert_eq!(json["histogram"]["C3"], 1);
}

/// A serial real-world flow: base → merge → the merged doc becomes the new
/// base for the NEXT round (merge stability under iteration).
#[test]
fn merged_doc_is_a_valid_new_base() {
    let mut base = doc(vec![(
        "V1",
        vec![clip("A", "a", 0, 24), clip("B", "b", 0, 24)],
    )]);
    stamp(&mut base);
    let mut ours = base.clone();
    let mut theirs = base.clone();
    ours.tracks.children[0].children[0].name = "A2".into();
    theirs.tracks.children[0].children.push({
        let mut c = clip("NEW", "n", 0, 24);
        c.stamp_uuid("u-new");
        c
    });
    let (round1, report) = merge(&base, &ours, &theirs).unwrap();
    assert_eq!(report.outcome, Outcome::Clean);

    // canonical round-trip of the merged doc — byte-stable, parseable
    let bytes = cairn_tl::canon::serialize_file(&round1).unwrap();
    let reparsed = cairn_tl::parse::parse_otio(&bytes).unwrap();
    assert_eq!(reparsed, round1);

    // round 2: another pair of disjoint edits on the merged doc
    let mut ours2 = round1.clone();
    let mut theirs2 = round1.clone();
    ours2.tracks.children[0].children[1].name = "B2".into();
    theirs2.tracks.children[0].children[2].enabled = false;
    let (round2, report2) = merge(&round1, &ours2, &theirs2).unwrap();
    assert_eq!(report2.outcome, Outcome::Clean);
    let names: Vec<&str> = round2.tracks.children[0]
        .children
        .iter()
        .map(|e| e.name.as_str())
        .collect();
    assert_eq!(names, ["A2", "B2", "NEW"]);
}

/// The fence policy: identical inputs merge identically regardless of which
/// caller-supplied side is labeled ours — EXCEPT ordering notes (C8), which
/// are role-dependent by definition (ours first).
#[test]
fn fence_policy_ours_is_the_surviving_save() {
    let mut base = doc(vec![("V1", vec![clip("A", "a", 0, 24)])]);
    stamp(&mut base);
    let mut x = base.clone();
    let mut y = base.clone();
    x.tracks.children[0].children[0].name = "X".into();
    y.tracks.children[0].children[0].name = "Y".into();
    // the CALLER decides which side is ours (the surviving fence) — merge
    // itself is symmetric: both orderings must produce the same CONFLICT SET
    let (_, r1) = merge(&base, &x, &y).unwrap();
    let (_, r2) = merge(&base, &y, &x).unwrap();
    assert_eq!(r1.outcome, Outcome::Conflicts);
    assert_eq!(r2.outcome, Outcome::Conflicts);
    assert_eq!(r1.histogram, r2.histogram);
}

/// Editor B worked inside a NEW track while editor A edited the old track:
/// the new track + its content land, the old-track edit lands.
#[test]
fn new_track_session_merges() {
    let mut base = doc(vec![(
        "V1",
        vec![clip("A", "a", 0, 24), clip("B", "b", 0, 24)],
    )]);
    stamp(&mut base);
    let mut ours = base.clone();
    ours.tracks.children[0].children[0].name = "A2".into();
    let mut theirs = base.clone();
    // theirs: new track V2 with one new clip + one moved (B)
    let b = theirs.tracks.children[0].children.remove(1);
    let mut new_clip = clip("TITLE", "t", 0, 24);
    new_clip.stamp_uuid("u-title");
    let mut v2 = Element::container(Kind::Track(TrackKind::Video), "V2", vec![new_clip, b]);
    let v2_id = uuid::Uuid::now_v7().to_string();
    v2.stamp_uuid(&v2_id);
    theirs.tracks.children.push(v2);
    let (merged, report) = merge(&base, &ours, &theirs).unwrap();
    assert_eq!(
        report.outcome,
        Outcome::Clean,
        "new-track sessions merge clean"
    );
    assert_eq!(merged.tracks.children.len(), 2);
    let v1: Vec<&str> = merged.tracks.children[0]
        .children
        .iter()
        .map(|e| e.name.as_str())
        .collect();
    let v2: Vec<&str> = merged.tracks.children[1]
        .children
        .iter()
        .map(|e| e.name.as_str())
        .collect();
    assert_eq!(v1, ["A2"], "B left V1");
    assert_eq!(
        v2,
        ["TITLE", "B"],
        "new track carries its content + the moved B"
    );
}

/// Both editors append the SAME clip (e.g. both imported the same asset):
/// deduped to one — no phantom duplicate.
#[test]
fn both_append_same_clip_dedupes() {
    let mut base = doc(vec![("V1", vec![clip("A", "a", 0, 24)])]);
    stamp(&mut base);
    let mut ours = base.clone();
    let mut theirs = base.clone();
    for side in [&mut ours, &mut theirs] {
        side.tracks.children[0].children.push({
            let mut c = clip("SHARED", "s", 0, 24);
            c.stamp_uuid("u-shared");
            c
        });
    }
    let (merged, report) = merge(&base, &ours, &theirs).unwrap();
    assert_eq!(report.outcome, Outcome::Clean);
    let names: Vec<&str> = merged.tracks.children[0]
        .children
        .iter()
        .map(|e| e.name.as_str())
        .collect();
    assert_eq!(
        names,
        ["A", "SHARED"],
        "identical appends dedupe to one element"
    );
}

/// 50-element timeline, both editors reordering heavily: merge must terminate
/// with a coherent, total document (no drops, no duplicates among survivors).
#[test]
fn large_doc_merge_is_total() {
    let items: Vec<Element> = (0..50)
        .map(|i| {
            let mut c = clip(&format!("c{i}"), &format!("file:///m{i}.mov"), 0, 24);
            c.stamp_uuid(&format!("u-{i}"));
            c
        })
        .collect();
    let base = doc(vec![("V1", items)]);
    let mut ours = base.clone();
    let mut theirs = base.clone();
    // ours reverses the first 10; theirs reverses the last 10 (disjoint sets,
    // but overlapping regions test the LIS machinery)
    for k in 0..5 {
        ours.tracks.children[0].children.swap(k, 9 - k);
        theirs.tracks.children[0].children.swap(40 + k, 49 - k);
    }
    ours.tracks.children[0].children[3].name = "renamed".into();
    let (merged, _report) = merge(&base, &ours, &theirs).unwrap();
    let n = merged.tracks.children[0].children.len();
    assert_eq!(n, 50, "no element lost or duplicated");
    // the renamed element survived with its edit
    assert!(merged.tracks.children[0]
        .children
        .iter()
        .any(|e| e.name == "renamed"));
    // outcome may be Clean or Notes/Conflicts (C4 reorders overlap) — the
    // INVARIANT is totality: exactly 50 distinct identities.
    let ids: std::collections::HashSet<_> = merged.tracks.children[0]
        .children
        .iter()
        .map(|e| e.cairn_uuid().map(str::to_string).unwrap_or_default())
        .collect();
    assert_eq!(ids.len(), 50, "all 50 identities distinct");
}

// ---------------------------------------------------------------------------
// Round 13 — the real-corpus catch: track-level edits were invisible to the
// diff. PRONOM's authentic FCP X sample (empty spine, one V1 track) exposed
// it live: both sides' track renames produced ZERO ops and the merged output
// kept the base name — a silent drop of both editors' work. These tests pin
// the fix (Op::TrackAttr) from both directions.
// ---------------------------------------------------------------------------

#[test]
fn track_rename_one_side_applies() {
    let mut base = doc(vec![("V1", vec![clip("Hero", "hero", 0, 96)])]);
    stamp(&mut base);
    let mut ours = base.clone();
    let theirs = base.clone();
    ours.tracks.children[0].name = "V1 A-roll".into();

    let (merged, report) = merge(&base, &ours, &theirs).unwrap();
    assert_eq!(
        merged.tracks.children[0].name, "V1 A-roll",
        "a one-sided track rename MUST apply (was silently dropped before Round 13)"
    );
    assert_eq!(report.outcome, Outcome::Clean);
    assert!(report.stats.applied >= 1, "the TrackAttr op must count");
}

#[test]
fn track_rename_both_sides_conflicts_not_last_write_wins() {
    let mut base = doc(vec![("V1", vec![clip("Hero", "hero", 0, 96)])]);
    stamp(&mut base);
    let mut ours = base.clone();
    let mut theirs = base.clone();
    ours.tracks.children[0].name = "V1 A-roll".into();
    theirs.tracks.children[0].name = "V1 B-roll".into();

    let (merged, report) = merge(&base, &ours, &theirs).unwrap();
    assert_eq!(
        report.outcome,
        Outcome::Conflicts,
        "same track, same field, different values = C3 human escalation"
    );
    assert_eq!(
        merged.tracks.children[0].name, "V1",
        "conflicting track renames withhold BOTH — base kept, never last-write-wins"
    );
    assert!(report.stats.withheld >= 2);
    assert_eq!(
        *report.histogram.get(&3).unwrap_or(&0),
        1,
        "classified C3 exactly once"
    );
}

#[test]
fn track_rename_vs_track_disable_both_apply() {
    // same track, DIFFERENT fields: C2 auto — both sides' work survives
    let mut base = doc(vec![("V1", vec![clip("Hero", "hero", 0, 96)])]);
    stamp(&mut base);
    let mut ours = base.clone();
    let mut theirs = base.clone();
    ours.tracks.children[0].name = "V1 A-roll".into();
    theirs.tracks.children[0].enabled = false;

    let (merged, report) = merge(&base, &ours, &theirs).unwrap();
    assert_eq!(merged.tracks.children[0].name, "V1 A-roll");
    assert!(
        !merged.tracks.children[0].enabled,
        "the disable applies too"
    );
    assert_eq!(report.outcome, Outcome::Clean);
}

#[test]
fn identical_track_renames_apply_once() {
    let mut base = doc(vec![("V1", vec![clip("Hero", "hero", 0, 96)])]);
    stamp(&mut base);
    let mut ours = base.clone();
    let mut theirs = base.clone();
    ours.tracks.children[0].name = "V1 A-roll".into();
    theirs.tracks.children[0].name = "V1 A-roll".into();

    let (merged, report) = merge(&base, &ours, &theirs).unwrap();
    assert_eq!(merged.tracks.children[0].name, "V1 A-roll");
    assert_eq!(report.outcome, Outcome::Clean);
    assert!(report.stats.deduped >= 1, "identical ops dedupe to ours");
}

#[test]
fn track_rename_vs_clip_edit_inside_independent() {
    // track attr + clip trim inside that track: independent aspects (C0),
    // both apply — the pre-Round-13 engine dropped the rename entirely
    let mut base = doc(vec![("V1", vec![clip("Hero", "hero", 0, 96)])]);
    stamp(&mut base);
    let mut ours = base.clone();
    let mut theirs = base.clone();
    ours.tracks.children[0].name = "V1 A-roll".into();
    theirs.tracks.children[0].children[0].source_range = Some(TimeRange {
        start: tv(5, 24),
        duration: tv(91, 24),
    });

    let (merged, report) = merge(&base, &ours, &theirs).unwrap();
    assert_eq!(merged.tracks.children[0].name, "V1 A-roll");
    assert_eq!(
        merged.tracks.children[0].children[0]
            .source_range
            .as_ref()
            .unwrap()
            .start
            .value,
        tv(5, 24).value
    );
    assert_eq!(report.outcome, Outcome::Clean);
}

#[test]
fn track_remove_vs_track_rename_escalates() {
    // C9: removing a track the other side renamed — human decides
    let mut base = doc(vec![
        ("V1", vec![clip("Hero", "hero", 0, 96)]),
        ("V2", vec![clip("Overlay", "ov", 0, 48)]),
    ]);
    stamp(&mut base);
    let mut ours = base.clone();
    let theirs = base.clone();
    ours.tracks.children.remove(1); // remove V2
    let mut theirs = theirs;
    // match by index: theirs renames what base index 1 is
    theirs.tracks.children[1].name = "V2 titles".into();

    let (merged, report) = merge(&base, &ours, &theirs).unwrap();
    assert_eq!(
        report.outcome,
        Outcome::Conflicts,
        "C9: track removal vs edit on that track escalates"
    );
    // V2 kept (removal withheld, deletion never wins silently)
    assert_eq!(merged.tracks.children.len(), 2);
}
