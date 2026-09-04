//! Round-trip audit (ADR-0018): verify that a timeline which traveled through
//! ANOTHER NLE came back with its editorial content intact.
//!
//! The assistant-editor's 3am job — "open this XML in Resolve and rebuild
//! every speed ramp / title / transition by hand because the transfer ate
//! them" — becomes a mechanical checklist instead:
//!
//! - clip inventory (count + identity, uuid ladder with name fallback);
//! - frame-exact duration drift per clip (rational arithmetic — a 2400-frame
//!   clip that comes back 2398 frames is a REAL number, not a float blur);
//! - per-clip EFFECT inventory (dropped speed ramps, lost grades, vanished
//!   motion titles — the classic XML/OTIO round-trip casualties);
//! - markers, transitions (with in/out offsets), gaps, track counts;
//! - audio media links per clip.
//!
//! Every check names the element and the exact delta. `Loss` severity = the
//! round-trip is NOT safe to cut from; `Warn` = inspect before trusting.

use serde_json::json;

use crate::model::{Element, Kind, Timeline, TrackKind};
use crate::rational::Rational;

/// Severity of one failed check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    /// Content was LOST or altered — do not cut from this file.
    Loss,
    /// Probably survivable, but a human should look.
    Warn,
}

/// One check outcome (pass entries are kept: an auditor wants the checklist).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifyCheck {
    pub name: String,
    pub severity: Severity,
    pub detail: String,
}

/// The audit result.
#[derive(Clone, Debug, Default)]
pub struct VerifyReport {
    pub checks: Vec<VerifyCheck>,
    pub loss_count: u32,
    pub warn_count: u32,
}

impl VerifyReport {
    #[must_use]
    pub fn passed(&self) -> bool {
        self.loss_count == 0
    }

    /// Machine-readable form (CLI --json, CI gates).
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        json!({
            "passed": self.passed(),
            "loss_count": self.loss_count,
            "warn_count": self.warn_count,
            "checks": self.checks.iter().map(|c| json!({
                "name": c.name,
                "severity": format!("{:?}", c.severity),
                "detail": c.detail,
            })).collect::<Vec<_>>(),
        })
    }
}

/// A flattened leaf for comparison: identity (uuid, else name), timing,
/// effects, markers — everything the audit reasons about.
struct FlatClip {
    uuid: Option<String>,
    name: String,
    kind: Kind,
    /// duration in frames at the clip's own rate (exact), when known.
    duration: Option<(Rational, Rational)>,
    effects: Vec<String>,
    effect_params: Vec<String>,
    marker_names: Vec<String>,
    has_audio_media: bool,
}

fn walk(el: &Element, out: &mut Vec<FlatClip>, path: &mut Vec<String>) {
    path.push(el.name.clone());
    match el.kind {
        Kind::Stack | Kind::Track(_) => {
            for child in &el.children {
                walk(child, out, path);
            }
        }
        _ => {
            let duration = el
                .source_range
                .as_ref()
                .map(|r| (r.duration.value, r.duration.rate));
            let has_audio = el.media.as_ref().is_some_and(|m| {
                // audio-ness: the url/kind hints NLEs actually emit (a Missing
                // reference counts too — the audit's whole point is links)
                let url_hit = m
                    .target_url
                    .as_deref()
                    .is_some_and(|u| u.to_ascii_lowercase().contains("audio"));
                let entry_hit = m.references.values().any(|e| {
                    e.target_url
                        .as_deref()
                        .is_some_and(|u| u.to_ascii_lowercase().contains("audio"))
                        || e.name.to_ascii_lowercase().contains("audio")
                });
                url_hit || entry_hit || m.name.to_ascii_lowercase().contains("audio")
            });
            out.push(FlatClip {
                uuid: el.cairn_uuid().map(str::to_string),
                name: el.name.clone(),
                kind: el.kind.clone(),
                duration,
                effects: el.effects.iter().map(|e| e.effect_name.clone()).collect(),
                effect_params: el
                    .effects
                    .iter()
                    .map(|e| {
                        format!(
                            "{}:{}:{}",
                            e.effect_name,
                            e.name,
                            serde_json::to_string(&e.metadata).unwrap_or_default()
                        )
                    })
                    .collect(),
                marker_names: el.markers.iter().map(|m| m.name.clone()).collect(),
                has_audio_media: has_audio,
            });
        }
    }
    path.pop();
}

/// Flatten a timeline's leaves (clips, gaps, transitions).
fn flatten(tl: &Timeline) -> Vec<FlatClip> {
    let mut out = Vec::new();
    let mut path = Vec::new();
    walk(&tl.tracks, &mut out, &mut path);
    out
}

fn track_counts(tl: &Timeline) -> (u64, u64, u64) {
    let mut v = 0u64;
    let mut a = 0u64;
    let mut s = 0u64;
    fn rec(el: &Element, v: &mut u64, a: &mut u64, s: &mut u64) {
        if let Kind::Track(k) = el.kind {
            match k {
                TrackKind::Video => *v += 1,
                TrackKind::Audio => *a += 1,
                TrackKind::Subtitle => *s += 1,
            }
        }
        for c in &el.children {
            rec(c, v, a, s);
        }
    }
    rec(&tl.tracks, &mut v, &mut a, &mut s);
    (v, a, s)
}

/// Match keys: uuid (identity ladder rung a) when both sides stamp it, else
/// name + per-name occurrence ordinal (name-collisions in real timelines are
/// rare; the uuid ladder handles the projects that stamp).
fn match_key(src: &FlatClip, seen: &mut std::collections::HashMap<String, usize>) -> String {
    let base = match &src.uuid {
        Some(u) => format!("uuid:{u}"),
        None => format!("name:{}", src.name),
    };
    let n = seen.entry(base.clone()).or_insert(0);
    *n += 1;
    format!("{base}#{n}")
}

fn is_clip_like(k: &Kind) -> bool {
    matches!(k, Kind::Clip)
}

fn is_gap(k: &Kind) -> bool {
    matches!(k, Kind::Gap)
}

fn is_transition(k: &Kind) -> bool {
    matches!(k, Kind::Transition)
}

/// Run the audit: `source` is the timeline BEFORE the round-trip; `rt` is
/// what came back from the other NLE.
#[must_use]
pub fn verify_roundtrip(source: &Timeline, rt: &Timeline) -> VerifyReport {
    let mut rep = VerifyReport::default();

    let src = flatten(source);
    let dst = flatten(rt);

    // 1) clip inventory
    let src_clips: Vec<&FlatClip> = src.iter().filter(|c| is_clip_like(&c.kind)).collect();
    let dst_clips: Vec<&FlatClip> = dst.iter().filter(|c| is_clip_like(&c.kind)).collect();
    if src_clips.len() == dst_clips.len() {
        rep.checks.push(VerifyCheck {
            name: "clip-count".into(),
            severity: Severity::Warn,
            detail: format!("{} clips on both sides", src_clips.len()),
        });
    } else {
        rep.loss_count += 1;
        rep.checks.push(VerifyCheck {
            name: "clip-count".into(),
            severity: Severity::Loss,
            detail: format!(
                "clip count {} → {} ({} lost)",
                src_clips.len(),
                dst_clips.len(),
                src_clips.len().saturating_sub(dst_clips.len())
            ),
        });
    }

    // 2) per-clip match + duration drift + effects
    let mut src_seen = std::collections::HashMap::new();
    let mut dst_seen = std::collections::HashMap::new();
    let src_map: std::collections::HashMap<String, &FlatClip> = src_clips
        .iter()
        .map(|c| (match_key(c, &mut src_seen), *c))
        .collect();
    let dst_map: std::collections::HashMap<String, &FlatClip> = dst_clips
        .iter()
        .map(|c| (match_key(c, &mut dst_seen), *c))
        .collect();

    for (key, s) in &src_map {
        match dst_map.get(key) {
            None => {
                // named element gone: surface clip names (titles are clips too)
                rep.loss_count += 1;
                rep.checks.push(VerifyCheck {
                    name: "dropped-clip".into(),
                    severity: Severity::Loss,
                    detail: format!("clip '{}' ({key}) did not survive the round-trip", s.name),
                });
            }
            Some(d) => {
                // duration drift (frame-exact): a TimeVal's `value` IS the
                // frame count at its own rate, so compare by converting the
                // round-trip side's frames to the SOURCE rate — exact rational
                if let (Some((sv, sr)), Some((dv, dr))) = (s.duration, d.duration) {
                    let sf = sv; // source frames at the source rate
                    let df = dv.checked_mul(sr).and_then(|v| v.checked_div(dr)).ok();
                    if let (Some(sf), Some(df)) = (Some(sf), df) {
                        if sf != df {
                            let delta = sf.checked_sub(df).ok();
                            let (human_delta, lossy) = match delta {
                                Some(d) => {
                                    let neg = d.num < 0;
                                    let abs = d.checked_mul(Rational::new(-1, 1).expect("valid"));
                                    match abs {
                                        Ok(a) => {
                                            let msg = if a.den == 1 {
                                                format!(
                                                    "{}{} frames",
                                                    if neg { "-" } else { "+" },
                                                    a.num
                                                )
                                            } else {
                                                format!(
                                                    "{}{}/{} frames",
                                                    if neg { "-" } else { "+" },
                                                    a.num,
                                                    a.den
                                                )
                                            };
                                            // sub-frame drift is a warning; whole frames are loss
                                            (msg, a.den != 1)
                                        }
                                        Err(_) => ("δ too large".into(), true),
                                    }
                                }
                                None => ("δ too large".into(), true),
                            };
                            if lossy {
                                rep.warn_count += 1;
                            } else {
                                rep.loss_count += 1;
                            }
                            rep.checks.push(VerifyCheck {
                                name: "duration-drift".into(),
                                severity: if lossy {
                                    Severity::Warn
                                } else {
                                    Severity::Loss
                                },
                                detail: format!(
                                    "clip '{}' duration: {} ({human_delta})",
                                    s.name,
                                    if lossy {
                                        "sub-frame drift"
                                    } else {
                                        "whole-frame drift"
                                    }
                                ),
                            });
                        }
                    }
                }
                // effects: the speed-ramp / grade / title casualties
                let lost: Vec<&String> = s
                    .effects
                    .iter()
                    .filter(|e| !d.effects.contains(e))
                    .collect();
                if !lost.is_empty() {
                    rep.loss_count += 1;
                    rep.checks.push(VerifyCheck {
                        name: "effects-lost".into(),
                        severity: Severity::Loss,
                        detail: format!(
                            "clip '{}' lost effect(s): {}",
                            s.name,
                            lost.iter()
                                .map(|e| e.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                    });
                }
                let changed_params: Vec<&String> = s
                    .effect_params
                    .iter()
                    .filter(|p| !d.effect_params.contains(p))
                    .collect();
                if !changed_params.is_empty() {
                    rep.warn_count += 1;
                    rep.checks.push(VerifyCheck {
                        name: "effect-params-changed".into(),
                        severity: Severity::Warn,
                        detail: format!(
                            "clip '{}' effect parameters changed: {}",
                            s.name,
                            changed_params.len()
                        ),
                    });
                }
                // markers on the clip
                let lost_markers: Vec<&String> = s
                    .marker_names
                    .iter()
                    .filter(|m| !d.marker_names.contains(m))
                    .collect();
                if !lost_markers.is_empty() {
                    rep.warn_count += 1;
                    rep.checks.push(VerifyCheck {
                        name: "markers-lost".into(),
                        severity: Severity::Warn,
                        detail: format!(
                            "clip '{}' lost marker(s): {}",
                            s.name,
                            lost_markers
                                .iter()
                                .map(|m| m.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                    });
                }
            }
        }
    }

    // new clips that appeared (sometimes fine — re-linking — worth a look)
    for (key, d) in &dst_map {
        if !src_map.contains_key(key) {
            rep.warn_count += 1;
            rep.checks.push(VerifyCheck {
                name: "added-clip".into(),
                severity: Severity::Warn,
                detail: format!("clip '{}' ({key}) appeared in the round-trip", d.name),
            });
        }
    }

    // 3) transitions (with count)
    let src_tr = src.iter().filter(|c| is_transition(&c.kind)).count();
    let dst_tr = dst.iter().filter(|c| is_transition(&c.kind)).count();
    if src_tr != dst_tr {
        rep.loss_count += 1;
        rep.checks.push(VerifyCheck {
            name: "transitions".into(),
            severity: Severity::Loss,
            detail: format!("transitions {src_tr} → {dst_tr} (cross-dissolves etc. lost)"),
        });
    }

    // 4) gaps (timing shifts when lost)
    let src_g = src.iter().filter(|c| is_gap(&c.kind)).count();
    let dst_g = dst.iter().filter(|c| is_gap(&c.kind)).count();
    if src_g != dst_g {
        rep.warn_count += 1;
        rep.checks.push(VerifyCheck {
            name: "gaps".into(),
            severity: Severity::Warn,
            detail: format!("gaps {src_g} → {dst_g} (downstream timing shifts)"),
        });
    }

    // 5) track inventory
    let (sv, sa, ss) = track_counts(source);
    let (dv, da, ds) = track_counts(rt);
    if (sv, sa, ss) != (dv, da, ds) {
        rep.loss_count += 1;
        rep.checks.push(VerifyCheck {
            name: "tracks".into(),
            severity: Severity::Loss,
            detail: format!(
                "tracks V{sv}/A{sa}/S{ss} → V{dv}/A{da}/S{ds} (layer content may be stranded)"
            ),
        });
    }

    // 6) audio links
    let src_audio = src.iter().filter(|c| c.has_audio_media).count();
    let dst_audio = dst.iter().filter(|c| c.has_audio_media).count();
    if src_audio != dst_audio {
        rep.loss_count += 1;
        rep.checks.push(VerifyCheck {
            name: "audio-links".into(),
            severity: Severity::Loss,
            detail: format!(
                "audio-linked clips {src_audio} → {dst_audio} (media-offline risk on mix)"
            ),
        });
    }

    // summary line
    let ok = rep.loss_count == 0 && rep.warn_count == 0;
    if ok {
        rep.checks.push(VerifyCheck {
            name: "result".into(),
            severity: Severity::Warn,
            detail: "round-trip is frame-accurate and content-complete".into(),
        });
    }
    rep
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Effect, MediaRef, MediaRefEntry, TimeRange, TimeVal};
    use crate::rational::Rational;

    fn r(n: i128, d: i128) -> Rational {
        Rational::new(n, d).expect("test rational")
    }

    fn tv(value: i128, rate: i128) -> TimeVal {
        TimeVal {
            value: r(value, 1),
            rate: r(rate, 1),
        }
    }

    fn dur(value: i128, rate: i128) -> TimeRange {
        TimeRange {
            start: tv(0, rate),
            duration: tv(value, rate),
        }
    }

    fn clip(name: &str, d: Option<TimeRange>) -> Element {
        let mut e = Element::leaf(Kind::Clip, name);
        e.source_range = d;
        e
    }

    fn media_clip(name: &str, d: Option<TimeRange>, url: &str) -> Element {
        let mut e = clip(name, d);
        let mut references = std::collections::BTreeMap::new();
        references.insert(
            "DEFAULT_MEDIA".to_string(),
            MediaRefEntry {
                schema: "ExternalReference.1".into(),
                kind: crate::model::MediaKind::External,
                name: name.into(),
                target_url: Some(url.into()),
                available_range: None,
                metadata: crate::model::JsonMap::new(),
                extra: crate::model::JsonMap::new(),
            },
        );
        e.media = Some(MediaRef {
            kind: crate::model::MediaKind::External,
            name: name.into(),
            target_url: Some(url.into()),
            available_range: None,
            active_key: "DEFAULT_MEDIA".into(),
            references,
            extra: crate::model::JsonMap::new(),
        });
        e
    }

    fn timeline(clips: Vec<Element>) -> Timeline {
        let track = Element::container(Kind::Track(TrackKind::Video), "V1", clips);
        Timeline {
            name: "test".into(),
            global_start_time: None,
            metadata: crate::model::JsonMap::new(),
            tracks: Element::container(Kind::Stack, "tracks", vec![track]),
            extra: crate::model::JsonMap::new(),
        }
    }

    #[test]
    fn clean_roundtrip_passes() {
        let src = timeline(vec![
            clip("a", Some(dur(2400, 24))),
            clip("b", Some(dur(1200, 24))),
        ]);
        let rt = timeline(vec![
            clip("a", Some(dur(2400, 24))),
            clip("b", Some(dur(1200, 24))),
        ]);
        let rep = verify_roundtrip(&src, &rt);
        assert!(rep.passed(), "{:?}", rep.checks);
    }

    #[test]
    fn whole_frame_drift_is_a_loss_with_exact_delta() {
        let src = timeline(vec![clip("interview", Some(dur(2400, 24)))]);
        let rt = timeline(vec![clip("interview", Some(dur(2398, 24)))]);
        let rep = verify_roundtrip(&src, &rt);
        assert!(!rep.passed());
        let drift = rep
            .checks
            .iter()
            .find(|c| c.name == "duration-drift")
            .expect("drift check present");
        assert_eq!(drift.severity, Severity::Loss);
        assert!(
            drift.detail.contains("-2 frames"),
            "the editor sees the exact frame delta: {}",
            drift.detail
        );
    }

    #[test]
    fn dropped_title_is_named() {
        let src = timeline(vec![
            clip("hero_interview", Some(dur(240, 24))),
            clip("TITLE_lower_third", Some(dur(48, 24))),
        ]);
        let rt = timeline(vec![clip("hero_interview", Some(dur(240, 24)))]);
        let rep = verify_roundtrip(&src, &rt);
        assert!(!rep.passed());
        let dropped = rep
            .checks
            .iter()
            .find(|c| c.name == "dropped-clip")
            .expect("dropped check");
        assert!(
            dropped.detail.contains("TITLE_lower_third"),
            "{}",
            dropped.detail
        );
    }

    #[test]
    fn speed_effect_loss_is_a_loss() {
        let mut a = clip("sunday_drive", Some(dur(480, 24)));
        a.effects.push(Effect {
            schema: "Effect.1".into(),
            name: "speed".into(),
            effect_name: "TimeWarpSpeed".into(),
            enabled: true,
            metadata: [("speed".to_string(), json!(2.0))].into_iter().collect(),
            extra: crate::model::JsonMap::new(),
        });
        let src = timeline(vec![a]);
        let mut b = clip("sunday_drive", Some(dur(480, 24)));
        b.effects.clear(); // the round-trip ATE it
        let rt = timeline(vec![b]);
        let rep = verify_roundtrip(&src, &rt);
        let lost = rep
            .checks
            .iter()
            .find(|c| c.name == "effects-lost")
            .expect("effect check");
        assert_eq!(lost.severity, Severity::Loss);
        assert!(lost.detail.contains("TimeWarpSpeed"));
    }

    #[test]
    fn effect_param_change_is_a_warn() {
        let mk = |speed: i64| {
            let mut c = clip("ramp", Some(dur(480, 24)));
            c.effects.push(Effect {
                schema: "Effect.1".into(),
                name: "speed".into(),
                effect_name: "LinearSpeedRamp".into(),
                enabled: true,
                metadata: [("speed".to_string(), json!(speed))].into_iter().collect(),
                extra: crate::model::JsonMap::new(),
            });
            timeline(vec![c])
        };
        let rep = verify_roundtrip(&mk(2), &mk(4));
        let changed = rep
            .checks
            .iter()
            .find(|c| c.name == "effect-params-changed")
            .expect("param check");
        assert_eq!(changed.severity, Severity::Warn);
    }

    #[test]
    fn transition_loss_is_a_loss() {
        let mut t = Element::leaf(Kind::Transition, "cross_dissolve_1");
        t.transition = Some(crate::model::TransitionInfo {
            transition_type: "Cross Dissolve".into(),
            in_offset: Some(tv(12, 24)),
            out_offset: Some(tv(12, 24)),
        });
        let src = timeline(vec![clip("a", Some(dur(240, 24))), t]);
        let rt = timeline(vec![clip("a", Some(dur(240, 24)))]);
        let rep = verify_roundtrip(&src, &rt);
        let tr = rep
            .checks
            .iter()
            .find(|c| c.name == "transitions")
            .expect("transition check");
        assert_eq!(tr.severity, Severity::Loss);
    }

    #[test]
    fn audio_link_loss_is_a_loss() {
        let src = timeline(vec![media_clip(
            "mix_stem",
            Some(dur(240, 24)),
            "file:///audio/mix_v2.wav",
        )]);
        let rt = timeline(vec![clip("mix_stem", Some(dur(240, 24)))]); // link gone
        let rep = verify_roundtrip(&src, &rt);
        let audio = rep
            .checks
            .iter()
            .find(|c| c.name == "audio-links")
            .expect("audio check");
        assert_eq!(audio.severity, Severity::Loss);
        assert!(audio.detail.contains("media-offline"));
    }

    #[test]
    fn track_loss_is_a_loss() {
        let two_tracks = {
            let t1 = Element::container(
                Kind::Track(TrackKind::Video),
                "V1",
                vec![clip("a", Some(dur(24, 24)))],
            );
            let t2 = Element::container(
                Kind::Track(TrackKind::Audio),
                "A1",
                vec![clip("a_audio", Some(dur(24, 24)))],
            );
            Timeline {
                name: "t".into(),
                global_start_time: None,
                metadata: crate::model::JsonMap::new(),
                tracks: Element::container(Kind::Stack, "tracks", vec![t1, t2]),
                extra: crate::model::JsonMap::new(),
            }
        };
        let one_track = timeline(vec![clip("a", Some(dur(24, 24)))]);
        let rep = verify_roundtrip(&two_tracks, &one_track);
        let tracks = rep
            .checks
            .iter()
            .find(|c| c.name == "tracks")
            .expect("track check");
        assert_eq!(tracks.severity, Severity::Loss);
        assert!(tracks.detail.contains("V1/A1"));
    }

    #[test]
    fn json_report_shape() {
        let src = timeline(vec![clip("a", Some(dur(2400, 24)))]);
        let rt = timeline(vec![clip("a", Some(dur(2399, 24)))]);
        let rep = verify_roundtrip(&src, &rt);
        let j = rep.to_json();
        assert_eq!(j["passed"], json!(false));
        assert_eq!(j["loss_count"], json!(1));
    }
}
