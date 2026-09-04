//! The NLE marker bridge (ADR-0020 §5): client review comments flow BACK
//! into the edit as timeline markers — the Frame.io loop's second half.
//! Editors should never hand-copy notes; the sound team should never
//! transcribe them either.
//!
//! Two export shapes:
//!
//! * **OTIO** ([`notes_to_otio`]): the canonical timeline with every
//!   comment attached as a 1-frame `Marker.1` on the tracks stack
//!   (name = `author · body`, comment = body, color by status). Round-trips
//!   through cairn's own OTIO parser — the marker audit (`verify`) sees
//!   them like hand-placed markers.
//! * **FCP7 XML** ([`notes_to_fcpxml`]): the classic interchange marker
//!   dialect Premiere, Resolve, and FCP all import. Frame-anchored
//!   `<marker>` elements on a minimal sequence.
//!
//! Frame math is integer-exact: a note anchored at frame F becomes a
//! marker at RationalTime { value: F, rate: R } — no float drift, so
//! re-import lands on the identical frame the client clicked.
//!
//! The rate R must be the review version's TRUE rational (25/1,
//! 24000/1001, …) — never a hardcoded default. The first dogfood run
//! caught exactly that bug: markers exported at a fixed 24 while the cut
//! was 25 fps landed 1.7 s late at the one-minute mark; a 23.976 cut
//! with `ntsc=FALSE` drifted a frame every ~42 s.

use crate::model::{JsonMap, Marker, TimeRange, TimeVal, Timeline};
use crate::notes::{Note, NoteSet};
use crate::rational::Rational;

fn rat(n: i128, d: i128) -> Rational {
    Rational::new(n, d).unwrap_or_else(|_| Rational::new(0, 1).expect("zero is lossless"))
}

fn frame_marker(n: &Note, rate: Rational) -> Marker {
    let frame = rat(n.anchor.frame.max(0), 1);
    let r = frame_marker_range(frame, rate);
    let mut metadata = JsonMap::new();
    metadata.insert("cairn/comment-id".into(), serde_json::json!(n.id));
    metadata.insert(
        "cairn/note-status".into(),
        serde_json::json!(n.status.as_str()),
    );
    Marker {
        schema: "Marker.1".into(),
        name: format!("{} \u{00b7} {}", n.author, one_line(&n.body, 80)),
        color: match n.status.as_str() {
            "RESOLVED" => "Green".into(),
            "REJECTED" => "Red".into(),
            _ => "Yellow".into(),
        },
        comment: one_line(&n.body, 400),
        marked_range: r,
        metadata,
        extra: JsonMap::new(),
    }
}

fn frame_marker_range(frame: Rational, rate: Rational) -> TimeRange {
    TimeRange {
        start: TimeVal { value: frame, rate },
        duration: TimeVal {
            value: rat(1, 1),
            rate,
        },
    }
}

/// First line of a note body, capped for a marker name.
fn one_line(body: &str, cap: usize) -> String {
    let first = body.lines().next().unwrap_or("");
    let mut s: String = first.chars().take(cap).collect();
    if first.chars().count() > cap {
        s.push('\u{2026}');
    }
    s
}

/// Attach every note as a 1-frame marker on the timeline's stack at the
/// timeline's own rate, returning a NEW timeline (the input is untouched
/// — the caller decides whether to write it out). Prefer
/// [`notes_to_otio_at`] when the notes belong to a review version: the
/// version's true rate is the honest one.
pub fn notes_to_otio(timeline: &Timeline, notes: &NoteSet) -> Timeline {
    notes_to_otio_at(timeline, notes, None)
}

/// [`notes_to_otio`] with an explicit marker rate (the review version's
/// true fps as a rational). `None` falls back to the timeline rate.
/// The marker VALUE stays the frame index — NDF frames counted on the
/// integer basis — so real time = frame / true-rate exactly as the media
/// plays, and the NLE snaps the marker to the identical frame the client
/// clicked.
pub fn notes_to_otio_at(timeline: &Timeline, notes: &NoteSet, rate: Option<Rational>) -> Timeline {
    let rate = rate.unwrap_or_else(|| timeline_rate(timeline));
    let mut out = timeline.clone();
    let mut markers: Vec<Marker> = notes
        .notes
        .values()
        .map(|n| frame_marker(n, rate))
        .collect();
    markers.sort_by(|a, b| {
        a.marked_range
            .start
            .value
            .cmp_exact(b.marked_range.start.value)
            .then_with(|| a.name.cmp(&b.name))
    });
    out.tracks.markers = markers;
    out
}

/// The timeline's working rate (global start time's rate if set, else
/// 24 — markers carry their own rate so this is only the default).
fn timeline_rate(timeline: &Timeline) -> Rational {
    timeline
        .global_start_time
        .as_ref()
        .map(|t| t.rate)
        .unwrap_or_else(|| rat(24, 1))
}

/// FCP7 XML rate fields for a true rational fps: the `<timebase>` is the
/// integer count basis and `<ntsc>TRUE</ntsc>` marks the 1001-derived
/// rates (23.976 / 29.97 / 59.94). True integer rates (24, 25, 30, 60)
/// are always `ntsc=FALSE` — PAL 25 with ntsc=TRUE is a real import bug
/// some tools exhibit.
///
/// Exotic fractional rates that are neither integer nor 1001-derived
/// (e.g. 12.5) cannot be expressed in FCP7 at all; they round up to the
/// next basis here (documented drift — use the OTIO export, which is
/// exact, for those).
pub fn fcpxml_rate_fields(fps_num: i64, fps_den: i64) -> (i64, bool) {
    let den = fps_den.max(1);
    let ntsc = den > 1 && fps_num % den != 0;
    // ceil: 24000/1001 -> 24, 30000/1001 -> 30, 25/1 -> 25
    let basis = (fps_num + den - 1) / den;
    (basis.max(1), ntsc)
}

/// The true rational rate for an fps pair, always well-formed.
pub fn true_rate(fps_num: i64, fps_den: i64) -> Rational {
    rat(i128::from(fps_num.max(1)), i128::from(fps_den.max(1)))
}

/// FCP7 XML markers for interchange (Premiere / Resolve / FCP import).
/// Frame-anchored: `<start>` is the frame number at the version's rate —
/// pass the fields from [`fcpxml_rate_fields`] computed from the TRUE
/// fps, never a hardcoded default.
pub fn notes_to_fcpxml(notes: &NoteSet, timebase: i64, ntsc: bool, sequence_name: &str) -> String {
    let esc = |s: &str| {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
    };
    let mut out = String::new();
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.push_str("<!DOCTYPE xmeml>\n");
    out.push_str("<xmeml version=\"4\">\n");
    out.push_str(&format!(
        "  <sequence id=\"cairn-notes\">\n    <name>{}</name>\n    <rate><timebase>{}</timebase><ntsc>{}</ntsc></rate>\n",
        esc(sequence_name),
        timebase,
        if ntsc { "TRUE" } else { "FALSE" }
    ));
    let mut rows: Vec<&Note> = notes.notes.values().collect();
    rows.sort_by(|a, b| a.anchor.frame.cmp(&b.anchor.frame).then(a.id.cmp(&b.id)));
    for n in rows {
        let frame = n.anchor.frame.max(0);
        out.push_str("    <marker>\n");
        out.push_str(&format!(
            "      <name>[{}] {} \u{00b7} {}</name>\n",
            n.status.as_str(),
            esc(&n.author),
            esc(&one_line(&n.body, 80))
        ));
        out.push_str(&format!("      <start>{}</start>\n", frame));
        out.push_str("      <duration>1</duration>\n");
        out.push_str(&format!(
            "      <comment>{}</comment>\n",
            esc(&one_line(&n.body, 400))
        ));
        out.push_str("    </marker>\n");
    }
    out.push_str("  </sequence>\n</xmeml>\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Kind;
    use crate::notes::{NoteAnchor, NoteStatus};

    fn notes() -> NoteSet {
        NoteSet::from_notes([
            Note::new(
                "jane",
                "hold this shot longer\nmaybe 12 frames",
                NoteAnchor {
                    clip: None,
                    frame: 42,
                    rate: 24,
                },
                NoteStatus::Open,
                100,
            ),
            Note::new(
                "bob",
                "music too loud",
                NoteAnchor {
                    clip: None,
                    frame: 100,
                    rate: 24,
                },
                NoteStatus::Resolved,
                200,
            ),
        ])
    }

    fn timeline() -> Timeline {
        Timeline {
            name: "Brand Film".into(),
            global_start_time: None,
            metadata: JsonMap::new(),
            tracks: crate::model::Element::container(Kind::Stack, "tracks", Vec::new()),
            extra: JsonMap::new(),
        }
    }

    #[test]
    fn otio_markers_are_one_frame_and_sorted() {
        let tl = timeline();
        let with = notes_to_otio(&tl, &notes());
        assert_eq!(with.tracks.markers.len(), 2);
        // input untouched
        assert!(tl.tracks.markers.is_empty());
        let m0 = &with.tracks.markers[0];
        assert_eq!(m0.marked_range.start.value, rat(42, 1));
        assert_eq!(m0.marked_range.start.rate, rat(24, 1));
        assert_eq!(m0.marked_range.duration.value, rat(1, 1));
        assert_eq!(m0.schema, "Marker.1");
        assert_eq!(m0.color, "Yellow");
        assert!(m0.name.starts_with("jane \u{00b7} hold this shot longer"));
        let m1 = &with.tracks.markers[1];
        assert_eq!(m1.marked_range.start.value, rat(100, 1));
        assert_eq!(m1.color, "Green"); // resolved
                                       // comment id is tracked in metadata
        assert!(m0.metadata.contains_key("cairn/comment-id"));
    }

    #[test]
    fn fcpxml_rate_fields_match_the_standard_rates() {
        // true integer rates: exact basis, never NTSC
        assert_eq!(fcpxml_rate_fields(24, 1), (24, false));
        assert_eq!(fcpxml_rate_fields(25, 1), (25, false));
        assert_eq!(fcpxml_rate_fields(30, 1), (30, false));
        assert_eq!(fcpxml_rate_fields(60, 1), (60, false));
        // 1001-derived: integer basis + ntsc=TRUE
        assert_eq!(fcpxml_rate_fields(24000, 1001), (24, true));
        assert_eq!(fcpxml_rate_fields(30000, 1001), (30, true));
        assert_eq!(fcpxml_rate_fields(60000, 1001), (60, true));
    }

    #[test]
    fn otio_markers_can_carry_the_versions_true_rate() {
        // the TC-drift regression: a 23.976 review version exported with
        // the old timeline-default 24 rate drifted a frame every ~42 s
        let tl = timeline();
        let with = notes_to_otio_at(&tl, &notes(), Some(true_rate(24000, 1001)));
        assert_eq!(
            with.tracks.markers[0].marked_range.start.rate,
            rat(24000, 1001)
        );
        // frame index unchanged: value stays the clicked frame
        assert_eq!(with.tracks.markers[0].marked_range.start.value, rat(42, 1));
        // and the plain variant still uses the timeline rate
        let plain = notes_to_otio(&tl, &notes());
        assert_eq!(plain.tracks.markers[0].marked_range.start.rate, rat(24, 1));
    }

    #[test]
    fn fcpxml_markers_are_frame_anchored_and_escaped() {
        let xml = notes_to_fcpxml(&notes(), 24, false, "Brand Film & Co");
        assert!(xml.contains("<!DOCTYPE xmeml>"));
        assert!(xml.contains("Brand Film &amp; Co"));
        assert!(xml.contains("<start>42</start>"));
        assert!(xml.contains("<start>100</start>"));
        assert!(xml.contains("[RESOLVED] bob"));
        assert!(xml.contains("[OPEN] jane"));
        // long first lines are capped with an ellipsis, never breaking XML
        let long = NoteSet::from_notes([Note::new(
            "x",
            "word ".repeat(60),
            NoteAnchor {
                clip: None,
                frame: 0,
                rate: 24,
            },
            NoteStatus::Open,
            1,
        )]);
        // ntsc=TRUE path renders the flag
        let xml_ntsc = notes_to_fcpxml(&notes(), 24, true, "t");
        assert!(xml_ntsc.contains("<ntsc>TRUE</ntsc>"));
        assert!(xml.contains("<ntsc>FALSE</ntsc>"));
        let xml2 = notes_to_fcpxml(&long, 25, false, "t");
        assert!(xml2.contains('\u{2026}'));
        // still exactly one marker element (long bodies never break the XML)
        assert_eq!(xml2.matches("<marker>").count(), 1);
    }

    #[test]
    fn marker_round_trips_through_canon_serialize() {
        let tl = timeline();
        let with = notes_to_otio(&tl, &notes());
        let json = crate::canon::serialize(&with).unwrap();
        // parses back and the markers survive (the round-trip audit's job)
        let back = crate::parse::parse_otio(&json).unwrap();
        assert_eq!(back.tracks.markers.len(), 2);
        assert_eq!(back.tracks.markers[0].marked_range.start.value, rat(42, 1));
    }

    #[test]
    fn negative_frames_clamp_to_zero_not_panic() {
        let neg = NoteSet::from_notes([Note::new(
            "x",
            "offline media",
            NoteAnchor {
                clip: None,
                frame: -5,
                rate: 24,
            },
            NoteStatus::Open,
            1,
        )]);
        let tl = timeline();
        let with = notes_to_otio(&tl, &neg);
        assert_eq!(with.tracks.markers[0].marked_range.start.value, rat(0, 1));
        let xml = notes_to_fcpxml(&neg, 24, false, "t");
        assert!(xml.contains("<start>0</start>"));
    }
}
