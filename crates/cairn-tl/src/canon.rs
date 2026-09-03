//! Canonical OTIO JSON serializer — byte-deterministic, python-otio 0.18.x
//! shape-compatible (Clip.2 media_references map, value-form RationalTime,
//! Marker.2 comment).
//!
//! Determinism contract (ADR-0015 §2.6): `serialize(parse(x))` is pure; equal
//! documents produce equal bytes. Map keys are emitted sorted (serde_json
//! BTreeMap) — a canonical order, not python's insertion order; python-otio
//! parses any order, and the interop job proves semantic equality both ways.
//! Times are emitted as the correctly-rounded f64 of the exact rational
//! (`num/den` division) — the same double python-otio would hold, computed
//! deterministically; the EXACTNESS lives in the model, the wire is the OTIO
//! schema's own float form.

use serde_json::{json, Map, Value};

use crate::model::{
    Effect, Element, JsonMap, Kind, Marker, MediaKind, MediaRefEntry, TimeRange, TimeVal, Timeline,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CanonError {
    /// A time value does not round to a finite f64 (would serialize as JSON
    /// null — serde_json's non-finite behavior — and silently corrupt the
    /// document; refuse instead, C10 policy).
    NonFiniteTime { at: String },
}

impl std::fmt::Display for CanonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CanonError::NonFiniteTime { at } => {
                write!(
                    f,
                    "time value at {at} does not fit an f64 — refusing to serialize"
                )
            }
        }
    }
}

impl std::error::Error for CanonError {}

/// Serialize to compact canonical JSON (hashing, reports, in-memory compare).
pub fn serialize(timeline: &Timeline) -> Result<String, CanonError> {
    let v = to_value(timeline)?;
    serde_json::to_string(&v).map_err(|_| CanonError::NonFiniteTime { at: "root".into() })
}

/// Serialize to pretty canonical JSON (`.otio` files — git-diffable).
pub fn serialize_pretty(timeline: &Timeline) -> Result<String, CanonError> {
    let v = to_value(timeline)?;
    serde_json::to_string_pretty(&v).map_err(|_| CanonError::NonFiniteTime { at: "root".into() })
}

/// File form: pretty + trailing newline.
pub fn serialize_file(timeline: &Timeline) -> Result<String, CanonError> {
    Ok(format!("{}\n", serialize_pretty(timeline)?))
}

/// The full JSON value tree (deterministic; maps sorted).
pub fn to_value(timeline: &Timeline) -> Result<Value, CanonError> {
    let mut root = Map::new();
    root.insert("OTIO_SCHEMA".into(), json!("Timeline.1"));
    insert_metadata(&mut root, "metadata", &timeline.metadata);
    root.insert("name".into(), json!(timeline.name));
    root.insert(
        "global_start_time".into(),
        match &timeline.global_start_time {
            None => Value::Null,
            Some(t) => time_val_json(t, "global_start_time")?,
        },
    );
    root.insert("tracks".into(), element_json(&timeline.tracks)?);
    merge_extra(&mut root, &timeline.extra);
    Ok(Value::Object(root))
}

fn element_json(e: &Element) -> Result<Value, CanonError> {
    let at = e.name.as_str();
    let mut o = Map::new();
    match &e.kind {
        Kind::Stack | Kind::Track(_) => {
            let tag = if matches!(e.kind, Kind::Stack) {
                "Stack.1"
            } else {
                "Track.1"
            };
            o.insert("OTIO_SCHEMA".into(), json!(tag));
            insert_common(&mut o, e, at)?;
            let children = e
                .children
                .iter()
                .map(element_json)
                .collect::<Result<Vec<_>, _>>()?;
            o.insert("children".into(), Value::Array(children));
            if let Kind::Track(kind) = &e.kind {
                o.insert("kind".into(), json!(kind.as_str()));
            }
        }
        Kind::Clip => {
            o.insert("OTIO_SCHEMA".into(), json!("Clip.2"));
            insert_common(&mut o, e, at)?;
            if let Some(media) = &e.media {
                let mut refs = Map::new();
                for (k, entry) in &media.references {
                    refs.insert(k.clone(), media_entry_json(entry, at)?);
                }
                o.insert("media_references".into(), Value::Object(refs));
                o.insert("active_media_reference_key".into(), json!(media.active_key));
            }
        }
        Kind::Gap => {
            o.insert("OTIO_SCHEMA".into(), json!("Gap.1"));
            insert_common(&mut o, e, at)?;
        }
        Kind::Transition => {
            o.insert("OTIO_SCHEMA".into(), json!("Transition.1"));
            insert_metadata(&mut o, "metadata", &e.metadata);
            o.insert("name".into(), json!(e.name));
            if let Some(t) = &e.transition {
                o.insert("transition_type".into(), json!(t.transition_type));
                o.insert(
                    "in_offset".into(),
                    match &t.in_offset {
                        None => Value::Null,
                        Some(v) => time_val_json(v, "in_offset")?,
                    },
                );
                o.insert(
                    "out_offset".into(),
                    match &t.out_offset {
                        None => Value::Null,
                        Some(v) => time_val_json(v, "out_offset")?,
                    },
                );
            }
            merge_extra(&mut o, &e.extra);
        }
        Kind::Unknown(tag) => {
            // verbatim: original schema tag + every preserved field
            o.insert("OTIO_SCHEMA".into(), json!(tag));
            merge_extra(&mut o, &e.extra);
        }
    }
    Ok(Value::Object(o))
}

/// Common Item/Composition fields in python-otio field order.
fn insert_common(o: &mut Map<String, Value>, e: &Element, at: &str) -> Result<(), CanonError> {
    insert_metadata(o, "metadata", &e.metadata);
    o.insert("name".into(), json!(e.name));
    o.insert(
        "source_range".into(),
        match &e.source_range {
            None => Value::Null,
            Some(r) => range_json(r, at)?,
        },
    );
    let effects = e
        .effects
        .iter()
        .map(|ef| {
            if ef.is_known_schema() {
                let mut m = Map::new();
                m.insert("OTIO_SCHEMA".into(), json!("Effect.1"));
                insert_metadata(&mut m, "metadata", &ef.metadata);
                m.insert("name".into(), json!(ef.name));
                m.insert("effect_name".into(), json!(ef.effect_name));
                m.insert("enabled".into(), json!(ef.enabled));
                merge_extra(&mut m, &ef.extra);
                Ok(Value::Object(m))
            } else {
                Ok(Value::Object(verbatim_map(&ef.extra, &ef.schema_tag())))
            }
        })
        .collect::<Result<Vec<_>, CanonError>>()?;
    o.insert("effects".into(), Value::Array(effects));
    let markers = e
        .markers
        .iter()
        .map(|mk| marker_json(mk, at))
        .collect::<Result<Vec<_>, CanonError>>()?;
    o.insert("markers".into(), Value::Array(markers));
    o.insert("enabled".into(), json!(e.enabled));
    o.insert(
        "color".into(),
        match &e.color {
            None => Value::Null,
            Some(c) => json!(c),
        },
    );
    merge_extra(o, &e.extra);
    Ok(())
}

fn marker_json(mk: &Marker, at: &str) -> Result<Value, CanonError> {
    if mk.is_known_schema() {
        let mut m = Map::new();
        m.insert("OTIO_SCHEMA".into(), json!("Marker.2"));
        insert_metadata(&mut m, "metadata", &mk.metadata);
        m.insert("name".into(), json!(mk.name));
        m.insert("color".into(), json!(mk.color));
        m.insert("marked_range".into(), range_json(&mk.marked_range, at)?);
        m.insert("comment".into(), json!(mk.comment));
        merge_extra(&mut m, &mk.extra);
        Ok(Value::Object(m))
    } else {
        Ok(Value::Object(verbatim_map(&mk.extra, &mk.schema_tag())))
    }
}

fn media_entry_json(entry: &MediaRefEntry, at: &str) -> Result<Value, CanonError> {
    if entry.is_known_schema() {
        let mut m = Map::new();
        let tag = match entry.kind {
            MediaKind::External => "ExternalReference.1",
            MediaKind::Missing => "MissingReference.1",
            MediaKind::Generator | MediaKind::Unknown(_) => {
                unreachable!("known-schema media entries are External/Missing only by construction")
            }
        };
        m.insert("OTIO_SCHEMA".into(), json!(tag));
        insert_metadata(&mut m, "metadata", &entry.metadata);
        m.insert("name".into(), json!(entry.name));
        m.insert(
            "available_range".into(),
            match &entry.available_range {
                None => Value::Null,
                Some(r) => range_json(r, at)?,
            },
        );
        m.insert("available_image_bounds".into(), Value::Null);
        if let Some(url) = &entry.target_url {
            m.insert("target_url".into(), json!(url));
        }
        merge_extra(&mut m, &entry.extra);
        Ok(Value::Object(m))
    } else {
        Ok(Value::Object(verbatim_map(
            &entry.extra,
            &entry.schema_tag(),
        )))
    }
}

fn range_json(r: &TimeRange, at: &str) -> Result<Value, CanonError> {
    let mut m = Map::new();
    m.insert("OTIO_SCHEMA".into(), json!("TimeRange.1"));
    m.insert("duration".into(), time_val_json(&r.duration, at)?);
    m.insert("start_time".into(), time_val_json(&r.start, at)?);
    Ok(Value::Object(m))
}

fn time_val_json(t: &TimeVal, at: &str) -> Result<Value, CanonError> {
    let rate = t.rate.to_f64_approx();
    let value = t.value.to_f64_approx();
    if !rate.is_finite() || !value.is_finite() {
        return Err(CanonError::NonFiniteTime { at: at.into() });
    }
    let mut m = Map::new();
    m.insert("OTIO_SCHEMA".into(), json!("RationalTime.1"));
    m.insert("rate".into(), json!(rate));
    m.insert("value".into(), json!(value));
    Ok(Value::Object(m))
}

fn insert_metadata(o: &mut Map<String, Value>, key: &str, meta: &JsonMap) {
    let mut m = Map::new();
    for (k, v) in meta {
        m.insert(k.clone(), v.clone());
    }
    o.insert(key.into(), Value::Object(m));
}

fn merge_extra(o: &mut Map<String, Value>, extra: &JsonMap) {
    for (k, v) in extra {
        o.insert(k.clone(), v.clone());
    }
}

/// Unknown-schema objects re-emit every preserved field verbatim.
fn verbatim_map(extra: &JsonMap, tag: &str) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("OTIO_SCHEMA".into(), json!(tag));
    for (k, v) in extra {
        m.insert(k.clone(), v.clone());
    }
    m
}

/// Emit a TimeRange as its canonical JSON value (ATTR payloads).
pub fn range_value(r: &TimeRange) -> Value {
    range_json(r, "range").unwrap_or(Value::Null)
}

/// Emit an effects list as canonical JSON (ATTR payloads).
pub fn effects_value(effects: &[Effect]) -> Value {
    Value::Array(
        effects
            .iter()
            .map(|ef| {
                if ef.is_known_schema() {
                    let mut m = Map::new();
                    m.insert("OTIO_SCHEMA".into(), json!("Effect.1"));
                    insert_metadata(&mut m, "metadata", &ef.metadata);
                    m.insert("name".into(), json!(ef.name));
                    m.insert("effect_name".into(), json!(ef.effect_name));
                    m.insert("enabled".into(), json!(ef.enabled));
                    merge_extra(&mut m, &ef.extra);
                    Value::Object(m)
                } else {
                    Value::Object(verbatim_map(&ef.extra, &ef.schema_tag()))
                }
            })
            .collect(),
    )
}

/// Emit a metadata/extra map as canonical JSON (ATTR payloads).
pub fn map_value(map: &JsonMap) -> Value {
    let mut m = Map::new();
    for (k, v) in map {
        m.insert(k.clone(), v.clone());
    }
    Value::Object(m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse_otio;

    const DOC: &str = include_str!("../fixtures/roundtrip_base.otio");

    #[test]
    fn parse_build_is_stable() {
        let tl = parse_otio(DOC).unwrap();
        let a = serialize(&tl).unwrap();
        let tl2 = parse_otio(&a).unwrap();
        let b = serialize(&tl2).unwrap();
        assert_eq!(
            a, b,
            "serialize ∘ parse must be a fixpoint on canonical output"
        );
    }

    #[test]
    fn determinism_same_input_same_bytes() {
        let tl = parse_otio(DOC).unwrap();
        assert_eq!(serialize(&tl).unwrap(), serialize(&tl).unwrap());
        assert_eq!(serialize(&tl.clone()).unwrap(), serialize(&tl).unwrap());
    }

    #[test]
    fn emits_python_shapes() {
        let tl = parse_otio(DOC).unwrap();
        let v = to_value(&tl).unwrap();
        let track = &v["tracks"]["children"][0];
        assert_eq!(track["OTIO_SCHEMA"], json!("Track.1"));
        assert_eq!(track["kind"], json!("Video"));
        let clip = &track["children"][0];
        assert_eq!(clip["OTIO_SCHEMA"], json!("Clip.2"));
        assert_eq!(
            clip["media_references"]["DEFAULT_MEDIA"]["OTIO_SCHEMA"],
            json!("ExternalReference.1")
        );
        assert_eq!(
            clip["media_references"]["DEFAULT_MEDIA"]["target_url"],
            json!("file:///media/sunrise_a.mov")
        );
        assert_eq!(clip["active_media_reference_key"], json!("DEFAULT_MEDIA"));
        let gap = &track["children"][1];
        assert_eq!(gap["OTIO_SCHEMA"], json!("Gap.1"));
        assert!(gap.get("source_range").is_some());
        assert!(gap.get("effects").is_some());
        assert!(gap.get("markers").is_some());
        assert_eq!(gap["enabled"], json!(true));
        assert!(gap.get("color").is_some());
    }

    #[test]
    fn unknown_schema_roundtrips_verbatim() {
        let mut tl = parse_otio(DOC).unwrap();
        // graft an unknown-schema child onto the track
        let mut unknown = Element::leaf(Kind::Unknown("CustomWidget.7".into()), "w");
        unknown.extra.insert(
            "payload".into(),
            serde_json::json!({"deep": [1, 2, {"x": null}], "keep": "me"}),
        );
        tl.tracks.children[0].children.push(unknown);
        let s = serialize(&tl).unwrap();
        let back = parse_otio(&s).unwrap();
        let w = &back.tracks.children[0].children[3];
        assert!(matches!(&w.kind, Kind::Unknown(t) if t == "CustomWidget.7"));
        assert_eq!(
            w.extra.get("payload"),
            Some(&serde_json::json!({"deep": [1, 2, {"x": null}], "keep": "me"}))
        );
        // and the bytes are stable across another round
        assert_eq!(serialize(&back).unwrap(), s);
    }

    #[test]
    fn python_otio_output_parses_to_the_same_model() {
        // roundtrip_python.otio is EMITTED BY python-otio 0.18.1 itself
        // (serialized from the same logical document as roundtrip_base.otio).
        // Our parser must accept it and produce the identical model.
        let ours = parse_otio(DOC).unwrap();
        let theirs = parse_otio(include_str!("../fixtures/roundtrip_python.otio")).unwrap();
        assert_eq!(
            ours, theirs,
            "python-otio output must parse to the same model"
        );
        // ...and our canonical form of it must be stable
        let a = serialize(&ours).unwrap();
        let b = serialize(&theirs).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn marker_roundtrip_keeps_comment_and_uuid() {
        let mut tl = parse_otio(DOC).unwrap();
        let clip = &mut tl.tracks.children[0].children[0];
        clip.markers.push(Marker {
            schema: "Marker.2".into(),
            name: "note".into(),
            color: "BLUE".into(),
            comment: "check audio".into(),
            marked_range: clip.source_range.clone().unwrap(),
            metadata: JsonMap::new(),
            extra: JsonMap::new(),
        });
        crate::model::stamp_marker(clip.markers.last_mut().unwrap(), "m-uuid-9");
        let s = serialize(&tl).unwrap();
        let back = parse_otio(&s).unwrap();
        let mk = &back.tracks.children[0].children[0].markers[0];
        assert_eq!(mk.comment, "check audio");
        assert_eq!(crate::model::marker_uuid(mk).as_deref(), Some("m-uuid-9"));
    }
}
