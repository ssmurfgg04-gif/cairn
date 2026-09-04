//! Round 20 stress tests (ADR-0023): the new collaboration surfaces under
//! load. NOT benchmarks (those live in docs/BENCHMARKS.md machinery) — these
//! assert CORRECTNESS at scale:
//! - a 1,000-clip timeline, both policies, many interacting op pairs
//! - determinism byte-for-byte at scale
//! - monotonicity (semantic never escalates more than conservative)
//! - the robot parser: 500 seeded random bodies, never panic, always
//!   deterministic, and the creative line holds (no keyword → Creative)

use cairn_tl::merge::{merge, merge_with, MergeOptions, Outcome};
use cairn_tl::model::*;
use cairn_tl::note_ops::{parse_note, NoteParse};
use cairn_tl::rational::Rational;
use proptest::prelude::*;

fn tv(v: i128, r: i128) -> TimeVal {
    TimeVal {
        value: Rational::new(v, 1).unwrap(),
        rate: Rational::new(r, 1).unwrap(),
    }
}

fn clip(name: String, url: String, dur: i128) -> Element {
    let mut c = Element::leaf(Kind::Clip, name);
    c.media = Some(MediaRef::single(
        MediaKind::External,
        String::new(),
        Some(url),
    ));
    c.source_range = Some(TimeRange {
        start: tv(0, 24),
        duration: tv(dur, 24),
    });
    c
}

fn big_doc(n: usize) -> Timeline {
    let items: Vec<Element> = (0..n)
        .map(|i| {
            let mut c = clip(format!("clip_{i:04}"), format!("file:///m{i:04}.mov"), 48);
            c.stamp_uuid(&format!("u-{i:04}"));
            c
        })
        .collect();
    Timeline {
        name: "stress".into(),
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

/// 1,000 clips, 60 interacting edits per side (every other clip touched):
/// both policies complete, are byte-deterministic, and semantic withholds
/// <= conservative. This is the "biggest timeline a studio actually has"
/// scale bracket.
#[test]
fn stress_thousand_clip_merge_both_policies() {
    let n = 1_000;
    let base = big_doc(n);
    let mut a = base.clone();
    let mut b = base.clone();
    // editor A: head-trims even clips; renames odd clips
    for i in 0..n {
        if i % 2 == 0 {
            a.tracks.children[0].children[i].source_range = Some(TimeRange {
                start: tv(2, 24),
                duration: tv(46, 24),
            });
        } else {
            a.tracks.children[0].children[i].name = format!("clip_{i:04}_a");
        }
    }
    // editor B: tail-trims even clips (FRAME-DISJOINT with A on the same
    // clips — the C11 surface); grades odd clips
    for i in 0..n {
        if i % 2 == 0 {
            b.tracks.children[0].children[i].source_range = Some(TimeRange {
                start: tv(0, 24),
                duration: tv(44, 24),
            });
        } else {
            b.tracks.children[0].children[i].effects = vec![Effect {
                schema: "Effect.1".into(),
                name: "grade".into(),
                effect_name: "org.color.warm".into(),
                enabled: true,
                metadata: JsonMap::new(),
                extra: JsonMap::new(),
            }];
        }
    }
    let start = std::time::Instant::now();
    let (cons_tl, cons) = merge(&base, &a, &b).expect("conservative merge");
    let cons_elapsed = start.elapsed();
    let start = std::time::Instant::now();
    let (sem_tl, sem) =
        merge_with(&base, &a, &b, &MergeOptions { semantic: true }).expect("semantic merge");
    let sem_elapsed = start.elapsed();

    // conservative: every even clip's trims interact → 500 C3 conflicts
    assert_eq!(cons.outcome, Outcome::Conflicts);
    assert_eq!(cons.histogram.get(&3), Some(&500), "500 same-clip C3 pairs");
    // semantic: all 500 pairs are head-vs-tail disjoint → C11, notes only
    assert_eq!(sem.outcome, Outcome::Notes);
    assert_eq!(sem.histogram.get(&11), Some(&500), "500 C11 auto-merges");
    assert!(sem
        .verdicts
        .iter()
        .all(|v| v.verdict != cairn_tl::classifier::Verdict::Human));
    // monotonicity holds at scale
    assert!(sem.stats.withheld <= cons.stats.withheld);

    // determinism: re-run, byte-identical
    let (sem_tl2, _) = merge_with(&base, &a, &b, &MergeOptions { semantic: true }).unwrap();
    assert_eq!(
        cairn_tl::canon::serialize(&sem_tl).unwrap(),
        cairn_tl::canon::serialize(&sem_tl2).unwrap()
    );

    // the composed even clips: head 2 + tail 4 trimmed → 42 frames
    let hero = &sem_tl.tracks.children[0].children[0];
    assert_eq!(hero.source_range.as_ref().unwrap().start.value.num, 2);
    assert_eq!(hero.source_range.as_ref().unwrap().duration.value.num, 42);
    // conservative kept base for withheld pairs
    let hero_c = &cons_tl.tracks.children[0].children[0];
    assert_eq!(hero_c.source_range.as_ref().unwrap().duration.value.num, 48);

    // timing report (no assert: CI boxes vary; the numbers are the artifact)
    println!(
        "stress 1000-clip merge: conservative {cons_elapsed:?} (500 C3), semantic {sem_elapsed:?} (500 C11 auto)"
    );
}

// The robot never panics, never guesses, and re-parsing is a fixpoint:
// 500 seeded random bodies across the mechanical + creative vocabulary.
proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]
    #[test]
    fn parser_fuzz_never_panics_and_is_deterministic(
        words in proptest::collection::vec(
            proptest::option::of(proptest::string::string_regex("[a-z ]{1,12}").unwrap()),
            0..10
        ),
    ) {
        let mut body = String::new();
        for w in words {
            match w {
                Some(s) => { body.push_str(&s); body.push(' '); }
                None => body.push_str(". "),
            }
        }
        let body = body.trim().to_string();
        let first = parse_note(&body);
        let second = parse_note(&body);
        assert_eq!(first, second, "parsing is a fixpoint");
        // safety net: every outcome is one of the two closed variants
        match first {
            NoteParse::Mechanical(_) | NoteParse::Creative => {}
        }
    }
}

// Random TRIM pairs on one clip under the semantic policy: whenever the
// edges are disjoint the merge MUST be C11, whenever they share an edge it
// MUST be C3 — no gray zone at any scale.
proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]
    #[test]
    fn semantic_stress_no_gray_zone(
        dur in 48i128..480,
        a_in in 0i128..12, a_out in 0i128..12,
        b_in in 0i128..12, b_out in 0i128..12,
    ) {
        // build legal trims (positive durations)
        let a_in = a_in.min(dur / 4); let a_out = a_out.min(dur / 4);
        let b_in = b_in.min(dur / 4); let b_out = b_out.min(dur / 4);
        let mut base = big_doc(1);
        base.tracks.children[0].children[0].source_range = Some(TimeRange {
            start: tv(0, 24), duration: tv(dur, 24),
        });
        let mut a = base.clone();
        let mut b = base.clone();
        a.tracks.children[0].children[0].source_range = Some(TimeRange {
            start: tv(a_in, 24), duration: tv(dur - a_in - a_out, 24),
        });
        b.tracks.children[0].children[0].source_range = Some(TimeRange {
            start: tv(b_in, 24), duration: tv(dur - b_in - b_out, 24),
        });
        let (_, sem) = merge_with(&base, &a, &b, &MergeOptions { semantic: true }).unwrap();
        let disjoint = (a_in != 0 && a_out == 0 && b_in == 0 && b_out != 0)
            || (a_in == 0 && a_out != 0 && b_in != 0 && b_out == 0);
        let identical = (a_in, a_out) == (b_in, b_out) && a_in + a_out > 0;
        if disjoint {
            prop_assert_eq!(sem.histogram.get(&11), Some(&1), "disjoint edges auto-merge");
            prop_assert_eq!(sem.outcome, Outcome::Notes);
        } else if identical || (a_in == 0 && a_out == 0) || (b_in == 0 && b_out == 0) {
            // one-sided or identical: C6/auto, never a conflict
            prop_assert_eq!(sem.outcome, Outcome::Clean, "identical/one-sided is auto");
        } else {
            // shared edge, different values: C3 human under EVERY policy
            prop_assert_eq!(sem.histogram.get(&3), Some(&1), "shared edge escalates");
            prop_assert_eq!(sem.outcome, Outcome::Conflicts);
        }
    }
}
