//! FCPXML bridge tests: the lossiness ledger as a tested fixture (ADR §4),
//! the out-of-ledger REFUSAL contract, and the full FCPXML→OTIO→merge
//! pipeline (two editors on the same .fcpxml).

use cairn_tl::fcpxml::{lossiness_ledger, parse_fcpxml, BridgeError};
use cairn_tl::merge::{merge, Outcome};
use cairn_tl::model::*;

/// A minimal but real-shaped FCPXML 1.11 document: two asset-clips, a gap,
/// a marker, a lane title, one format, two assets.
const DOC: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE fcpxml>
<fcpxml version="1.11">
  <resources>
    <format id="r1" name="FFVideoFormat1080p24" frameDuration="100/2400s" width="1920" height="1080"/>
    <asset id="a1" name="Sunrise A" uid="123" start="3600/2400s" duration="4800/2400s" hasVideo="1" hasAudio="0" format="r1">
      <media-rep kind="original-media" src="file:///media/sunrise_a.mov"/>
    </asset>
    <asset id="a2" name="Sunrise B" uid="456" start="0s" duration="2400/2400s" hasVideo="1" hasAudio="0" format="r1">
      <media-rep kind="original-media" src="file:///media/sunrise_b.mov"/>
    </asset>
  </resources>
  <library>
    <event name="Round 12">
      <project name="Bridge Test">
        <sequence id="s1" format="r1" duration="8400/2400s" tcStart="0s" tcFormat="NDF" name="Seq">
          <spine>
            <asset-clip name="Sunrise A" ref="a1" offset="0s" duration="4800/2400s" start="3600/2400s" role="main">
              <marker start="1200/2400s" duration="100/2400s" value="Check this cut"/>
            </asset-clip>
            <gap name="Gap" offset="4800/2400s" duration="1200/2400s"/>
            <asset-clip name="Sunrise B" ref="a2" offset="6000/2400s" duration="2400/2400s" start="0s"/>
            <title name="Lower Third" lane="1" offset="6000/2400s" duration="1200/2400s" ref="r1">
              <text>LICENSED BY CAIRN</text>
            </title>
          </spine>
        </sequence>
      </project>
    </event>
  </library>
</fcpxml>
"#;

fn tv(v: i128, d: i128) -> TimeVal {
    TimeVal {
        value: cairn_tl::rational::Rational::new(v, 1).unwrap(),
        rate: cairn_tl::rational::Rational::new(d, 1).unwrap(),
    }
}

#[test]
fn bridge_parses_structure() {
    let tl = parse_fcpxml(DOC).unwrap();
    assert_eq!(tl.name, "Bridge Test");
    // tracks: V1 (spine) + V2 (lane 1) — the stacked-track approximation
    assert_eq!(tl.tracks.children.len(), 2);
    let v1 = &tl.tracks.children[0];
    let v2 = &tl.tracks.children[1];
    assert_eq!(v1.children.len(), 3, "two clips + gap on the spine");
    assert_eq!(v2.children.len(), 1, "lane title on the stacked track");

    // spine item 0: asset-clip with exact source_range from num/den attrs
    let clip = &v1.children[0];
    assert!(matches!(clip.kind, Kind::Clip));
    assert_eq!(clip.name, "Sunrise A");
    let sr = clip.source_range.as_ref().unwrap();
    // start 3600/2400s, duration 4800/2400s — exact rationals
    assert_eq!(sr.start.value, tv(3600, 2400).value);
    assert_eq!(sr.start.rate, tv(1, 2400).rate);
    assert_eq!(sr.duration.value, tv(4800, 2400).value);
    // media reference from the asset's media-rep
    assert_eq!(
        clip.active_media_url().as_deref(),
        Some("file:///media/sunrise_a.mov")
    );

    // marker carried over with exact times
    assert_eq!(clip.markers.len(), 1);
    assert_eq!(clip.markers[0].name, "Check this cut");

    // gap
    assert!(matches!(v1.children[1].kind, Kind::Gap));
    assert_eq!(
        v1.children[1].source_range.as_ref().unwrap().duration.value,
        tv(1200, 2400).value
    );

    // identity stamped at ingest (capture substrate)
    assert!(tl.walk().iter().all(|e| e.cairn_uuid().is_some()));
}

#[test]
fn ledger_entries_fire() {
    let tl = parse_fcpxml(DOC).unwrap();
    // title element ledgered
    let v2 = &tl.tracks.children[1];
    let title = &v2.children[0];
    let cairn = title.metadata.get("cairn").unwrap();
    assert_eq!(cairn["fcpxml"]["element"], "title");
    // roles preserved verbatim (ledger entry)
    let clip = &tl.tracks.children[0].children[0];
    let cairn = clip.metadata.get("cairn").unwrap();
    assert_eq!(cairn["fcpxml"]["roles"]["role"], "main");
    // and the ledger itself is non-empty and specific
    assert!(lossiness_ledger().len() >= 6);
    assert!(lossiness_ledger()
        .iter()
        .all(|e| !e.fcpxml_feature.is_empty()));
}

#[test]
fn out_of_ledger_refuses_with_element_named() {
    // an element the bridge does not know, inside the spine → C10 refusal
    let bad = DOC.replace(
        "<title name=\"Lower Third\" lane=\"1\" offset=\"6000/2400s\" duration=\"1200/2400s\" ref=\"r1\">",
        "<mystery-widget secret=\"1\">",
    );
    let err = parse_fcpxml(&bad).unwrap_err();
    match &err {
        BridgeError::Unsupported { element, .. } => {
            assert_eq!(
                element, "mystery-widget",
                "the refusal must name the element"
            );
        }
        other => panic!("expected Unsupported, got {other:?}"),
    }
    assert!(err.to_string().starts_with("C10"));
}

#[test]
fn ref_clip_and_multicam_ledgered_not_refused() {
    let compound = DOC.replace(
        "<asset-clip name=\"Sunrise B\" ref=\"a2\" offset=\"6000/2400s\" duration=\"2400/2400s\" start=\"0s\"/>",
        "<ref-clip name=\"Compound\" offset=\"6000/2400s\" duration=\"2400/2400s\" ref=\"r2\"/>",
    );
    let tl = parse_fcpxml(&compound).unwrap();
    let v1 = &tl.tracks.children[0];
    let compound_el = v1.children.iter().find(|e| e.name == "Compound").unwrap();
    assert_eq!(
        compound_el.metadata["cairn"]["fcpxml"]["ledger"],
        "compound-flattened"
    );

    let mc = DOC.replace(
        "<asset-clip name=\"Sunrise B\" ref=\"a2\" offset=\"6000/2400s\" duration=\"2400/2400s\" start=\"0s\"/>",
        "<mc-clip name=\"Multicam\" offset=\"6000/2400s\" duration=\"2400/2400s\" ref=\"r3\"/>",
    );
    let tl = parse_fcpxml(&mc).unwrap();
    let el = tl.tracks.children[0]
        .children
        .iter()
        .find(|e| e.name == "Multicam")
        .unwrap();
    assert_eq!(
        el.metadata["cairn"]["fcpxml"]["ledger"],
        "multicam-flattened"
    );
}

#[test]
fn bad_version_refuses() {
    let v2 = DOC.replace("version=\"1.11\"", "version=\"2.0\"");
    assert!(parse_fcpxml(&v2).is_err());
}

#[test]
fn bad_time_refuses() {
    let bad = DOC.replace("duration=\"4800/2400s\"", "duration=\"notatime\"");
    assert!(parse_fcpxml(&bad).is_err());
}

/// The full pipeline: FCPXML in on both sides → OTIO merge → canonical OTIO
/// out (the NLE imports OTIO; Cairn never writes vendor XML).
#[test]
fn fcpxml_two_editor_pipeline() {
    let base = parse_fcpxml(DOC).unwrap();
    // editor A: renames the first clip (name attr in FCPXML)
    let mut ours = base.clone();
    ours.tracks.children[0].children[0].name = "Sunrise A v2".into();
    // editor B: trims the second clip's tail by 600 ticks
    let mut theirs = base.clone();
    theirs.tracks.children[0].children[2].source_range = Some(TimeRange {
        start: tv(0, 2400),
        duration: tv(1800, 2400),
    });

    let (merged, report) = merge(&base, &ours, &theirs).unwrap();
    assert_eq!(
        report.outcome,
        Outcome::Clean,
        "disjoint FCPXML edits merge clean"
    );
    let v1 = &merged.tracks.children[0];
    assert_eq!(v1.children[0].name, "Sunrise A v2");
    // exact TIME (seconds), not the tick form — the trim keeps the element
    // rate, which may differ from FCPXML's tick denominator
    assert_eq!(
        v1.children[2]
            .source_range
            .as_ref()
            .unwrap()
            .duration
            .seconds()
            .unwrap(),
        cairn_tl::rational::Rational::new(1800, 2400).unwrap()
    );
    // canonical OTIO output is parseable and byte-stable (the NLE import form)
    let bytes = cairn_tl::canon::serialize_file(&merged).unwrap();
    let reparsed = cairn_tl::parse::parse_otio(&bytes).unwrap();
    assert_eq!(reparsed, merged);
}

/// FCPXML sides go through the same identity stamping → the merged output is
/// itself re-mergeable (chained merges keep working).
#[test]
fn fcpxml_merge_output_is_stable_base() {
    let base = parse_fcpxml(DOC).unwrap();
    let mut ours = base.clone();
    ours.tracks.children[0].children[1].source_range = Some(TimeRange {
        start: tv(0, 2400),
        duration: tv(600, 2400),
    });
    let (round1, r1) = merge(&base, &ours, &base.clone()).unwrap();
    assert_eq!(r1.outcome, Outcome::Clean);
    // round 2 on the merged doc
    let mut theirs = round1.clone();
    theirs.tracks.children[0].children[2].name = "Renamed B".into();
    let (round2, r2) = merge(&round1, &round1.clone(), &theirs).unwrap();
    assert_eq!(r2.outcome, Outcome::Clean);
    assert_eq!(round2.tracks.children[0].children[2].name, "Renamed B");
}
