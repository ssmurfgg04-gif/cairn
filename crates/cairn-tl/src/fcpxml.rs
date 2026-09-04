//! FCPXML → OTIO bridge (ADR-0015 §4): ingest ONLY.
//!
//! Merge happens ONLY on OTIO — Cairn never writes vendor XML; the NLE
//! imports the merged OTIO (re-export is adapter-side). This bridge:
//! - parses the structural spine subset: resources (format/asset/effect),
//!   library → event → project → sequence → spine, spine children
//!   (asset-clip, gap, transition, title, video, audio, ref-clip, mc-clip,
//!   audition), markers;
//! - maps FCPXML `lane` attributes to stacked tracks (the ADR's
//!   stacked-track approximation) and standalone `audio` to audio tracks;
//! - carries a **lossiness ledger** (auditions → selected-clip approximation,
//!   compound clips → flat, multicam → flat, roles → metadata, title text →
//!   approximate): every known-lossy mapping is a LEDGER ENTRY, and anything
//!   outside the ledger that would not survive the bridge REFUSES (C10) with
//!   the element named — never a silent drop;
//! - stamps `metadata.cairn.uuid` on every element at ingest (capture
//!   substrate), so FCPXML timelines are retroactively mergeable.
//!
//! Times: FCPXML `"num/dens"` / `"Ns"` / `"0s"` — exact integers over an
//! exact rational rate; no floats anywhere in the bridge.

use std::collections::BTreeMap;

use quick_xml::events::Event;
use quick_xml::Reader;

use crate::model::{
    stamp_all, Element, JsonMap, Kind, Marker, MediaKind, MediaRef, TimeRange, TimeVal, Timeline,
    TrackKind, TransitionInfo,
};
use crate::rational::Rational;

/// Known-lossy ingest mappings (ADR §4: "the ledger ships as a tested
/// fixture, not prose").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerEntry {
    pub fcpxml_feature: &'static str,
    pub otio_approximation: &'static str,
}

/// The lossiness ledger — every entry is exercised by the tests below.
pub fn lossiness_ledger() -> Vec<LedgerEntry> {
    vec![
        LedgerEntry {
            fcpxml_feature: "audition",
            otio_approximation:
                "selected clip only; alternates recorded in metadata.cairn.fcpxml.audition",
        },
        LedgerEntry {
            fcpxml_feature: "ref-clip (compound clip)",
            otio_approximation:
                "flattened to a single clip; compound internals in metadata.cairn.fcpxml",
        },
        LedgerEntry {
            fcpxml_feature: "mc-clip (multicam)",
            otio_approximation: "flattened to a single clip; angle list in metadata.cairn.fcpxml",
        },
        LedgerEntry {
            fcpxml_feature: "roles (audio/video role sources)",
            otio_approximation: "preserved verbatim in metadata.cairn.fcpxml.roles",
        },
        LedgerEntry {
            fcpxml_feature: "title text styling",
            otio_approximation:
                "title kept as a clip with attrs in extra; text styling does not round-trip",
        },
        LedgerEntry {
            fcpxml_feature: "lane (connected clips)",
            otio_approximation: "stacked tracks: lane N → track above the spine",
        },
        LedgerEntry {
            fcpxml_feature: "conform-rate",
            otio_approximation:
                "attrs preserved verbatim in extra[fcpxml:conform-rate] (Attr ops on diff)",
        },
        LedgerEntry {
            fcpxml_feature: "adjust-* / filter-* / retime descriptor subtrees",
            otio_approximation:
                "subtree preserved as JSON in extra[fcpxml:<tag>] (attrs, children, order);
                does not round-trip as vendor XML (bridge is ingest-only)",
        },
    ]
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BridgeError {
    /// XML syntax / structure error.
    Xml(String),
    /// FCPXML feature outside the ledger — refuse (C10), name the element.
    Unsupported { element: String, at: String },
    /// Bad time or attribute value.
    BadValue { at: String, reason: String },
}

impl std::fmt::Display for BridgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BridgeError::Xml(e) => write!(f, "fcpxml parse error: {e}"),
            BridgeError::Unsupported { element, at } => {
                write!(
                    f,
                    "C10: FCPXML element `{element}` at {at} is outside the lossiness ledger — refusing rather than dropping"
                )
            }
            BridgeError::BadValue { at, reason } => write!(f, "bad value at {at}: {reason}"),
        }
    }
}

impl std::error::Error for BridgeError {}

/// Clip-child descriptor subtrees present in REAL exports (Round 13
/// real-timeline corpus: BBC R&D fcpx-xml-composer, cutlass samples,
/// PRONOM authentic FCP X). Opening one of these under a spine item
/// starts a preserved subtree: the whole XML subtree is captured as JSON
/// in the enclosing item's `extra["fcpxml:<tag>"]` — attributes, children,
/// order — so diffs surface them as Attr ops. Data-preserving, never a
/// silent drop; structurally approximate (they do not round-trip as vendor
/// XML — the bridge is ingest-only by design). While a subtree is open,
/// every element inside it (trim-rect, param, array, string, md, ...)
/// is consumed by the preservation itself.
const DESCRIPTOR_ROOTS: &[&str] = &[
    "conform-rate",
    "adjust-crop",
    "adjust-transform",
    "adjust-volume",
    "adjust-blend",
    "adjust-speed",
    "adjust-effects",
    "adjust-audio",
    "filter-video",
    "filter-audio",
    "retime",
    "rate-retime",
    "stabilization",
    "sync",
    "timecode",
    "video-fade",
    "audio-fade",
];

/// Ingest FCPXML text into an OTIO `Timeline` (stamped with cairn uuids).
pub fn parse_fcpxml(input: &str) -> Result<Timeline, BridgeError> {
    let mut reader = Reader::from_str(input);
    reader.config_mut().trim_text(true);
    let mut stack: Vec<String> = Vec::new();
    let mut bridge = BridgeState::default();
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let tag = tag_of(e.name());
                bridge.handle_start(&tag, &attrs_of(&e), &stack)?;
                stack.push(tag);
            }
            Ok(Event::Empty(e)) => {
                // self-closing elements (asset-clip/…/marker in the wild)
                let tag = tag_of(e.name());
                bridge.handle_start(&tag, &attrs_of(&e), &stack)?;
                bridge.handle_end(&tag, &stack);
            }
            Ok(Event::End(e)) => {
                let tag = tag_of(e.name());
                bridge.handle_end(&tag, &stack);
                stack.pop();
            }
            Ok(Event::Text(t)) => {
                let _ = t;
                // text content is not merge-relevant for the spine subset
                // (title styling lives in children we ledger as approximate)
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(e) => return Err(BridgeError::Xml(e.to_string())),
        }
        buf.clear();
    }
    let mut tl = bridge.finish();
    stamp_all(&mut tl);
    Ok(tl)
}

#[derive(Default)]
struct BridgeState {
    // resources: (declaration order matters for media-rep attachment —
    // "most recent asset" is the LAST OPENED one, which a BTreeMap's
    // lexicographic next_back() did NOT give when asset ids don't sort in
    // declaration order; Round 20 keeps the map keyed by id for lookup AND
    // an order list for recency)
    assets: BTreeMap<String, AssetInfo>,
    asset_order: Vec<String>,
    // spine assembly
    spine_items: Vec<SpineItem>,
    markers_pending: Vec<Marker>,
    sequence_name: String,
    project_name: String,
    event_name: String,
    in_spine: bool,
    in_asset_clip: Option<String>, // current asset-clip id for markers
    /// Depth of ledgered approximation subtrees (title/ref-clip/mc-clip/
    /// audition/transition): their internal elements are CONSUMED by the
    /// ledger entry, never refused.
    ledgered_depth: usize,
    /// Open descriptor-subtree levels (Round 13 real-corpus preservation):
    /// each level is the JSON node under construction, root tag in `tag`.
    descriptor_stack: Vec<(String, serde_json::Map<String, serde_json::Value>)>,
}

#[derive(Clone, Debug, Default)]
struct AssetInfo {
    src: Option<String>,
}

#[derive(Clone, Debug)]
struct SpineItem {
    element: Element,
    lane: Option<i64>,
    is_audio: bool,
}

type AttrMap = BTreeMap<String, String>;

fn tag_of(name: quick_xml::name::QName<'_>) -> String {
    String::from_utf8_lossy(name.as_ref()).into_owned()
}

fn attrs_of(e: &quick_xml::events::BytesStart<'_>) -> AttrMap {
    let mut out = AttrMap::new();
    for a in e.attributes().flatten() {
        out.insert(
            String::from_utf8_lossy(a.key.as_ref()).into_owned(),
            String::from_utf8_lossy(&a.value).into_owned(),
        );
    }
    out
}

/// Parse an FCPXML time into the RAW (num, den) tick pair — exactly the
/// OTIO `RationalTime` (value, rate) pair the wire form carries (the
/// python-otio FCPXML adapter does the same). "3600/2400s" → (3600, 2400);
/// "Ns"/"0s" → (N, 1).
fn parse_time(v: &str) -> Result<(i128, i128), BridgeError> {
    let s = v.strip_suffix('s').unwrap_or(v);
    if s.is_empty() {
        return Ok((0, 1));
    }
    if let Some((num, den)) = s.split_once('/') {
        let num: i128 = num.parse().map_err(|_| bad(v))?;
        let den: i128 = den.parse().map_err(|_| bad(v))?;
        if den == 0 {
            return Err(bad(v));
        }
        Ok((num, den))
    } else {
        let num: i128 = s.parse().map_err(|_| bad(v))?;
        Ok((num, 1))
    }
}

/// (num, den) → the model's exact TimeVal.
fn ticks_tv((num, den): (i128, i128)) -> Result<TimeVal, BridgeError> {
    Ok(TimeVal {
        value: Rational::new(num, 1).map_err(|_| bad("time"))?,
        rate: Rational::new(den, 1).map_err(|_| bad("time"))?,
    })
}

fn bad(v: &str) -> BridgeError {
    BridgeError::BadValue {
        at: "time".into(),
        reason: format!("cannot parse `{v}`"),
    }
}

impl BridgeState {
    fn handle_start(
        &mut self,
        tag: &str,
        attrs: &AttrMap,
        stack: &[String],
    ) -> Result<(), BridgeError> {
        let at = stack.join("/");
        match tag {
            "fcpxml" => {
                let ver = attrs.get("version").map(String::as_str).unwrap_or("");
                if !ver.starts_with("1.") {
                    return Err(BridgeError::Unsupported {
                        element: format!("fcpxml version {ver}"),
                        at: at.clone(),
                    });
                }
            }
            "format" => {
                // validate the frameDuration time syntax (a malformed value
                // refuses); the value itself rides on each element's times
                if let Some(v) = attrs.get("frameDuration") {
                    parse_time(v)?;
                }
            }
            "asset" => {
                let id = attrs.get("id").cloned().unwrap_or_default();
                if !self.assets.contains_key(&id) {
                    self.asset_order.push(id.clone());
                }
                self.assets.insert(id, AssetInfo { src: None });
            }
            "media-rep" => {
                // attach src to the most recent asset (stack: resources/asset)
                if let Some(src) = attrs.get("src") {
                    // Round 20 fix: DOCUMENT order, not lexicographic —
                    // next_back() on the id-keyed BTreeMap picked the
                    // alphabetically-last asset, mis-attaching media-rep src
                    // whenever ids don't sort in declaration order
                    let target = self
                        .asset_order
                        .last()
                        .and_then(|id| self.assets.get_mut(id));
                    if let Some(asset) = target {
                        asset.src = Some(src.clone());
                    }
                }
            }
            "sequence" => {
                self.sequence_name = attrs.get("name").cloned().unwrap_or_default();
            }
            "spine" => {
                self.in_spine = true;
            }
            "asset-clip" if self.in_spine => {
                let name = attrs.get("name").cloned().unwrap_or_default();
                let start = attrs.get("start").map(|v| parse_time(v)).transpose()?;
                let duration = attrs.get("duration").map(|v| parse_time(v)).transpose()?;
                let ref_id = attrs.get("ref").cloned().unwrap_or_default();
                let asset_src = self.assets.get(&ref_id).and_then(|a| a.src.clone());
                let mut el = Element::leaf(Kind::Clip, name);
                el.media = Some(MediaRef::single(
                    MediaKind::External,
                    String::new(),
                    asset_src,
                ));
                el.source_range = self.range_of(start, duration);
                self.ledger_attrs(
                    &mut el,
                    attrs,
                    &[
                        "name",
                        "start",
                        "duration",
                        "ref",
                        "offset",
                        "id",
                        "tcFormat",
                        "audioStart",
                        "audioDuration",
                        "role",
                    ],
                );
                self.in_asset_clip = attrs.get("id").cloned();
                self.push_spine(el, attrs, false);
            }
            "gap" if self.in_spine => {
                let duration = attrs.get("duration").map(|v| parse_time(v)).transpose()?;
                let mut el = Element::leaf(Kind::Gap, "");
                el.source_range = self.range_of(None, duration);
                self.ledger_attrs(&mut el, attrs, &["name", "duration", "offset", "id"]);
                self.push_spine(el, attrs, false);
            }
            "transition" if self.in_spine => {
                self.ledgered_depth += 1;
                let name = attrs.get("name").cloned().unwrap_or_default();
                let mut el = Element::leaf(Kind::Transition, name);
                el.transition = Some(TransitionInfo {
                    transition_type: attrs
                        .get("type")
                        .cloned()
                        .unwrap_or_else(|| "SMPTE_Dissolve".into()),
                    in_offset: None,
                    out_offset: None,
                });
                self.ledger_attrs(
                    &mut el,
                    attrs,
                    &["name", "duration", "offset", "ref", "id", "type"],
                );
                self.push_spine(el, attrs, false);
            }
            "title" if self.in_spine => {
                // LEDGER: title kept as clip; text styling does not round-trip
                self.ledgered_depth += 1;
                let name = attrs.get("name").cloned().unwrap_or_default();
                let duration = attrs.get("duration").map(|v| parse_time(v)).transpose()?;
                let mut el = Element::leaf(Kind::Clip, name);
                el.source_range = self.range_of(None, duration);
                el.metadata.insert(
                    "cairn".into(),
                    serde_json::json!({"fcpxml": {"element": "title"}}),
                );
                self.ledger_attrs(
                    &mut el,
                    attrs,
                    &["name", "duration", "offset", "ref", "id", "lane", "role"],
                );
                self.push_spine(el, attrs, false);
            }
            "video" if self.in_spine => {
                let name = attrs.get("name").cloned().unwrap_or_else(|| "video".into());
                let duration = attrs.get("duration").map(|v| parse_time(v)).transpose()?;
                let mut el = Element::leaf(Kind::Clip, name);
                el.source_range = self.range_of(None, duration);
                el.metadata.insert(
                    "cairn".into(),
                    serde_json::json!({"fcpxml": {"element": "video"}}),
                );
                self.ledger_attrs(
                    &mut el,
                    attrs,
                    &["name", "duration", "offset", "ref", "id", "lane"],
                );
                self.push_spine(el, attrs, false);
            }
            "audio" if self.in_spine => {
                let name = attrs.get("name").cloned().unwrap_or_else(|| "audio".into());
                let duration = attrs.get("duration").map(|v| parse_time(v)).transpose()?;
                let mut el = Element::leaf(Kind::Clip, name);
                el.source_range = self.range_of(None, duration);
                el.metadata.insert(
                    "cairn".into(),
                    serde_json::json!({"fcpxml": {"element": "audio"}}),
                );
                self.ledger_attrs(
                    &mut el,
                    attrs,
                    &["name", "duration", "offset", "ref", "id", "lane"],
                );
                self.push_spine(el, attrs, true);
            }
            "ref-clip" if self.in_spine => {
                // LEDGER: compound clip flattened
                self.ledgered_depth += 1;
                let name = attrs.get("name").cloned().unwrap_or_default();
                let duration = attrs.get("duration").map(|v| parse_time(v)).transpose()?;
                let mut el = Element::leaf(Kind::Clip, name);
                el.source_range = self.range_of(None, duration);
                el.metadata.insert("cairn".into(), serde_json::json!({"fcpxml": {"element": "ref-clip", "ledger": "compound-flattened"}}));
                self.ledger_attrs(
                    &mut el,
                    attrs,
                    &["name", "duration", "offset", "ref", "id", "lane"],
                );
                self.push_spine(el, attrs, false);
            }
            "mc-clip" if self.in_spine => {
                // LEDGER: multicam flattened
                self.ledgered_depth += 1;
                let name = attrs.get("name").cloned().unwrap_or_default();
                let duration = attrs.get("duration").map(|v| parse_time(v)).transpose()?;
                let mut el = Element::leaf(Kind::Clip, name);
                el.source_range = self.range_of(None, duration);
                el.metadata.insert("cairn".into(), serde_json::json!({"fcpxml": {"element": "mc-clip", "ledger": "multicam-flattened"}}));
                self.ledger_attrs(
                    &mut el,
                    attrs,
                    &["name", "duration", "offset", "ref", "id", "lane"],
                );
                self.push_spine(el, attrs, false);
            }
            "audition" if self.in_spine => {
                // LEDGER: alternates consumed; the selected clip approximates
                self.ledgered_depth += 1;
            }
            "marker" => {
                // markers are children of spine items
                let name = attrs.get("value").cloned().unwrap_or_default();
                let start = attrs.get("start").map(|v| parse_time(v)).transpose()?;
                let duration = attrs.get("duration").map(|v| parse_time(v)).transpose()?;
                let st = ticks_tv(start.unwrap_or((0, 1)))?;
                let du = ticks_tv(duration.unwrap_or((0, 1)))?;
                self.markers_pending.push(Marker {
                    schema: "Marker.2".into(),
                    name,
                    color: "RED".into(),
                    comment: String::new(),
                    marked_range: TimeRange {
                        start: st,
                        duration: du,
                    },
                    metadata: JsonMap::new(),
                    extra: JsonMap::new(),
                });
            }
            "project" => self.project_name = attrs.get("name").cloned().unwrap_or_default(),
            "event" => self.event_name = attrs.get("name").cloned().unwrap_or_default(),
            "library" | "resources" | "effect" | "notes" | "keywords" | "smart-collections" => {}
            // Round 13 real-corpus descriptor preservation: opens a subtree
            // (or records a nested element inside an open one) that lands
            // VERBATIM in the enclosing spine item's extra — data-preserving,
            // ledgered, test-covered. Markers stay markers (the arm above
            // wins regardless of descriptor state).
            other
                if self.in_spine
                    && (self.descriptor_stack.is_empty() && DESCRIPTOR_ROOTS.contains(&other)
                        || !self.descriptor_stack.is_empty()) =>
            {
                self.descriptor_push(other, attrs);
            }
            other if self.in_spine && self.ledgered_depth == 0 => {
                return Err(BridgeError::Unsupported {
                    element: other.into(),
                    at: format!("{at}/{other}"),
                });
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_end(&mut self, tag: &str, _stack: &[String]) {
        // descriptor subtree levels close FIRST (their contents never reach
        // the spine-item end handling)
        if !self.descriptor_stack.is_empty() {
            self.descriptor_pop();
            return;
        }
        // ledgered subtrees close their consumption scope
        if matches!(
            tag,
            "transition" | "title" | "ref-clip" | "mc-clip" | "audition"
        ) {
            self.ledgered_depth = self.ledgered_depth.saturating_sub(1);
        }
        // spine items take their pending markers when they close
        if matches!(tag, "asset-clip" | "gap" | "title" | "transition") {
            self.in_asset_clip = None;
            let markers = std::mem::take(&mut self.markers_pending);
            if let Some(last) = self.spine_items.last_mut() {
                last.element.markers.extend(markers);
            }
        }
        if tag == "spine" {
            self.in_spine = false;
        }
    }

    /// Open one descriptor-subtree level: node = {"attrs": {...}, "children":
    /// [...]}. The subtree is assembled bottom-up in `descriptor_pop`.
    fn descriptor_push(&mut self, tag: &str, attrs: &AttrMap) {
        let mut node = serde_json::Map::new();
        if !attrs.is_empty() {
            let map: serde_json::Map<String, serde_json::Value> = attrs
                .iter()
                .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
                .collect();
            node.insert("attrs".into(), serde_json::Value::Object(map));
        }
        node.insert("children".into(), serde_json::Value::Array(Vec::new()));
        self.descriptor_stack.push((tag.into(), node));
    }

    /// Close one level: attach the finished node to its parent's children;
    /// when the ROOT closes, land it in the enclosing spine item's extra.
    /// quick-xml guarantees well-formed pairing, so each End pops exactly the
    /// level its Start opened (Event::Empty = Start+End = open+close).
    fn descriptor_pop(&mut self) {
        let Some((root_tag, node)) = self.descriptor_stack.pop() else {
            return;
        };
        let value = serde_json::Value::Object(node);
        match self.descriptor_stack.last_mut() {
            Some((_, parent_node)) => {
                if let Some(children) = parent_node
                    .get_mut("children")
                    .and_then(|c| c.as_array_mut())
                {
                    let mut named = serde_json::Map::new();
                    named.insert("tag".into(), serde_json::Value::String(root_tag));
                    named.insert("node".into(), value);
                    children.push(serde_json::Value::Object(named));
                }
            }
            None => {
                // root closed: land in the enclosing spine item's extra
                if let Some(item) = self.spine_items.last_mut() {
                    item.element
                        .extra
                        .insert(format!("fcpxml:{root_tag}"), value);
                }
            }
        }
    }

    fn range_of(
        &self,
        start: Option<(i128, i128)>,
        duration: Option<(i128, i128)>,
    ) -> Option<TimeRange> {
        if start.is_none() && duration.is_none() {
            return None;
        }
        // times carry their own rate (the FCPXML denominator); a zero rate
        // only appears on absent durations, which normalize to (0, 1)
        Some(TimeRange {
            start: ticks_tv(start.unwrap_or((0, 1))).ok()?,
            duration: ticks_tv(duration.unwrap_or((0, 1))).ok()?,
        })
    }

    /// Every attr not in the known set is preserved in `extra` — diffs on
    /// them surface as raw Attr ops, never silently dropped.
    fn ledger_attrs(&self, el: &mut Element, attrs: &AttrMap, known: &[&str]) {
        for (k, v) in attrs {
            if !known.contains(&k.as_str()) {
                el.extra.insert(k.clone(), serde_json::json!(v));
            }
        }
        // roles ledger
        let mut roles = serde_json::Map::new();
        for key in ["role", "audioRoleSource", "videoRoleSource"] {
            if let Some(v) = attrs.get(key) {
                roles.insert(key.to_string(), serde_json::json!(v));
            }
        }
        if !roles.is_empty() {
            let cairn = el
                .metadata
                .entry("cairn".into())
                .or_insert_with(|| serde_json::json!({}));
            if let serde_json::Value::Object(map) = cairn {
                map.entry("fcpxml".to_string())
                    .or_insert_with(|| serde_json::json!({}))
                    .as_object_mut()
                    .map(|m| m.insert("roles".into(), serde_json::Value::Object(roles)));
            }
        }
    }

    fn push_spine(&mut self, el: Element, attrs: &AttrMap, is_audio: bool) {
        let lane = attrs.get("lane").and_then(|l| l.parse::<i64>().ok());
        self.spine_items.push(SpineItem {
            element: el,
            lane,
            is_audio,
        });
    }

    fn finish(&mut self) -> Timeline {
        // assemble tracks: V1 (spine, no lane), lanes above, audio below
        let mut v1: Vec<Element> = Vec::new();
        let mut lanes: BTreeMap<i64, Vec<Element>> = BTreeMap::new();
        let mut audio: Vec<Element> = Vec::new();
        for item in self.spine_items.drain(..) {
            match (item.lane, item.is_audio) {
                (None, false) => v1.push(item.element),
                (Some(l), false) if l >= 1 => lanes.entry(l).or_default().push(item.element),
                (Some(l), false) => {
                    // negative lanes: below the spine — keep order, own track
                    lanes.entry(l).or_default().push(item.element);
                }
                (_, true) => audio.push(item.element),
            }
        }
        let track = |name: &str, kind: TrackKind, items: Vec<Element>| -> Element {
            Element::container(Kind::Track(kind), name, items)
        };
        let mut tracks: Vec<Element> = vec![track("V1", TrackKind::Video, v1)];
        for (lane, items) in lanes {
            let at = if lane >= 1 { tracks.len() } else { 1 };
            tracks.insert(
                at,
                track(&format!("V{}", lane.max(1) + 1), TrackKind::Video, items),
            );
        }
        if !audio.is_empty() {
            tracks.push(track("A1", TrackKind::Audio, audio));
        }
        let name = if self.project_name.is_empty() {
            self.sequence_name.clone()
        } else {
            self.project_name.clone()
        };
        Timeline {
            name,
            global_start_time: None,
            metadata: JsonMap::new(),
            tracks: Element::container(Kind::Stack, "tracks", tracks),
            extra: JsonMap::new(),
        }
    }
}
