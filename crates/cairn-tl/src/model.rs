//! OTIO document model (the ADR-0015 §2.1 canonical form).
//!
//! Design constraints, in order:
//! 1. **Determinism**: two parses of the same bytes yield structurally identical
//!    documents; serialization is byte-stable (canon.rs).
//! 2. **Exactness**: every time value is an exact [`Rational`] pair (value, rate)
//!    reconstructed from the wire doubles via the IEEE mantissa — no float
//!    arithmetic anywhere in the model.
//! 3. **Total capture**: fields the model does not understand are preserved in
//!    `extra` and re-emitted verbatim, so their diffs surface as raw ATTR ops
//!    (never silently dropped — the silent-loss rule, I2).
//! 4. **Interop**: the canonical serialization is byte-shape-compatible with
//!    python-otio 0.18.x (`Clip.2` media_references map, value-form
//!    RationalTime, `Marker.2` comment) — proven by the CI interop job and the
//!    local `scripts/tl_interop_check.py` oracle.

use std::collections::BTreeMap;

use crate::rational::Rational;

/// A JSON value (metadata, extra fields). `BTreeMap` keeps key order sorted —
/// deterministic serialization for free.
pub type JsonMap = BTreeMap<String, serde_json::Value>;

/// (value, rate) pair, exactly as OTIO `RationalTime` means it:
/// seconds = value / rate, both stored exactly (reconstructed from the wire
/// doubles). `rate` is kept (not collapsed to seconds) so the wire round-trip
/// is value-exact: emit value/rate as the same doubles we parsed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TimeVal {
    pub value: Rational,
    pub rate: Rational,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TimeError {
    /// Zero or non-finite rate.
    BadRate,
    /// Rational bound exceeded (C10 honesty policy).
    OutOfRange,
}

impl TimeVal {
    /// Exact seconds (value / rate).
    pub fn seconds(&self) -> Result<Rational, TimeError> {
        self.value.checked_div(self.rate).map_err(|e| match e {
            crate::rational::RationalError::ZeroDen
            | crate::rational::RationalError::OutOfRange => TimeError::OutOfRange,
            crate::rational::RationalError::NotLosslessF64 => TimeError::OutOfRange,
        })
    }

    /// Build from exact seconds over a rate: value = seconds · rate.
    pub fn from_seconds(seconds: Rational, rate: Rational) -> Result<TimeVal, TimeError> {
        if rate.is_zero() {
            return Err(TimeError::BadRate);
        }
        let value = seconds
            .checked_mul(rate)
            .map_err(|_| TimeError::OutOfRange)?;
        Ok(TimeVal { value, rate })
    }

    /// Zero time at a given rate.
    pub fn zero(rate: Rational) -> Result<TimeVal, TimeError> {
        if rate.is_zero() {
            return Err(TimeError::BadRate);
        }
        Ok(TimeVal {
            value: Rational::ZERO,
            rate,
        })
    }
}

/// OTIO `TimeRange` — start_time + duration, both exact.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TimeRange {
    pub start: TimeVal,
    pub duration: TimeVal,
}

impl TimeRange {
    /// Exact end time (start + duration), preserving the start's rate.
    pub fn end(&self) -> Result<TimeVal, TimeError> {
        let s = self.start.seconds()?;
        let d = self.duration.seconds()?;
        let e = s.checked_add(d).map_err(|_| TimeError::OutOfRange)?;
        TimeVal::from_seconds(e, self.start.rate)
    }
}

/// Element kind — the closed set the merge reasons about. `Unknown` carries the
/// original `OTIO_SCHEMA` tag so round-trips are byte-faithful and diffs on
/// unsupported objects surface as raw ATTR ops instead of being dropped.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    Stack,
    Track(TrackKind),
    Clip,
    Gap,
    Transition,
    /// Unrecognized schema object, preserved verbatim in `extra`.
    Unknown(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TrackKind {
    Video,
    Audio,
    Subtitle,
}

impl TrackKind {
    pub fn as_str(self) -> &'static str {
        match self {
            TrackKind::Video => "Video",
            TrackKind::Audio => "Audio",
            TrackKind::Subtitle => "Subtitle",
        }
    }
    pub fn parse(s: &str) -> TrackKind {
        match s {
            "Audio" => TrackKind::Audio,
            "Subtitle" => TrackKind::Subtitle,
            _ => TrackKind::Video,
        }
    }
}

/// Media reference kinds (the subset with merge-relevant identity).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum MediaKind {
    External,
    Missing,
    Generator,
    Unknown(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaRef {
    pub kind: MediaKind,
    pub name: String,
    pub target_url: Option<String>,
    pub available_range: Option<TimeRange>,
    /// Active media-reference key (Clip.2 `active_media_reference_key`).
    pub active_key: String,
    /// All references when the Clip.2 map form carries more than DEFAULT_MEDIA.
    pub references: BTreeMap<String, MediaRefEntry>,
    /// Unpreserved fields, verbatim.
    pub extra: JsonMap,
}

/// One entry of the Clip.2 `media_references` map.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaRefEntry {
    pub schema: String,
    pub kind: MediaKind,
    pub name: String,
    pub target_url: Option<String>,
    pub available_range: Option<TimeRange>,
    pub metadata: JsonMap,
    pub extra: JsonMap,
}

impl MediaRefEntry {
    pub fn is_known_schema(&self) -> bool {
        matches!(
            self.schema.as_str(),
            "ExternalReference.1" | "MissingReference.1"
        )
    }
    pub fn schema_tag(&self) -> String {
        self.schema.clone()
    }
}

/// OTIO `Marker` — `name`, `color`, `comment`, `marked_range`; stamped with
/// `metadata.cairn.uuid` for C1 union semantics. `schema` records the source
/// tag; unknown marker schemas are preserved verbatim in `extra`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Marker {
    pub schema: String,
    pub name: String,
    pub color: String,
    pub comment: String,
    pub marked_range: TimeRange,
    pub metadata: JsonMap,
    pub extra: JsonMap,
}

impl Marker {
    /// Known schema → structured emit; otherwise verbatim from `extra`.
    pub fn is_known_schema(&self) -> bool {
        matches!(self.schema.as_str(), "Marker.1" | "Marker.2")
    }
    pub fn schema_tag(&self) -> String {
        self.schema.clone()
    }
}

/// OTIO `Effect` (opacity, speed effects, filters) — preserved structurally;
/// parameter changes surface as ATTR ops on the owning element via `extra`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Effect {
    pub schema: String,
    pub name: String,
    pub effect_name: String,
    pub enabled: bool,
    pub metadata: JsonMap,
    pub extra: JsonMap,
}

impl Effect {
    pub fn is_known_schema(&self) -> bool {
        self.schema == "Effect.1"
    }
    pub fn schema_tag(&self) -> String {
        self.schema.clone()
    }
}

/// One timeline element. Containers (`Stack`, `Track`) carry `children`; leaf
/// kinds carry media/effects/markers. `extra` preserves every field the model
/// does not structurally understand (animation, schemadef, custom metadata
/// beyond cairn's own keys is still in `metadata`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Element {
    pub kind: Kind,
    pub name: String,
    /// OTIO `source_range` (None = use media available range / children).
    pub source_range: Option<TimeRange>,
    pub enabled: bool,
    pub color: Option<String>,
    pub metadata: JsonMap,
    /// Clip media (None for Gap/Track/Stack/Transition).
    pub media: Option<MediaRef>,
    pub effects: Vec<Effect>,
    pub markers: Vec<Marker>,
    /// Container children (Stack/Track). Empty for leaves.
    pub children: Vec<Element>,
    /// Transition-specific fields when kind == Transition.
    pub transition: Option<TransitionInfo>,
    /// Unrecognized fields, verbatim (diffs surface as ATTR ops).
    pub extra: JsonMap,
}

/// OTIO `Transition` merge-relevant fields.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransitionInfo {
    pub transition_type: String,
    /// in/out offsets as exact times.
    pub in_offset: Option<TimeVal>,
    pub out_offset: Option<TimeVal>,
}

impl Element {
    /// A leaf element (Clip/Gap/Transition/Unknown) with sensible defaults.
    pub fn leaf(kind: Kind, name: impl Into<String>) -> Element {
        Element {
            kind,
            name: name.into(),
            source_range: None,
            enabled: true,
            color: None,
            metadata: JsonMap::new(),
            media: None,
            effects: Vec::new(),
            markers: Vec::new(),
            children: Vec::new(),
            transition: None,
            extra: JsonMap::new(),
        }
    }

    /// A container element (Stack/Track).
    pub fn container(kind: Kind, name: impl Into<String>, children: Vec<Element>) -> Element {
        let mut e = Element::leaf(kind, name);
        e.children = children;
        e
    }

    /// The `metadata.cairn.uuid` stamp (identity ladder rung (a)), if present.
    pub fn cairn_uuid(&self) -> Option<&str> {
        self.metadata
            .get("cairn")
            .and_then(|v| v.get("uuid"))
            .and_then(serde_json::Value::as_str)
    }

    /// Stamp a `metadata.cairn.uuid` on THIS element only (capture substrate
    /// — idempotent). Deliberately NOT recursive: `stamp_all` walks the tree
    /// and gives every element its OWN identity (a recursive stamp would give
    /// the whole subtree ONE uuid — identity collapse, the silent-loss bug
    /// class this comment exists to prevent).
    pub fn stamp_uuid(&mut self, uuid: &str) {
        let cairn = self
            .metadata
            .entry("cairn".into())
            .or_insert_with(|| serde_json::json!({}));
        if let serde_json::Value::Object(map) = cairn {
            map.entry("uuid".to_string())
                .or_insert_with(|| serde_json::json!(uuid));
        }
    }

    /// True when the element carries merge-relevant CONTENT (media, timing,
    /// or transition parameters). Contentless elements (unnamed, rangeless
    /// gaps) are ladder-rung (d) — position-only identity.
    pub fn has_content(&self) -> bool {
        self.media.is_some() || self.source_range.is_some() || self.transition.is_some()
    }

    /// Content fingerprint for identity ladder rung (c): a stable string over
    /// the merge-relevant content (media identity + in/out + kind), NOT the
    /// mutable presentation attributes (name/opacity/speed are identity-free
    /// by design — renames must match).
    pub fn content_fingerprint(&self) -> String {
        let media = self
            .media
            .as_ref()
            .map(|m| {
                format!(
                    "{}|{}",
                    m.active_target_url().unwrap_or_default(),
                    m.available_range
                        .as_ref()
                        .map(|r| time_key(&r.start, &r.duration))
                        .unwrap_or_default()
                )
            })
            .unwrap_or_default();
        let range = self
            .source_range
            .as_ref()
            .map(|r| time_key(&r.start, &r.duration))
            .unwrap_or_default();
        let tr = self
            .transition
            .as_ref()
            .map(|t| format!("{}|{}", t.transition_type, t.in_offset.is_some()))
            .unwrap_or_default();
        format!("{:?}|{}|{}|{}", self.kind, media, range, tr)
    }

    /// The media-reference URL this element currently points at (rung (c)
    /// identity input). Clip.2: the ACTIVE entry; Clip.1 legacy: the single ref.
    pub fn active_media_url(&self) -> Option<String> {
        self.media.as_ref().and_then(|m| m.active_target_url())
    }

    /// Deep element count (self + descendants).
    pub fn count(&self) -> usize {
        1 + self.children.iter().map(Element::count).sum::<usize>()
    }
}

impl MediaRef {
    /// Legacy single-reference form (Clip.1) — wraps into the map shape.
    pub fn single(kind: MediaKind, name: String, target_url: Option<String>) -> MediaRef {
        let schema = match &kind {
            MediaKind::External => "ExternalReference.1".to_string(),
            MediaKind::Missing => "MissingReference.1".to_string(),
            MediaKind::Generator => "GeneratorReference.1".to_string(),
            MediaKind::Unknown(t) => t.clone(),
        };
        let entry = MediaRefEntry {
            schema,
            kind: kind.clone(),
            name: name.clone(),
            target_url: target_url.clone(),
            available_range: None,
            metadata: JsonMap::new(),
            extra: JsonMap::new(),
        };
        let mut references = BTreeMap::new();
        references.insert("DEFAULT_MEDIA".to_string(), entry);
        MediaRef {
            kind,
            name,
            target_url,
            available_range: None,
            active_key: "DEFAULT_MEDIA".to_string(),
            references,
            extra: JsonMap::new(),
        }
    }

    /// The URL of the ACTIVE reference (what the clip points at NOW).
    pub fn active_target_url(&self) -> Option<String> {
        if let Some(entry) = self.references.get(&self.active_key) {
            return entry.target_url.clone();
        }
        self.target_url.clone()
    }
}

/// Stable key for a time pair (identity inputs must be canonical strings).
fn time_key(start: &TimeVal, duration: &TimeVal) -> String {
    format!(
        "{}/{}|{}/{}",
        start.value.num, start.value.den, duration.value.num, duration.value.den
    )
}

/// The top-level OTIO `Timeline`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Timeline {
    pub name: String,
    pub global_start_time: Option<TimeVal>,
    pub metadata: JsonMap,
    /// The `tracks` Stack.
    pub tracks: Element,
    pub extra: JsonMap,
}

impl Timeline {
    /// Iterate every element in the tree (stack, tracks, items), pre-order.
    pub fn walk(&self) -> Vec<&Element> {
        let mut out = Vec::with_capacity(self.tracks.count());
        walk_ref(&self.tracks, &mut out);
        out
    }

    /// Mutable pre-order walk (callback form — a tree of `&mut` cannot be
    /// collected safely without unsafe, and this crate forbids it).
    pub fn walk_mut<F: FnMut(&mut Element)>(&mut self, mut f: F) {
        walk_cb(&mut self.tracks, &mut f);
    }
}

fn walk_ref<'a>(e: &'a Element, out: &mut Vec<&'a Element>) {
    out.push(e);
    for c in &e.children {
        walk_ref(c, out);
    }
}

fn walk_cb<F: FnMut(&mut Element)>(e: &mut Element, f: &mut F) {
    f(e);
    for c in &mut e.children {
        walk_cb(c, f);
    }
}

/// Stamp every element + marker with a fresh `metadata.cairn.uuid`
/// (the v1 capture substrate: identity survives renames/moves forever after).
pub fn stamp_all(timeline: &mut Timeline) {
    timeline.walk_mut(|e| {
        if e.cairn_uuid().is_none() {
            let id = uuid::Uuid::now_v7().to_string();
            e.stamp_uuid(&id);
        }
        for m in &mut e.markers {
            if marker_uuid(m).is_none() {
                let id = uuid::Uuid::now_v7().to_string();
                stamp_marker(m, &id);
            }
        }
    });
}

/// Marker `metadata.cairn.uuid` accessors (markers have their own identity).
pub fn marker_uuid(m: &Marker) -> Option<String> {
    m.metadata
        .get("cairn")
        .and_then(|v| v.get("uuid"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

pub fn stamp_marker(m: &mut Marker, uuid: &str) {
    let cairn = m
        .metadata
        .entry("cairn".into())
        .or_insert_with(|| serde_json::json!({}));
    if let serde_json::Value::Object(map) = cairn {
        map.entry("uuid".to_string())
            .or_insert_with(|| serde_json::json!(uuid));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tv(value: i128, rate: i128) -> TimeVal {
        TimeVal {
            value: Rational::new(value, 1).unwrap(),
            rate: Rational::new(rate, 1).unwrap(),
        }
    }

    #[test]
    fn time_math_is_exact() {
        let t = tv(10, 24);
        assert_eq!(t.seconds().unwrap(), Rational::new(10, 24).unwrap());
        // 10 frames @24 + 7 frames @24 = 17 frames — via seconds, exact
        let a = tv(10, 24).seconds().unwrap();
        let b = tv(7, 24).seconds().unwrap();
        let sum = a.checked_add(b).unwrap();
        let back = TimeVal::from_seconds(sum, Rational::new(24, 1).unwrap()).unwrap();
        assert_eq!(back.value, Rational::new(17, 1).unwrap());
        assert_eq!(back.value.num, 17);
    }

    #[test]
    fn time_range_end_exact() {
        let r = TimeRange {
            start: tv(10, 24),
            duration: tv(14, 24),
        };
        assert_eq!(r.end().unwrap().value.num, 24);
        // mixed rates stay exact: 1/24 + 1/25
        let r = TimeRange {
            start: tv(1, 24),
            duration: tv(1, 25),
        };
        let e = r.end().unwrap();
        assert_eq!(e.seconds().unwrap(), Rational::new(49, 600).unwrap());
    }

    #[test]
    fn zero_rate_refused() {
        assert!(TimeVal::zero(Rational::ZERO).is_err());
        assert!(TimeVal::from_seconds(Rational::ZERO, Rational::ZERO).is_err());
    }

    #[test]
    fn stamp_uuid_idempotent_and_local() {
        let mut track = Element::container(
            Kind::Track(TrackKind::Video),
            "V1",
            vec![
                Element::leaf(Kind::Clip, "A"),
                Element::leaf(Kind::Clip, "B"),
            ],
        );
        track.stamp_uuid("00000000-0000-7000-8000-000000000001");
        assert_eq!(
            track.cairn_uuid(),
            Some("00000000-0000-7000-8000-000000000001")
        );
        // stamps are LOCAL: children keep their own (empty) identity
        for c in &track.children {
            assert_eq!(c.cairn_uuid(), None);
        }
        // idempotent
        track.stamp_uuid("00000000-0000-7000-8000-000000000002");
        assert_eq!(
            track.cairn_uuid(),
            Some("00000000-0000-7000-8000-000000000001")
        );
    }

    #[test]
    fn content_fingerprint_ignores_presentation() {
        let mut a = Element::leaf(Kind::Clip, "Sunrise");
        a.media = Some(MediaRef::single(
            MediaKind::External,
            String::new(),
            Some("file:///media/sunrise.mov".into()),
        ));
        let mut b = a.clone();
        b.name = "Renamed".into(); // rename does NOT change identity rung (c)
        b.enabled = false; // presentation attribute
        assert_eq!(a.content_fingerprint(), b.content_fingerprint());
        b.source_range = Some(TimeRange {
            start: tv(0, 24),
            duration: tv(24, 24),
        });
        assert_ne!(a.content_fingerprint(), b.content_fingerprint());
    }

    #[test]
    fn timeline_walk_pre_order() {
        let mut tl = Timeline {
            name: "TL".into(),
            global_start_time: None,
            metadata: JsonMap::new(),
            tracks: Element::container(
                Kind::Stack,
                "tracks",
                vec![
                    Element::container(
                        Kind::Track(TrackKind::Video),
                        "V1",
                        vec![
                            Element::leaf(Kind::Clip, "A"),
                            Element::leaf(Kind::Gap, "G"),
                        ],
                    ),
                    Element::container(Kind::Track(TrackKind::Audio), "A1", vec![]),
                ],
            ),
            extra: JsonMap::new(),
        };
        stamp_all(&mut tl);
        let names: Vec<&str> = tl.walk().iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["tracks", "V1", "A", "G", "A1"]);
        assert!(tl.walk().iter().all(|e| e.cairn_uuid().is_some()));
    }

    #[test]
    fn marker_stamps() {
        let mut m = Marker {
            schema: "Marker.2".into(),
            name: "M".into(),
            color: "RED".into(),
            comment: String::new(),
            marked_range: TimeRange {
                start: tv(0, 24),
                duration: tv(0, 24),
            },
            metadata: JsonMap::new(),
            extra: JsonMap::new(),
        };
        stamp_marker(&mut m, "u-1");
        stamp_marker(&mut m, "u-2"); // idempotent
        assert_eq!(marker_uuid(&m).as_deref(), Some("u-1"));
    }

    #[test]
    fn schema_tags_flow() {
        let e = Effect {
            schema: "Effect.1".into(),
            name: "e".into(),
            effect_name: "blur".into(),
            enabled: true,
            metadata: JsonMap::new(),
            extra: JsonMap::new(),
        };
        assert!(e.is_known_schema());
        assert_eq!(e.schema_tag(), "Effect.1");
        let mk = Marker {
            schema: "CustomMarker.3".into(),
            name: "m".into(),
            color: "RED".into(),
            comment: String::new(),
            marked_range: TimeRange {
                start: tv(0, 24),
                duration: tv(0, 24),
            },
            metadata: JsonMap::new(),
            extra: JsonMap::new(),
        };
        assert!(!mk.is_known_schema());
    }
}
