//! OTIO JSON reader — lenient parse into the exact model.
//!
//! Accepts both `Clip.2` (media_references map, python-otio ≥0.18 canonical)
//! and legacy `Clip.1` (single media_reference) input; canonical output is
//! always Clip.2 (canon.rs). Unknown schemas are captured verbatim as
//! `Kind::Unknown` / raw extras — never dropped.
//!
//! Identity stamping happens HERE on load when absent (the capture substrate,
//! ADR §1.2): every parsed element and marker gets `metadata.cairn.uuid`, so
//! side A and side B see stable identity even for hand-authored files that
//! never went through `tl-capture`.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::model::{
    Effect, Element, JsonMap, Kind, Marker, MediaKind, MediaRef, MediaRefEntry, TimeRange, TimeVal,
    Timeline, TrackKind, TransitionInfo,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    BadJson(String),
    UnexpectedSchema { found: String, at: String },
    MissingField { field: &'static str, at: String },
    BadTime { at: String, reason: String },
    NotAnObject { at: String },
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::BadJson(e) => write!(f, "invalid JSON: {e}"),
            ParseError::UnexpectedSchema { found, at } => {
                write!(f, "unsupported OTIO schema `{found}` at {at}")
            }
            ParseError::MissingField { field, at } => write!(f, "missing field `{field}` at {at}"),
            ParseError::BadTime { at, reason } => write!(f, "bad time value at {at}: {reason}"),
            ParseError::NotAnObject { at } => write!(f, "expected an object at {at}"),
        }
    }
}

impl std::error::Error for ParseError {}

/// Parse an OTIO JSON document (Timeline.1, or a bare Stack.1 wrapped as one).
pub fn parse_otio(input: &str) -> Result<Timeline, ParseError> {
    let v: Value = serde_json::from_str(input).map_err(|e| ParseError::BadJson(e.to_string()))?;
    parse_timeline_value(&v)
}

fn parse_timeline_value(v: &Value) -> Result<Timeline, ParseError> {
    let obj = v.as_object().ok_or_else(|| ParseError::NotAnObject {
        at: "timeline root".into(),
    })?;
    let tag = obj.get("OTIO_SCHEMA").and_then(Value::as_str).unwrap_or("");
    match tag {
        "Timeline.1" => {
            let tracks = obj.get("tracks").ok_or_else(|| ParseError::MissingField {
                field: "tracks",
                at: "Timeline".into(),
            })?;
            Ok(Timeline {
                name: str_field(obj, "name", "Timeline"),
                global_start_time: opt_time(obj.get("global_start_time"), "global_start_time")?,
                metadata: map_field(obj, "metadata")?,
                tracks: parse_element_value(tracks, "Timeline.tracks")?,
                extra: extra_of(
                    obj,
                    &[
                        "OTIO_SCHEMA",
                        "metadata",
                        "name",
                        "global_start_time",
                        "tracks",
                    ],
                ),
            })
        }
        "Stack.1" => Ok(Timeline {
            name: "timeline".into(),
            global_start_time: None,
            metadata: JsonMap::new(),
            tracks: parse_element_value(v, "root")?,
            extra: JsonMap::new(),
        }),
        found => Err(ParseError::UnexpectedSchema {
            found: found.into(),
            at: "timeline root".into(),
        }),
    }
}

fn parse_element_value(v: &Value, at: &str) -> Result<Element, ParseError> {
    let obj = v
        .as_object()
        .ok_or_else(|| ParseError::NotAnObject { at: at.to_string() })?;
    let tag = obj.get("OTIO_SCHEMA").and_then(Value::as_str).unwrap_or("");
    let known = [
        "OTIO_SCHEMA",
        "metadata",
        "name",
        "source_range",
        "effects",
        "markers",
        "enabled",
        "color",
        "children",
        "kind",
        "media_references",
        "active_media_reference_key",
        "media_reference",
        "transition_type",
        "in_offset",
        "out_offset",
    ];
    let extra = extra_of(obj, &known);

    match tag {
        "Stack.1" => {
            let children = children_of(obj, at)?;
            Ok(Element {
                kind: Kind::Stack,
                ..common_fields(obj, tag, at, extra, children)?
            })
        }
        "Track.1" => {
            let kind = TrackKind::parse(obj.get("kind").and_then(Value::as_str).unwrap_or("Video"));
            let children = children_of(obj, at)?;
            Ok(Element {
                kind: Kind::Track(kind),
                ..common_fields(obj, tag, at, extra, children)?
            })
        }
        "Clip.2" | "Clip.1" => {
            let media = parse_media(obj, at, tag)?;
            Ok(Element {
                kind: Kind::Clip,
                media,
                ..common_fields(obj, tag, at, extra, Vec::new())?
            })
        }
        "Gap.1" => Ok(Element {
            kind: Kind::Gap,
            ..common_fields(obj, tag, at, extra, Vec::new())?
        }),
        "Transition.1" => {
            let transition = TransitionInfo {
                transition_type: obj
                    .get("transition_type")
                    .and_then(Value::as_str)
                    .unwrap_or("SMPTE_Dissolve")
                    .to_string(),
                in_offset: opt_time(obj.get("in_offset"), "in_offset")?,
                out_offset: opt_time(obj.get("out_offset"), "out_offset")?,
            };
            Ok(Element {
                kind: Kind::Transition,
                transition: Some(transition),
                ..common_fields(obj, tag, at, extra, Vec::new())?
            })
        }
        _ => Ok(Element {
            kind: Kind::Unknown(tag.to_string()),
            ..common_fields(obj, tag, at, extra, Vec::new())?
        }),
    }
}

fn common_fields(
    obj: &serde_json::Map<String, Value>,
    tag: &str,
    at: &str,
    extra: JsonMap,
    children: Vec<Element>,
) -> Result<Element, ParseError> {
    let (markers, marker_raw) = parse_markers(obj, at)?;
    let effects = parse_effects(obj, at)?;
    Ok(Element {
        kind: Kind::Unknown(tag.to_string()),
        name: str_field(obj, "name", ""),
        source_range: opt_range(obj.get("source_range"), "source_range", at)?,
        enabled: obj.get("enabled").and_then(Value::as_bool).unwrap_or(true),
        color: obj.get("color").and_then(Value::as_str).map(str::to_string),
        metadata: map_field(obj, "metadata")?,
        media: None,
        effects,
        markers,
        children,
        transition: None,
        extra: merge_raw(extra, marker_raw),
    })
}

/// Parse the media-reference block: Clip.2 map form or Clip.1 single form.
fn parse_media(
    obj: &serde_json::Map<String, Value>,
    at: &str,
    tag: &str,
) -> Result<Option<MediaRef>, ParseError> {
    if tag == "Clip.2" {
        let Some(Value::Object(refs)) = obj.get("media_references") else {
            return Ok(None); // absent media is legal (missing reference)
        };
        if refs.is_empty() {
            return Ok(None);
        }
        let active_key = obj
            .get("active_media_reference_key")
            .and_then(Value::as_str)
            .unwrap_or("DEFAULT_MEDIA")
            .to_string();
        let mut references = BTreeMap::new();
        let mut target_url = None;
        let mut available = None;
        for (k, rv) in refs {
            let entry = parse_media_entry(rv, at)?;
            if *k == active_key {
                target_url = entry.target_url.clone();
                available = entry.available_range.clone();
            }
            references.insert(k.clone(), entry);
        }
        return Ok(Some(MediaRef {
            kind: MediaKind::External,
            name: String::new(),
            target_url,
            available_range: available,
            active_key,
            references,
            extra: JsonMap::new(),
        }));
    }
    // Clip.1 legacy single form
    match obj.get("media_reference") {
        None | Some(Value::Null) => Ok(None),
        Some(rv) => {
            let entry = parse_media_entry(rv, at)?;
            let mut references = BTreeMap::new();
            let kind = entry.kind.clone();
            let url = entry.target_url.clone();
            let avail = entry.available_range.clone();
            references.insert("DEFAULT_MEDIA".to_string(), entry);
            Ok(Some(MediaRef {
                kind,
                name: String::new(),
                target_url: url,
                available_range: avail,
                active_key: "DEFAULT_MEDIA".into(),
                references,
                extra: JsonMap::new(),
            }))
        }
    }
}

fn parse_media_entry(v: &Value, at: &str) -> Result<MediaRefEntry, ParseError> {
    let obj = v
        .as_object()
        .ok_or_else(|| ParseError::NotAnObject { at: at.to_string() })?;
    let tag = obj.get("OTIO_SCHEMA").and_then(Value::as_str).unwrap_or("");
    let known: &[&str] = match tag {
        "ExternalReference.1" | "MissingReference.1" => &[
            "OTIO_SCHEMA",
            "metadata",
            "name",
            "available_range",
            "available_image_bounds",
            "target_url",
        ],
        // unknown media-reference schemas: preserve EVERY field verbatim
        _ => &[],
    };
    let kind = match tag {
        "ExternalReference.1" => MediaKind::External,
        "MissingReference.1" => MediaKind::Missing,
        "GeneratorReference.1" => MediaKind::Generator,
        other => MediaKind::Unknown(other.to_string()),
    };
    Ok(MediaRefEntry {
        schema: tag.to_string(),
        kind,
        name: str_field(obj, "name", ""),
        target_url: obj
            .get("target_url")
            .and_then(Value::as_str)
            .map(str::to_string),
        available_range: opt_range(obj.get("available_range"), "available_range", at)?,
        metadata: map_field(obj, "metadata")?,
        extra: extra_of(obj, known),
    })
}

fn parse_markers(
    obj: &serde_json::Map<String, Value>,
    at: &str,
) -> Result<(Vec<Marker>, JsonMap), ParseError> {
    let mut out = Vec::new();
    if let Some(Value::Array(items)) = obj.get("markers") {
        for (i, mv) in items.iter().enumerate() {
            let mobj = mv.as_object().ok_or_else(|| ParseError::NotAnObject {
                at: format!("{at}.markers[{i}]"),
            })?;
            let tag = mobj
                .get("OTIO_SCHEMA")
                .and_then(Value::as_str)
                .unwrap_or("");
            let marked_range = mobj
                .get("marked_range")
                .map(|r| parse_range(r, "marked_range", at))
                .transpose()?
                .unwrap_or(TimeRange {
                    start: zero_time(),
                    duration: zero_time(),
                });
            let m_known: &[&str] = if matches!(tag, "Marker.1" | "Marker.2") {
                &[
                    "OTIO_SCHEMA",
                    "metadata",
                    "name",
                    "color",
                    "marked_range",
                    "comment",
                ]
            } else {
                &[] // unknown marker schemas: verbatim
            };
            out.push(Marker {
                schema: tag.to_string(),
                name: str_field(mobj, "name", ""),
                color: mobj
                    .get("color")
                    .and_then(Value::as_str)
                    .unwrap_or("RED")
                    .to_string(),
                comment: mobj
                    .get("comment")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                marked_range,
                metadata: map_field(mobj, "metadata")?,
                extra: extra_of(mobj, m_known),
            });
        }
    }
    Ok((out, JsonMap::new()))
}

fn parse_effects(
    obj: &serde_json::Map<String, Value>,
    at: &str,
) -> Result<Vec<Effect>, ParseError> {
    let mut out = Vec::new();
    if let Some(Value::Array(items)) = obj.get("effects") {
        for (i, ev) in items.iter().enumerate() {
            let eobj = ev.as_object().ok_or_else(|| ParseError::NotAnObject {
                at: format!("{at}.effects[{i}]"),
            })?;
            let tag = eobj
                .get("OTIO_SCHEMA")
                .and_then(Value::as_str)
                .unwrap_or("");
            let e_known: &[&str] = if tag == "Effect.1" {
                &["OTIO_SCHEMA", "metadata", "name", "effect_name", "enabled"]
            } else {
                &[] // unknown effect schemas: verbatim
            };
            out.push(Effect {
                schema: tag.to_string(),
                name: str_field(eobj, "name", ""),
                effect_name: str_field(eobj, "effect_name", ""),
                enabled: eobj.get("enabled").and_then(Value::as_bool).unwrap_or(true),
                metadata: map_field(eobj, "metadata")?,
                extra: extra_of(eobj, e_known),
            });
        }
    }
    Ok(out)
}

fn children_of(obj: &serde_json::Map<String, Value>, at: &str) -> Result<Vec<Element>, ParseError> {
    match obj.get("children") {
        Some(Value::Array(items)) => items
            .iter()
            .enumerate()
            .map(|(i, cv)| parse_element_value(cv, &format!("{at}.children[{i}]")))
            .collect(),
        _ => Ok(Vec::new()),
    }
}

fn parse_range(v: &Value, field: &str, at: &str) -> Result<TimeRange, ParseError> {
    let obj = v.as_object().ok_or_else(|| ParseError::NotAnObject {
        at: format!("{at}.{field}"),
    })?;
    Ok(TimeRange {
        start: parse_time_val(obj.get("start_time"), field, at)?,
        duration: parse_time_val(obj.get("duration"), field, at)?,
    })
}

fn opt_range(v: Option<&Value>, field: &str, at: &str) -> Result<Option<TimeRange>, ParseError> {
    match v {
        None | Some(Value::Null) => Ok(None),
        Some(r) => Ok(Some(parse_range(r, field, at)?)),
    }
}

fn parse_time_val(v: Option<&Value>, field: &str, at: &str) -> Result<TimeVal, ParseError> {
    let at = format!("{at}.{field}");
    let obj = v
        .and_then(Value::as_object)
        .ok_or_else(|| ParseError::MissingField {
            field: "RationalTime",
            at: at.clone(),
        })?;
    let value = num_as_rational(obj.get("value"), "value", &at)?;
    let rate = num_as_rational(obj.get("rate"), "rate", &at)?;
    if rate.is_zero() {
        return Err(ParseError::BadTime {
            at,
            reason: "rate is zero".into(),
        });
    }
    Ok(TimeVal { value, rate })
}

fn opt_time(v: Option<&Value>, field: &str) -> Result<Option<TimeVal>, ParseError> {
    match v {
        None | Some(Value::Null) => Ok(None),
        Some(tv) => Ok(Some(parse_time_val(Some(tv), field, field)?)),
    }
}

fn num_as_rational(
    v: Option<&Value>,
    field: &str,
    at: &str,
) -> Result<crate::rational::Rational, ParseError> {
    let n = v.ok_or_else(|| ParseError::MissingField {
        field: "value",
        at: at.to_string(),
    })?;
    let f = n.as_f64().ok_or_else(|| ParseError::BadTime {
        at: at.to_string(),
        reason: format!("{field} is not a number"),
    })?;
    if !f.is_finite() {
        return Err(ParseError::BadTime {
            at: at.to_string(),
            reason: format!("{field} is not finite"),
        });
    }
    crate::rational::f64_to_rational(f).map_err(|e| ParseError::BadTime {
        at: at.to_string(),
        reason: format!("{field}: {e:?}"),
    })
}

fn zero_time() -> TimeVal {
    TimeVal {
        value: crate::rational::Rational::ZERO,
        rate: crate::rational::Rational::new(1, 1).expect("1/1 is valid"),
    }
}

fn str_field(obj: &serde_json::Map<String, Value>, field: &str, default: &str) -> String {
    obj.get(field)
        .and_then(Value::as_str)
        .unwrap_or(default)
        .to_string()
}

fn map_field(obj: &serde_json::Map<String, Value>, field: &str) -> Result<JsonMap, ParseError> {
    match obj.get(field) {
        None | Some(Value::Null) => Ok(JsonMap::new()),
        Some(Value::Object(m)) => {
            let mut out = JsonMap::new();
            for (k, v) in m {
                out.insert(k.clone(), v.clone());
            }
            Ok(out)
        }
        Some(_) => Err(ParseError::NotAnObject {
            at: format!("field {field}"),
        }),
    }
}

/// Collect every field NOT in the known set (verbatim preservation).
fn extra_of(obj: &serde_json::Map<String, Value>, known: &[&str]) -> JsonMap {
    let mut out = JsonMap::new();
    for (k, v) in obj {
        if !known.contains(&k.as_str()) {
            out.insert(k.clone(), v.clone());
        }
    }
    out
}

fn merge_raw(mut base: JsonMap, extra: JsonMap) -> JsonMap {
    for (k, v) in extra {
        base.insert(k, v);
    }
    base
}

/// Rebuild a TimeRange from a canonical value (ATTR payloads).
pub fn range_from_value(v: &Value) -> Option<TimeRange> {
    let obj = v.as_object()?;
    let start = parse_time_val(obj.get("start_time"), "start_time", "range").ok()?;
    let duration = parse_time_val(obj.get("duration"), "duration", "range").ok()?;
    Some(TimeRange { start, duration })
}

/// Rebuild an effects list from a canonical value (ATTR payloads).
pub fn effects_from_value(v: &Value) -> Option<Vec<Effect>> {
    let arr = v.as_array()?;
    let mut out = Vec::new();
    for ev in arr {
        let eobj = ev.as_object()?;
        let tag = eobj
            .get("OTIO_SCHEMA")
            .and_then(Value::as_str)
            .unwrap_or("");
        let known: &[&str] = if tag == "Effect.1" {
            &["OTIO_SCHEMA", "metadata", "name", "effect_name", "enabled"]
        } else {
            &[]
        };
        out.push(Effect {
            schema: tag.to_string(),
            name: str_field(eobj, "name", ""),
            effect_name: str_field(eobj, "effect_name", ""),
            enabled: eobj.get("enabled").and_then(Value::as_bool).unwrap_or(true),
            metadata: map_field(eobj, "metadata").ok()?,
            extra: extra_of(eobj, known),
        });
    }
    Some(out)
}

/// Rebuild a metadata/extra map from a canonical value (ATTR payloads).
pub fn map_from_value(v: &Value) -> Option<JsonMap> {
    let obj = v.as_object()?;
    let mut out = JsonMap::new();
    for (k, val) in obj {
        out.insert(k.clone(), val.clone());
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::stamp_all;

    const DOC: &str = r#"{
      "OTIO_SCHEMA": "Timeline.1",
      "metadata": {},
      "name": "Demo",
      "global_start_time": null,
      "tracks": {
        "OTIO_SCHEMA": "Stack.1",
        "metadata": {},
        "name": "tracks",
        "source_range": null,
        "effects": [],
        "markers": [],
        "enabled": true,
        "color": null,
        "children": [
          {
            "OTIO_SCHEMA": "Track.1",
            "metadata": {},
            "name": "V1",
            "source_range": null,
            "effects": [],
            "markers": [],
            "enabled": true,
            "color": null,
            "children": [
              {
                "OTIO_SCHEMA": "Clip.2",
                "metadata": {},
                "name": "A",
                "source_range": null,
                "effects": [],
                "markers": [],
                "enabled": true,
                "color": null,
                "media_references": {
                  "DEFAULT_MEDIA": {
                    "OTIO_SCHEMA": "ExternalReference.1",
                    "metadata": {},
                    "name": "",
                    "available_range": null,
                    "available_image_bounds": null,
                    "target_url": "file:///a.mov"
                  }
                },
                "active_media_reference_key": "DEFAULT_MEDIA"
              },
              {
                "OTIO_SCHEMA": "Gap.1",
                "metadata": {},
                "name": "",
                "source_range": {
                  "OTIO_SCHEMA": "TimeRange.1",
                  "duration": {"OTIO_SCHEMA": "RationalTime.1", "rate": 24.0, "value": 24.0},
                  "start_time": {"OTIO_SCHEMA": "RationalTime.1", "rate": 24.0, "value": 0.0}
                },
                "effects": [],
                "markers": [],
                "enabled": true,
                "color": null
              }
            ],
            "kind": "Video"
          }
        ]
      }
    }"#;

    #[test]
    fn parses_canonical_doc() {
        let tl = parse_otio(DOC).unwrap();
        assert_eq!(tl.name, "Demo");
        let names: Vec<&str> = tl.walk().iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["tracks", "V1", "A", ""]);
        let stack = &tl.tracks;
        let track = &stack.children[0];
        assert!(matches!(track.kind, Kind::Track(TrackKind::Video)));
        let clip = &track.children[0];
        assert!(matches!(clip.kind, Kind::Clip));
        assert_eq!(clip.active_media_url().as_deref(), Some("file:///a.mov"));
        let gap = &track.children[1];
        let sr = gap.source_range.as_ref().unwrap();
        assert_eq!(sr.duration.value.num, 24);
        assert_eq!(sr.duration.rate.num, 24);
    }

    #[test]
    fn parses_legacy_clip1() {
        let legacy = r#"{
          "OTIO_SCHEMA": "Timeline.1", "metadata": {}, "name": "L", "global_start_time": null,
          "tracks": {
            "OTIO_SCHEMA": "Stack.1", "metadata": {}, "name": "tracks", "source_range": null,
            "effects": [], "markers": [], "enabled": true, "color": null,
            "children": [{
              "OTIO_SCHEMA": "Track.1", "metadata": {}, "name": "V1", "source_range": null,
              "effects": [], "markers": [], "enabled": true, "color": null, "kind": "Video",
              "children": [{
                "OTIO_SCHEMA": "Clip.1", "metadata": {}, "name": "X", "source_range": null,
                "effects": [], "markers": [], "enabled": true, "color": null,
                "media_reference": {
                  "OTIO_SCHEMA": "MissingReference.1", "metadata": {}, "name": "",
                  "available_range": null, "available_image_bounds": null
                }
              }]
            }]
          }
        }"#;
        let tl = parse_otio(legacy).unwrap();
        let clip = &tl.tracks.children[0].children[0];
        let media = clip.media.as_ref().unwrap();
        assert_eq!(media.references.len(), 1);
        assert!(matches!(
            media.references["DEFAULT_MEDIA"].kind,
            MediaKind::Missing
        ));
    }

    #[test]
    fn unknown_schema_preserved_not_dropped() {
        let doc = DOC.replace("\"Clip.2\"", "\"Clip.CUSTOM\"");
        // also strip Clip.2-only fields so the object is fully unknown-ish
        let tl = parse_otio(&doc).unwrap();
        let clip = &tl.tracks.children[0].children[0];
        assert!(matches!(&clip.kind, Kind::Unknown(t) if t == "Clip.CUSTOM"));
    }

    #[test]
    fn bad_schema_refused() {
        assert!(parse_otio("{\"OTIO_SCHEMA\": \"Nope.9\"}").is_err());
        assert!(parse_otio("not json").is_err());
        // zero rate refused (C10 honesty)
        let zero_rate = DOC.replace("\"rate\": 24.0", "\"rate\": 0.0");
        assert!(parse_otio(&zero_rate).is_err());
    }

    #[test]
    fn parse_stamps_identity_on_load() {
        // parse_otio does NOT stamp (stamp_all is explicit); stamping is the
        // capture adapter's job. Assert no phantom uuids appear:
        let tl = parse_otio(DOC).unwrap();
        assert!(tl.walk().iter().all(|e| e.cairn_uuid().is_none()));
        // and stamp_all fills every element
        let mut tl2 = tl.clone();
        stamp_all(&mut tl2);
        assert!(tl2.walk().iter().all(|e| e.cairn_uuid().is_some()));
    }
}
