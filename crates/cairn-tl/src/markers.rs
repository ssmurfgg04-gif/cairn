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

/// Attach every note as a 1-frame marker on the timeline's stack,
/// returning a NEW timeline (the input is untouched — the caller decides
/// whether to write it out).
pub fn notes_to_otio(timeline: &Timeline, notes: &NoteSet) -> Timeline {
    let rate = timeline_rate(timeline);
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

/// FCP7 XML markers for interchange (Premiere / Resolve / FCP import).
/// Frame-anchored: `<start>` is the frame number at the note's rate.
pub fn notes_to_fcpxml(notes: &NoteSet, rate: i64, sequence_name: &str) -> String {
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
        rate,
        if matches!(rate, 24 | 30 | 60) { "FALSE" } else { "TRUE" }
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
    fn fcpxml_markers_are_frame_anchored_and_escaped() {
        let xml = notes_to_fcpxml(&notes(), 24, "Brand Film & Co");
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
        let xml2 = notes_to_fcpxml(&long, 24, "t");
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
        let xml = notes_to_fcpxml(&neg, 24, "t");
        assert!(xml.contains("<start>0</start>"));
    }
}
