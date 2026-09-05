//! Review notes (ADR-0018): frame-anchored, content-addressed, and
//! deterministically three-way mergeable — the answer to "scattered feedback
//! hell" (Frame.io threads, email, WhatsApp screenshots, sticky notes).
//!
//! A note is anchored to a FRAME (exact rational) and optionally to a clip
//! identity. Its id is content-derived:
//!
//! ```text
//! id = blake3(anchor_key ‖ body ‖ author)[0..16]
//! ```
//!
//! Content-derived ids give the merge its editorial semantics for free:
//!
//! - **an edit is a new id** — the old note vanishes, the new one appears,
//!   and both sides converge without ever mangling text mid-sentence;
//! - **a status flip keeps the id** — deterministic lattice decides
//!   (Resolved is sticky; Rejected-vs-Resolved is the one real conflict);
//! - **unchanged-vs-delete → deletion wins** (a note a human removed stays
//!   removed); **edit-vs-delete → the edit survives** (it is a NEW note).
//!
//! The same-anchor-same-author collision (two notes at the same frame by the
//! same author with different text) is surfaced as a conflict entry — that is
//! the genuine "two people answered the same client comment differently"
//! case, and no algorithm should silently pick one.
//!
//! CSV import/export (with the `Frame Number` column alias real review tools
//! use) makes the round-trip Frame.io ⇄ cairn lossless enough for editorial
//! work.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Note lifecycle. `Resolved` is sticky across merges.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum NoteStatus {
    Open,
    Resolved,
    Rejected,
}

impl NoteStatus {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            NoteStatus::Open => "OPEN",
            NoteStatus::Resolved => "RESOLVED",
            NoteStatus::Rejected => "REJECTED",
        }
    }

    /// Parse (case-insensitive, leading/trailing space tolerant).
    #[must_use]
    pub fn parse(s: &str) -> Option<NoteStatus> {
        match s.trim().to_ascii_uppercase().as_str() {
            "OPEN" => Some(NoteStatus::Open),
            "RESOLVED" | "DONE" | "FIXED" | "ADDRESSED" => Some(NoteStatus::Resolved),
            "REJECTED" | "WONTFIX" | "WONT-FIX" | "DISMISSED" => Some(NoteStatus::Rejected),
            _ => None,
        }
    }
}

/// Where a note points: an exact frame at a rate, optionally a clip identity
/// (element uuid or name — the identity ladder's first rungs), and since
/// ADR-0028 an optional inclusive frame RANGE (a comment that spans a
/// region, not one frame).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoteAnchor {
    /// Clip uuid / name-path key (identity ladder rung a/b), if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clip: Option<String>,
    /// Frame number (exact integer at `rate`). For v2 range notes this is
    /// the range START (the seek target); the anchor keeps it for v1 compat.
    pub frame: i128,
    /// Frames per second the frame number counts in (e.g. 24, 25, 30000/1001).
    pub rate: i128,
    /// Inclusive (start, end) frame range — v2 only. `None` (or `[f, f]`)
    /// is the v1 degenerate: a point note.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<(i128, i128)>,
}

impl NoteAnchor {
    /// The merge key: clip identity if present, else the range start. A
    /// range note and a point note at the range start deliberately land in
    /// the SAME bucket (ADR-0028 §B) — that is where a conflict entry is
    /// useful: two editors talking about the same region.
    #[must_use]
    pub fn key(&self) -> String {
        let start = self.range_start();
        match &self.clip {
            Some(c) => format!("clip:{c}"),
            None => format!("frame:{start}@{}", self.rate),
        }
    }

    /// The effective range: `range` when present, else the degenerate
    /// point `[frame, frame]` (a v1 note parses as v2 with this envelope).
    #[must_use]
    pub fn effective_range(&self) -> (i128, i128) {
        self.range.unwrap_or((self.frame, self.frame))
    }

    /// The range's start frame (the seek target / merge-bucket frame).
    #[must_use]
    pub fn range_start(&self) -> i128 {
        self.range.map_or(self.frame, |r| r.0)
    }

    /// The v2 id material's range key: `"{start}:{end}@{rate}"`.
    #[must_use]
    pub fn range_key(&self) -> String {
        let (s, e) = self.effective_range();
        format!("{s}:{e}@{}", self.rate)
    }
}

/// What kind of note this is (ADR-0028 §C): a plain comment, a marker
/// pinned to a spot on the frame, or a drawn annotation overlay. v1 notes
/// are always `Comment` (the default envelope).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum NoteKind {
    #[default]
    Comment,
    Pin,
    Annotation,
}

impl NoteKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            NoteKind::Comment => "comment",
            NoteKind::Pin => "pin",
            NoteKind::Annotation => "annotation",
        }
    }

    /// Parse (case-insensitive; unknown -> `None`).
    #[must_use]
    pub fn parse(s: &str) -> Option<NoteKind> {
        match s.trim().to_ascii_lowercase().as_str() {
            "comment" | "note" => Some(NoteKind::Comment),
            "pin" | "marker" => Some(NoteKind::Pin),
            "annotation" | "drawing" | "overlay" => Some(NoteKind::Annotation),
            _ => None,
        }
    }
}

/// Per-note visibility (ADR-0028 §E): `Internal` notes sync to studio
/// devices but are filtered at the review-portal boundary — a client
/// holding a guest link literally never receives the bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum NoteVisibility {
    #[default]
    Public,
    Internal,
}

impl NoteVisibility {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            NoteVisibility::Public => "public",
            NoteVisibility::Internal => "internal",
        }
    }

    /// Parse (case-insensitive; unknown -> `None`).
    #[must_use]
    pub fn parse(s: &str) -> Option<NoteVisibility> {
        match s.trim().to_ascii_lowercase().as_str() {
            "public" | "client" => Some(NoteVisibility::Public),
            "internal" | "private" | "studio" => Some(NoteVisibility::Internal),
            _ => None,
        }
    }
}

/// One review note. `PartialEq` only (the v2 pin carries f32s, which
/// are not `Eq`); ordering is by id via the BTreeMap, never by value.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Note {
    /// Content-derived id (v1: blake3 of anchor‖body‖author; v2: the
    /// "note2" material — see [`Note::derive_id`]). 16 hex chars.
    pub id: String,
    pub author: String,
    pub body: String,
    pub status: NoteStatus,
    pub anchor: NoteAnchor,
    pub created_ms: i64,
    /// v2: comment / pin / annotation. Serialized only when not Comment,
    /// so v1 files stay byte-identical.
    #[serde(default, skip_serializing_if = "is_default_kind")]
    pub kind: NoteKind,
    /// v2: normalized on-frame position (x, y) in 0.0..=1.0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pin: Option<(f32, f32)>,
    /// v2: BLAKE3 hex of the annotation overlay blob in the project CAS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachment: Option<String>,
    /// v2: public / internal. Serialized only when Internal.
    #[serde(default, skip_serializing_if = "is_default_visibility")]
    pub visibility: NoteVisibility,
}

// serde's `skip_serializing_if` hands the predicate a REFERENCE, so the
// by-value clippy suggestion does not apply here.
#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_default_kind(k: &NoteKind) -> bool {
    *k == NoteKind::Comment
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_default_visibility(v: &NoteVisibility) -> bool {
    *v == NoteVisibility::Public
}

impl Note {
    /// v1 id material: `blake3(anchor_key ‖ 0x1F ‖ body ‖ 0x1F ‖ author)`.
    fn id_material_v1(anchor: &NoteAnchor, body: &str, author: &str) -> Vec<u8> {
        let mut m = Vec::new();
        m.extend_from_slice(anchor.key().as_bytes());
        m.push(0x1F);
        m.extend_from_slice(body.as_bytes());
        m.push(0x1F);
        m.extend_from_slice(author.as_bytes());
        m
    }

    /// v2 id material (ADR-0028 §A): the literal `"note2"` tag FIRST, then
    /// the v1 fields, then kind / range_key / visibility. The tag makes
    /// v1-vs-v2 collision impossible by construction: v1 material begins
    /// with `clip:`/`frame:`, v2 begins with `note2`.
    ///
    /// `pin` and `attachment` are deliberately NOT id material: they are
    /// presentation/attachment data that merges field-wise (a moved pin is
    /// an edit to the same note, not a new note).
    fn id_material_v2(note: &Note) -> Vec<u8> {
        let mut m = Vec::new();
        m.extend_from_slice(b"note2");
        m.push(0x1F);
        m.extend_from_slice(note.anchor.key().as_bytes());
        m.push(0x1F);
        m.extend_from_slice(note.body.as_bytes());
        m.push(0x1F);
        m.extend_from_slice(note.author.as_bytes());
        m.push(0x1F);
        m.extend_from_slice(note.kind.as_str().as_bytes());
        m.push(0x1F);
        m.extend_from_slice(note.anchor.range_key().as_bytes());
        m.push(0x1F);
        m.extend_from_slice(note.visibility.as_str().as_bytes());
        m
    }

    fn hash16(material: &[u8]) -> String {
        let h = blake3::hash(material);
        let mut out = String::with_capacity(16);
        for b in &h.as_bytes()[..8] {
            out.push_str(&format!("{b:02x}"));
        }
        out
    }

    /// Compute the v1 content-derived id (blake3 of anchor‖body‖author).
    #[must_use]
    pub fn derive_id(anchor: &NoteAnchor, body: &str, author: &str) -> String {
        Note::hash16(&Note::id_material_v1(anchor, body, author))
    }

    /// Does this note carry any v2 feature? Writers choose per note
    /// (ADR-0028 §A): a plain frame comment is written as v1 — smallest
    /// representation, broadest compat; anything with a range, pin,
    /// annotation attachment, non-Comment kind, or Internal visibility is
    /// written as v2.
    #[must_use]
    pub fn is_v2(&self) -> bool {
        self.kind != NoteKind::Comment
            || self.pin.is_some()
            || self.attachment.is_some()
            || self.visibility == NoteVisibility::Internal
            || self
                .anchor
                .range
                .is_some_and(|(s, e)| s != self.anchor.frame || e != self.anchor.frame)
    }

    /// Build a note (id derived from content, v1 or v2 by shape).
    #[must_use]
    pub fn new(
        author: impl Into<String>,
        body: impl Into<String>,
        anchor: NoteAnchor,
        status: NoteStatus,
        created_ms: i64,
    ) -> Note {
        Note::with_envelope(
            author,
            body,
            anchor,
            status,
            created_ms,
            NoteKind::Comment,
            None,
            None,
            NoteVisibility::Public,
        )
    }

    /// Build a note with the v2 envelope fields. Fields at their defaults
    /// produce a plain v1 note (same id as [`Note::new`]); any v2 feature
    /// switches the id material to the versioned v2 formula.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn with_envelope(
        author: impl Into<String>,
        body: impl Into<String>,
        anchor: NoteAnchor,
        status: NoteStatus,
        created_ms: i64,
        kind: NoteKind,
        pin: Option<(f32, f32)>,
        attachment: Option<String>,
        visibility: NoteVisibility,
    ) -> Note {
        let author = author.into();
        let body = body.into();
        let mut note = Note {
            id: String::new(),
            author,
            body,
            status,
            anchor,
            created_ms,
            kind,
            pin,
            attachment,
            visibility,
        };
        note.id = if note.is_v2() {
            Note::hash16(&Note::id_material_v2(&note))
        } else {
            Note::derive_id(&note.anchor, &note.body, &note.author)
        };
        note
    }
}

/// An ordered set of notes (BTreeMap: deterministic serialization for free).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct NoteSet {
    pub notes: BTreeMap<String, Note>,
}

impl NoteSet {
    #[must_use]
    pub fn from_notes(notes: impl IntoIterator<Item = Note>) -> NoteSet {
        NoteSet {
            notes: notes.into_iter().map(|n| (n.id.clone(), n)).collect(),
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.notes.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.notes.is_empty()
    }

    /// Serialize to canonical JSON bytes (BTreeMap order — byte-stable).
    pub fn to_json(&self) -> Result<Vec<u8>, String> {
        serde_json::to_vec_pretty(self).map_err(|e| format!("serialize notes: {e}"))
    }

    /// Parse from canonical JSON bytes.
    pub fn from_json(bytes: &[u8]) -> Result<NoteSet, String> {
        serde_json::from_slice(bytes).map_err(|e| format!("parse notes: {e}"))
    }

    /// Parse from a JSON string.
    pub fn from_json_str(s: &str) -> Result<NoteSet, String> {
        serde_json::from_str(s).map_err(|e| format!("parse notes: {e}"))
    }
}

/// A merge conflict the HUMAN must decide (never auto-resolved text).
#[derive(Clone, Debug, PartialEq)]
pub struct NoteConflict {
    /// The anchor both sides wrote to (human-readable).
    pub anchor: String,
    pub ours: Note,
    pub theirs: Note,
    pub reason: String,
}

/// Deterministic three-way merge result.
#[derive(Clone, Debug, Default)]
pub struct NoteMerge {
    pub merged: NoteSet,
    pub conflicts: Vec<NoteConflict>,
}

/// Deterministic three-way merge of review notes.
///
/// Rules (ADR-0018 §3, each one property-tested):
/// 1. added-on-one-side → kept (union);
/// 2. identical → kept;
/// 3. unchanged-vs-delete → **deletion wins** (a human removed it);
/// 4. edit-vs-delete → the edit survives (it is a NEW id — rule 1);
/// 5. same id, both sides touched status → lattice: Resolved is sticky;
///    Rejected-vs-Resolved is a conflict;
/// 6. same anchor + same author + different bodies (different ids) → both
///    kept + a conflict entry — the "two answers to one comment" case.
// ours/theirs/base triple idiom carries short names on purpose.
#[allow(clippy::many_single_char_names)]
#[must_use]
pub fn merge_notes(base: &NoteSet, ours: &NoteSet, theirs: &NoteSet) -> NoteMerge {
    let mut merged = NoteSet::default();
    let mut conflicts = Vec::new();

    let all_ids: std::collections::BTreeSet<&String> = base
        .notes
        .keys()
        .chain(ours.notes.keys())
        .chain(theirs.notes.keys())
        .collect();

    for id in all_ids {
        let base_note = base.notes.get(id);
        let ours_note = ours.notes.get(id);
        let theirs_note = theirs.notes.get(id);
        match (base_note, ours_note, theirs_note) {
            // 1) added on one side only → kept (union)
            (None, Some(o), None) => {
                merged.notes.insert(o.id.clone(), o.clone());
            }
            (None, None, Some(t)) => {
                merged.notes.insert(t.id.clone(), t.clone());
            }
            // 2) both sides hold the id: same content material by
            // construction — only identity-NEUTRAL fields can differ
            // (status, created, and since ADR-0028 pin/attachment)
            (Some(base_note), Some(o), Some(t)) => {
                if o == t {
                    merged.notes.insert(o.id.clone(), o.clone());
                } else {
                    match merge_status(o.status, t.status) {
                        Ok(s) => {
                            let mut n = o.clone();
                            n.status = s;
                            n.created_ms = o.created_ms.max(t.created_ms);
                            // v2 identity-neutral fields: three-way per
                            // field — the changed side wins; both-changed
                            // is a genuine fork a human must decide
                            let mut diverged = None;
                            match pick_field("pin", base_note, o, t, |x| x.pin) {
                                Ok(v) => n.pin = v,
                                Err(field) => diverged = Some(field),
                            }
                            match pick_field("attachment", base_note, o, t, |x| {
                                x.attachment.clone()
                            }) {
                                Ok(v) => n.attachment = v,
                                Err(field) => diverged = Some(field),
                            }
                            merged.notes.insert(n.id.clone(), n);
                            if let Some(field) = diverged {
                                conflicts.push(NoteConflict {
                                    anchor: o.anchor.key(),
                                    ours: o.clone(),
                                    theirs: t.clone(),
                                    reason: format!("{field} moved differently on both sides — an editorial decision, not a merge"),
                                });
                            }
                        }
                        Err(reason) => {
                            // keep OURS in the set + surface the conflict
                            merged.notes.insert(o.id.clone(), o.clone());
                            conflicts.push(NoteConflict {
                                anchor: o.anchor.key(),
                                ours: o.clone(),
                                theirs: t.clone(),
                                reason,
                            });
                        }
                    }
                }
            }
            // 3) both added the id independently: identical → one; different
            // → an id collision (16-hex content ids — should never fire)
            (None, Some(o), Some(t)) => {
                merged.notes.insert(o.id.clone(), o.clone());
                if o != t {
                    conflicts.push(NoteConflict {
                        anchor: o.anchor.key(),
                        ours: o.clone(),
                        theirs: t.clone(),
                        reason: "id collision with different content".into(),
                    });
                }
            }
            // 4) deleted on BOTH sides — stay deleted
            (Some(_), None, None) => {}
            // 5) deletion (one side) vs kept (other side) → deletion WINS.
            //    A body edit would have produced a NEW id (case 1); a status
            //    flip alone does not protect a note a human removed.
            (Some(_), None, Some(_)) => {}
            (Some(_), Some(_), None) => {}
            (None, None, None) => unreachable!("id came from somewhere"),
        }
    }

    // rule 6: same anchor + same author, different bodies (distinct ids)
    let by_author_anchor: BTreeMap<(String, String), Vec<&Note>> = {
        let mut m: BTreeMap<(String, String), Vec<&Note>> = BTreeMap::new();
        for n in merged.notes.values() {
            m.entry((n.author.clone(), n.anchor.key()))
                .or_default()
                .push(n);
        }
        m
    };
    for ((_author, anchor), group) in by_author_anchor {
        if group.len() > 1 {
            // deterministic pair: first (by id) vs second
            let mut sorted: Vec<&Note> = group;
            sorted.sort_by(|a, b| a.id.cmp(&b.id));
            let (first, second) = (sorted[0], sorted[1]);
            conflicts.push(NoteConflict {
                anchor: anchor.clone(),
                ours: (*first).clone(),
                theirs: (*second).clone(),
                reason: "same author answered the same anchor twice".into(),
            });
        }
    }

    NoteMerge { merged, conflicts }
}

/// Status lattice: Resolved is sticky; Rejected-vs-Resolved is a conflict.
fn merge_status(a: NoteStatus, b: NoteStatus) -> Result<NoteStatus, String> {
    use NoteStatus::{Open, Rejected, Resolved};
    match (a, b) {
        (Open, Open) => Ok(Open),
        (Resolved, Resolved) => Ok(Resolved),
        (Rejected, Rejected) => Ok(Rejected),
        (Open, x) | (x, Open) => Ok(x),
        (Rejected, Resolved) | (Resolved, Rejected) => {
            Err("rejected vs resolved — an editorial decision, not a merge".into())
        }
    }
}

/// Three-way pick for one identity-neutral field (ADR-0028): equal →
/// ours; only theirs changed → theirs; only ours changed → ours; both
/// changed differently → `Err(field)` (a fork a human must decide).
fn pick_field<T: PartialEq + Clone>(
    field: &'static str,
    base: &Note,
    ours: &Note,
    theirs: &Note,
    get: impl Fn(&Note) -> T,
) -> Result<T, &'static str> {
    let (b, o, t) = (get(base), get(ours), get(theirs));
    if o == t {
        Ok(o)
    } else if o == b {
        Ok(t)
    } else if t == b {
        Ok(o)
    } else {
        Err(field)
    }
}

// ---- CSV ---------------------------------------------------------------------

/// CSV export/import for review-tool interop (Frame.io-style exports).
///
/// Columns: `id,frame,rate,timecode,clip,author,status,body` — plus the
/// v2 columns appended (ADR-0028): `kind,range_end,pin_x,pin_y,
/// attachment,visibility`. v1 rows emit them empty (a v1 file's bytes are
/// unchanged apart from the header). Import accepts `Frame Number` as an
/// alias for `frame` (the header real review tools emit), `Timecode` for
/// tc, and derives missing ids from content. Rate defaults to 24 when no
/// `rate` column. A `pin` row may carry an empty body (a pure marker).
pub mod csv {
    use super::{Note, NoteAnchor, NoteKind, NoteSet, NoteStatus, NoteVisibility};

    /// One parsed CSV row error (line + reason).
    #[derive(Debug)]
    pub struct CsvError {
        pub line: usize,
        pub reason: String,
    }

    /// Frame number → `HH:MM:SS:FF` at `rate` (editorial convention;
    /// negative frames stay negative-prefixed — offline media land there).
    #[must_use]
    pub fn timecode(frame: i128, rate: i128) -> String {
        if rate <= 0 {
            return format!("{frame}");
        }
        let sign = if frame < 0 { "-" } else { "" };
        let f = frame.unsigned_abs();
        let rate = u128::try_from(rate).unwrap_or(1);
        let secs = f / rate;
        let ff = f % rate;
        let mm = (secs / 60) % 60;
        let ss = secs % 60;
        let hh = secs / 3600;
        format!("{sign}{hh:02}:{mm:02}:{ss:02}:{ff:02}")
    }

    /// Export a NoteSet to CSV bytes (deterministic: frame, then id order).
    /// All notes, both visibilities (the studio's own file).
    pub fn export(set: &NoteSet) -> String {
        export_visible(set, None)
    }

    /// Export with a visibility filter (ADR-0028 §E): `Some(Public)` is
    /// "what the client gets" (the CLI default), `Some(Internal)` the
    /// studio-only residue, `None` everything.
    pub fn export_visible(set: &NoteSet, only: Option<NoteVisibility>) -> String {
        let mut rows: Vec<&Note> = set
            .notes
            .values()
            .filter(|n| only.is_none_or(|v| n.visibility == v))
            .collect();
        rows.sort_by(|a, b| {
            (a.anchor.frame, a.anchor.rate, &a.id).cmp(&(b.anchor.frame, b.anchor.rate, &b.id))
        });
        let mut out = String::from(
            "id,frame,rate,timecode,clip,author,status,body,kind,range_end,pin_x,pin_y,attachment,visibility\n",
        );
        for n in rows {
            let clip = n.anchor.clip.as_deref().unwrap_or("");
            let tc = timecode(n.anchor.frame, n.anchor.rate);
            let body = csv_escape(&n.body);
            let (_, e) = n.anchor.effective_range();
            let range_end = if e == n.anchor.frame {
                String::new()
            } else {
                format!("{e}")
            };
            let (pin_x, pin_y) = match n.pin {
                Some((x, y)) => (format!("{x:.3}"), format!("{y:.3}")),
                None => (String::new(), String::new()),
            };
            out.push_str(&format!(
                "{},{},{},{},{},{},{},{},{},{},{},{},{},{}\n",
                n.id,
                n.anchor.frame,
                n.anchor.rate,
                tc,
                csv_escape(clip),
                csv_escape(&n.author),
                n.status.as_str(),
                body,
                n.kind.as_str(),
                range_end,
                pin_x,
                pin_y,
                csv_escape(n.attachment.as_deref().unwrap_or("")),
                n.visibility.as_str(),
            ));
        }
        out
    }

    /// Minimal CSV quoting: wrap in quotes when the field contains a comma,
    /// quote, or newline; double embedded quotes.
    fn csv_escape(s: &str) -> String {
        if s.contains(',') || s.contains('"') || s.contains('\n') || s.contains('\r') {
            format!("\"{}\"", s.replace('"', "\"\""))
        } else {
            s.to_string()
        }
    }

    /// Split one CSV line honoring quoted fields.
    fn split_csv_line(line: &str) -> Vec<String> {
        let mut fields = Vec::new();
        let mut cur = String::new();
        let mut in_quotes = false;
        let mut chars = line.chars().peekable();
        while let Some(c) = chars.next() {
            if in_quotes {
                if c == '"' {
                    if chars.peek() == Some(&'"') {
                        cur.push('"');
                        chars.next();
                    } else {
                        in_quotes = false;
                    }
                } else {
                    cur.push(c);
                }
            } else if c == '"' {
                in_quotes = true;
            } else if c == ',' {
                fields.push(std::mem::take(&mut cur));
            } else {
                cur.push(c);
            }
        }
        fields.push(cur);
        fields
    }

    /// Import a NoteSet from CSV. `default_author` fills missing authors;
    /// `default_rate` (frames/sec) applies when the file has no rate column.
    ///
    /// # Errors
    /// One [`CsvError`] per bad row.
    pub fn import(
        csv_text: &str,
        default_author: &str,
        default_rate: i128,
    ) -> Result<NoteSet, Vec<CsvError>> {
        let mut lines = csv_text.lines().enumerate();
        let Some((_idx, header_line)) = lines.next() else {
            return Ok(NoteSet::default());
        };
        let header: Vec<String> = split_csv_line(header_line)
            .into_iter()
            .map(|h| h.trim().to_ascii_lowercase())
            .collect();
        let col = |names: &[&str]| -> Option<usize> {
            for n in names {
                if let Some(i) = header.iter().position(|h| h == n) {
                    return Some(i);
                }
            }
            None
        };
        // "Frame Number" (with its case variants) is the alias real tools emit
        let frame_col = col(&["frame", "frame number"]);
        let rate_col = col(&["rate", "fps"]);
        let clip_col = col(&["clip", "clip name", "clipname"]);
        let author_col = col(&["author", "reviewer", "user", "name"]);
        let status_col = col(&["status"]);
        let body_col = col(&["body", "comment", "note", "message", "text"]);
        let id_col = col(&["id", "note id"]);
        // v2 envelope columns (ADR-0028): absent in v1-era files
        let kind_col = col(&["kind", "type"]);
        let range_end_col = col(&["range_end", "end", "frame_end", "range end"]);
        let pin_x_col = col(&["pin_x", "pinx"]);
        let pin_y_col = col(&["pin_y", "piny"]);
        let attachment_col = col(&["attachment"]);
        let visibility_col = col(&["visibility", "audience"]);

        let mut errors = Vec::new();
        let mut set = NoteSet::default();
        for (i, line) in lines {
            if line.trim().is_empty() {
                continue;
            }
            let fields = split_csv_line(line);
            let get = |idx: Option<usize>| -> String {
                idx.and_then(|i| fields.get(i).cloned()).unwrap_or_default()
            };
            let frame: i128 = match get(frame_col).trim().parse() {
                Ok(v) => v,
                Err(_) => {
                    errors.push(CsvError {
                        line: i + 1,
                        reason: format!("bad frame: {}", get(frame_col)),
                    });
                    continue;
                }
            };
            let rate: i128 = get(rate_col).trim().parse().unwrap_or(default_rate);
            if rate <= 0 {
                errors.push(CsvError {
                    line: i + 1,
                    reason: format!("bad rate: {rate}"),
                });
                continue;
            }
            let kind = {
                let k = get(kind_col);
                if k.trim().is_empty() {
                    NoteKind::Comment
                } else {
                    NoteKind::parse(&k).unwrap_or_else(|| {
                        errors.push(CsvError {
                            line: i + 1,
                            reason: format!("bad kind: {k}"),
                        });
                        NoteKind::Comment
                    })
                }
            };
            let body = get(body_col);
            // v2: a pure marker (pin) may carry an empty body — its id
            // hashes the (possibly empty) body like any other note
            if body.trim().is_empty() && kind != NoteKind::Pin {
                errors.push(CsvError {
                    line: i + 1,
                    reason: "empty body".into(),
                });
                continue;
            }
            let author = {
                let a = get(author_col);
                if a.trim().is_empty() {
                    default_author.to_string()
                } else {
                    a
                }
            };
            let status = {
                let s = get(status_col);
                if s.trim().is_empty() {
                    NoteStatus::Open
                } else {
                    NoteStatus::parse(&s).unwrap_or(NoteStatus::Open)
                }
            };
            let range_end: Option<i128> = get(range_end_col)
                .trim()
                .parse::<i128>()
                .ok()
                .filter(|&e| e != frame);
            if let Some(e) = range_end {
                if e < frame {
                    errors.push(CsvError {
                        line: i + 1,
                        reason: format!("range_end {e} before frame {frame}"),
                    });
                    continue;
                }
            }
            let pin = match (
                get(pin_x_col).trim().parse::<f32>().ok(),
                get(pin_y_col).trim().parse::<f32>().ok(),
            ) {
                (Some(x), Some(y)) => Some((x.clamp(0.0, 1.0), y.clamp(0.0, 1.0))),
                _ => None,
            };
            let attachment = {
                let a = get(attachment_col);
                (!a.trim().is_empty()).then_some(a)
            };
            let visibility = {
                let v = get(visibility_col);
                if v.trim().is_empty() {
                    NoteVisibility::Public
                } else {
                    NoteVisibility::parse(&v).unwrap_or(NoteVisibility::Public)
                }
            };
            let anchor = NoteAnchor {
                clip: {
                    let c = get(clip_col);
                    (!c.trim().is_empty()).then_some(c)
                },
                frame,
                rate,
                range: range_end.map(|e| (frame, e)),
            };
            let mut note = Note::with_envelope(
                author, body, anchor, status, 0, kind, pin, attachment, visibility,
            );
            // honor a provided id when it is well-formed (round-trip fidelity)
            let provided = get(id_col);
            if !provided.trim().is_empty() && provided.len() == 16 {
                note.id = provided;
            }
            set.notes.insert(note.id.clone(), note);
        }
        if errors.is_empty() {
            Ok(set)
        } else {
            Err(errors)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn anchor(frame: i128) -> NoteAnchor {
        NoteAnchor {
            clip: Some("clip-interview-01".into()),
            frame,
            rate: 24,
            range: None,
        }
    }
    fn note(author: &str, body: &str, frame: i128, ms: i64) -> Note {
        Note::new(author, body, anchor(frame), NoteStatus::Open, ms)
    }

    #[test]
    fn ids_are_content_derived_and_edit_sensitive() {
        let n1 = note("alice", "trim 2s off the top", 100, 1);
        let n2 = note("alice", "trim 2s off the top", 100, 2);
        let n3 = note("alice", "trim 3s off the top", 100, 1);
        let n4 = note("bob", "trim 2s off the top", 100, 1);
        assert_eq!(n1.id, n2.id, "created_ms is not part of identity");
        assert_ne!(n1.id, n3.id, "a body edit is a new note");
        assert_ne!(n1.id, n4.id, "author is part of identity");
        assert_eq!(n1.id.len(), 16);
    }

    #[test]
    fn union_of_disjoint_additions() {
        let base = NoteSet::default();
        let ours = NoteSet::from_notes([note("alice", "cut here", 10, 1)]);
        let theirs = NoteSet::from_notes([note("bob", "also here", 20, 2)]);
        let m = merge_notes(&base, &ours, &theirs);
        assert_eq!(m.merged.len(), 2);
        assert!(m.conflicts.is_empty());
    }

    #[test]
    fn identical_additions_converge() {
        let base = NoteSet::default();
        let n = note("alice", "same note", 30, 1);
        let ours = NoteSet::from_notes([n.clone()]);
        let theirs = NoteSet::from_notes([n]);
        let m = merge_notes(&base, &ours, &theirs);
        assert_eq!(m.merged.len(), 1);
        assert!(m.conflicts.is_empty());
    }

    /// THE deletion rule: unchanged-vs-delete → deletion WINS.
    #[test]
    fn unchanged_vs_delete_deletion_wins() {
        let n = note("alice", "stale note", 40, 1);
        let base = NoteSet::from_notes([n.clone()]);
        let ours = base.clone(); // untouched
        let theirs = NoteSet::default(); // deleted it
        let m = merge_notes(&base, &ours, &theirs);
        assert!(m.merged.is_empty(), "the human's deletion sticks");
        // mirror direction
        let m2 = merge_notes(&base, &theirs, &ours);
        assert!(m2.merged.is_empty());
    }

    /// Edit-vs-delete: the edit survives because it is a NEW id.
    #[test]
    fn edit_vs_delete_edit_survives() {
        let old = note("alice", "v1 wording", 50, 1);
        let mut base = NoteSet::default();
        base.notes.insert(old.id.clone(), old.clone());
        // ours EDITED (delete old + add new); theirs deleted old
        let new = note("alice", "v2 wording", 50, 2);
        let mut ours = NoteSet::default();
        ours.notes.insert(new.id.clone(), new.clone());
        let theirs = NoteSet::default();
        let m = merge_notes(&base, &ours, &theirs);
        assert_eq!(m.merged.len(), 1);
        assert!(m.merged.notes.contains_key(&new.id), "the edit survives");
        assert!(!m.merged.notes.contains_key(&old.id));
    }

    #[test]
    fn resolved_is_sticky_across_status_flips() {
        let n = note("alice", "fix the audio pop", 60, 1);
        let id = n.id.clone();
        let mut base = NoteSet::default();
        base.notes.insert(id.clone(), n);
        let mut ours = base.clone();
        ours.notes.get_mut(&id).unwrap().status = NoteStatus::Resolved;
        let theirs = base.clone(); // still Open
        let m = merge_notes(&base, &ours, &theirs);
        assert_eq!(
            m.merged.notes[&id].status,
            NoteStatus::Resolved,
            "one side resolving sticks"
        );
        // mirror
        let m2 = merge_notes(&base, &theirs, &ours);
        assert_eq!(m2.merged.notes[&id].status, NoteStatus::Resolved);
    }
    #[test]
    fn rejected_vs_resolved_is_a_conflict() {
        let n = note("alice", "hero shot too dark", 70, 1);
        let id = n.id.clone();
        let mut base = NoteSet::default();
        base.notes.insert(id.clone(), n);
        let mut ours = base.clone();
        ours.notes.get_mut(&id).unwrap().status = NoteStatus::Rejected;
        let mut theirs = base.clone();
        theirs.notes.get_mut(&id).unwrap().status = NoteStatus::Resolved;
        let m = merge_notes(&base, &ours, &theirs);
        assert_eq!(
            m.merged.len(),
            1,
            "content survives; the STATUS asks a human"
        );
        assert_eq!(m.conflicts.len(), 1);
        assert!(m.conflicts[0].reason.contains("editorial"));
    }

    #[test]
    fn same_anchor_same_author_two_bodies_conflicts() {
        // the real-world case: two answers to the same client note
        let a = note("alice", "raise the music 2dB", 80, 1);
        let b = note("alice", "raise the music 4dB", 80, 2);
        let mut ours = NoteSet::default();
        ours.notes.insert(a.id.clone(), a);
        let mut theirs = NoteSet::default();
        theirs.notes.insert(b.id.clone(), b);
        let m = merge_notes(&NoteSet::default(), &ours, &theirs);
        assert_eq!(m.merged.len(), 2, "nothing is dropped");
        assert_eq!(
            m.conflicts.len(),
            1,
            "but the editor is told about the fork"
        );
        assert_eq!(
            m.conflicts[0].reason,
            "same author answered the same anchor twice"
        );
    }

    #[test]
    fn different_authors_same_frame_is_not_a_conflict() {
        // two reviewers flagging the same moment is COLLABORATION, not a fork
        let a = note("alice", "music too loud", 90, 1);
        let b = note("bob", "also mix is muddy", 90, 2);
        let mut ours = NoteSet::default();
        ours.notes.insert(a.id.clone(), a);
        let mut theirs = NoteSet::default();
        theirs.notes.insert(b.id.clone(), b);
        let m = merge_notes(&NoteSet::default(), &ours, &theirs);
        assert_eq!(m.merged.len(), 2);
        assert!(m.conflicts.is_empty());
    }

    #[test]
    fn json_roundtrip_deterministic() {
        let set = NoteSet::from_notes([
            note("alice", "one", 1, 1),
            note("bob", "two, with comma", 2, 2),
            note("carol", "quote \" inside", 3, 3),
        ]);
        let bytes = set.to_json().unwrap();
        let back = NoteSet::from_json(&bytes).unwrap();
        assert_eq!(set, back);
        let bytes2 = back.to_json().unwrap();
        assert_eq!(bytes, bytes2, "byte-stable serialization");
    }

    #[test]
    fn csv_roundtrip_with_escapables() {
        let set = NoteSet::from_notes([
            note("alice", "trim, the top", 101, 1),
            note("bob dylan", "quote \"here\"", 202, 2),
            Note::new(
                "carol",
                "no clip anchor",
                NoteAnchor {
                    clip: None,
                    frame: 303,
                    rate: 25,
                    range: None,
                },
                NoteStatus::Resolved,
                3,
            ),
        ]);
        let csv_text = csv::export(&set);
        assert!(csv_text.starts_with(
            "id,frame,rate,timecode,clip,author,status,body,kind,range_end,pin_x,pin_y,attachment,visibility\n"
        ));
        let back = csv::import(&csv_text, "unknown", 24).unwrap();
        assert_eq!(back.len(), 3);
        // bodies survive the escape round-trip
        assert_eq!(
            back.notes
                .values()
                .find(|n| n.author == "alice")
                .unwrap()
                .body,
            "trim, the top"
        );
        assert_eq!(
            back.notes
                .values()
                .find(|n| n.author == "bob dylan")
                .unwrap()
                .body,
            "quote \"here\""
        );
        // status + rate survive
        let carol = back.notes.values().find(|n| n.author == "carol").unwrap();
        assert_eq!(carol.status, NoteStatus::Resolved);
        assert_eq!(carol.anchor.rate, 25);
        assert!(carol.anchor.clip.is_none());
    }

    /// The alias that real review tools emit (previous integration bug):
    /// `Frame Number` must import as the frame column.
    #[test]
    fn frame_number_column_alias() {
        let csv_text =
            "Frame Number,Author,Comment\n120,alice,cut on the action\n240,bob,add lower third\n";
        let set = csv::import(csv_text, "unknown", 24).unwrap();
        assert_eq!(set.len(), 2);
        let n = set
            .notes
            .values()
            .find(|n| n.author == "alice")
            .expect("alice's note");
        assert_eq!(n.anchor.frame, 120);
    }

    #[test]
    fn import_derives_ids_and_reports_bad_rows() {
        let csv_text = "frame,author,body\nx10,alice,good note\n20,bob,\n30,carol,ok\n";
        let err = csv::import(csv_text, "unknown", 24).unwrap_err();
        assert_eq!(err.len(), 2, "bad frame + empty body reported");
        assert_eq!(err[0].line, 2);
    }

    #[test]
    fn timecode_formatting() {
        assert_eq!(csv::timecode(0, 24), "00:00:00:00");
        assert_eq!(csv::timecode(23, 24), "00:00:00:23");
        assert_eq!(csv::timecode(24, 24), "00:00:01:00");
        assert_eq!(csv::timecode(25 * 24 + 5, 24), "00:00:25:05");
        assert_eq!(csv::timecode(3600 * 24, 24), "01:00:00:00");
        assert_eq!(csv::timecode(-3, 24), "-00:00:00:03");
    }

    // ---- note-shape v2 (ADR-0028) ---------------------------------------

    fn v2_anchor(frame: i128, range: Option<(i128, i128)>) -> NoteAnchor {
        NoteAnchor {
            clip: Some("clip-interview-01".into()),
            frame,
            rate: 24,
            range,
        }
    }

    /// Gate 1: the id partition. No v1 id ever equals a v2 id for the
    /// same anchor/body/author; v2 ids differ when kind/range/visibility
    /// differ; pin and attachment are identity-NEUTRAL (same id).
    #[test]
    fn v2_id_partition_and_identity_neutral_fields() {
        let v1 = note("alice", "fix this", 100, 1);
        let make = |kind: NoteKind,
                    range: Option<(i128, i128)>,
                    pin: Option<(f32, f32)>,
                    att: Option<String>,
                    vis: NoteVisibility| {
            Note::with_envelope(
                "alice",
                "fix this",
                v2_anchor(100, range),
                NoteStatus::Open,
                1,
                kind,
                pin,
                att,
                vis,
            )
        };
        let base = make(
            NoteKind::Comment,
            Some((100, 100)),
            None,
            None,
            NoteVisibility::Public,
        );
        // degenerate range [f,f] + defaults = the v1 shape, same id
        assert!(!base.is_v2());
        assert_eq!(base.id, v1.id, "degenerate envelope stays v1");

        let ranged = make(
            NoteKind::Comment,
            Some((100, 140)),
            None,
            None,
            NoteVisibility::Public,
        );
        let pin = make(
            NoteKind::Pin,
            None,
            Some((0.5, 0.5)),
            None,
            NoteVisibility::Public,
        );
        let att = make(
            NoteKind::Annotation,
            None,
            None,
            Some("ab".repeat(16)),
            NoteVisibility::Public,
        );
        let internal = make(
            NoteKind::Comment,
            None,
            None,
            None,
            NoteVisibility::Internal,
        );
        for n in [&ranged, &pin, &att, &internal] {
            assert!(n.is_v2());
            assert_ne!(n.id, v1.id, "v2 ids never collide with the v1 id");
        }
        // the v2 ids are pairwise distinct (kind/range/visibility differ)
        let pin_id = pin.id.clone();
        let att_id = att.id.clone();
        let ids = [ranged.id, pin_id.clone(), att_id.clone(), internal.id];
        for i in 0..ids.len() {
            for j in i + 1..ids.len() {
                assert_ne!(ids[i], ids[j]);
            }
        }
        // pin position and attachment do NOT change identity: moving a pin
        // is an edit to the same note, so the id survives
        let moved = make(
            NoteKind::Pin,
            None,
            Some((0.25, 0.75)),
            None,
            NoteVisibility::Public,
        );
        assert_eq!(moved.id, pin_id, "pin position is identity-neutral");
        let other_att = make(
            NoteKind::Annotation,
            None,
            None,
            Some("cd".repeat(16)),
            NoteVisibility::Public,
        );
        assert_eq!(other_att.id, att_id, "attachment hash is identity-neutral");
    }

    /// Gate 2: a v1 note PARSES with the default v2 envelope and keeps its
    /// id; and the v1 JSON form is byte-identical to the pre-v2 shape (no
    /// new keys leak into v1 files).
    #[test]
    fn v1_notes_parse_with_default_envelope_and_serialize_unchanged() {
        let n = note("alice", "legacy note", 42, 7);
        let json = serde_json::to_string_pretty(&n).unwrap();
        // exactly the eight v1 keys, nothing more
        let keys: Vec<&str> = ["id", "author", "body", "status", "anchor", "created_ms"].to_vec();
        for k in keys {
            assert!(json.contains(&format!("\"{k}\"")), "v1 key {k} present");
        }
        assert!(!json.contains("kind"));
        assert!(!json.contains("pin"));
        assert!(!json.contains("attachment"));
        assert!(!json.contains("visibility"));
        assert!(!json.contains("range"));
        let back: Note = serde_json::from_str(&json).unwrap();
        assert_eq!(back, n);
        assert!(!back.is_v2());
        assert_eq!(back.kind, NoteKind::Comment);
        assert_eq!(back.visibility, NoteVisibility::Public);
        assert!(back.pin.is_none());
        assert_eq!(back.anchor.effective_range(), (42, 42));
        assert_eq!(back.anchor.range_key(), "42:42@24");
        // byte-stable through the set serializer (the sidecar contract)
        let set = NoteSet::from_notes([n.clone()]);
        let bytes = set.to_json().unwrap();
        let back_set = NoteSet::from_json(&bytes).unwrap();
        assert_eq!(back_set, set);
        assert_eq!(bytes, back_set.to_json().unwrap());
    }

    /// Gate 2b: a v2 note survives serialize -> parse -> merge id-stable.
    #[test]
    fn v2_roundtrip_is_id_stable_through_merge() {
        let n = Note::with_envelope(
            "alice",
            "hold the middle section",
            v2_anchor(100, Some((100, 160))),
            NoteStatus::Open,
            5,
            NoteKind::Comment,
            None,
            None,
            NoteVisibility::Public,
        );
        let set = NoteSet::from_notes([n.clone()]);
        let bytes = set.to_json().unwrap();
        let back = NoteSet::from_json(&bytes).unwrap();
        assert_eq!(
            back.notes[&n.id], n,
            "id and envelope survive the round-trip"
        );
        // merge with an empty theirs: union keeps it unchanged
        let m = merge_notes(&NoteSet::default(), &set, &NoteSet::default());
        assert_eq!(m.merged.notes[&n.id], n);
        assert!(m.conflicts.is_empty());
    }

    /// Gate 3: a range note and a point note on one clip land in the same
    /// merge bucket (the conflict a human wants); distinct ranges are
    /// distinct notes that both survive.
    #[test]
    fn merge_buckets_range_and_point_notes_on_one_clip() {
        let point = note("alice", "tighten here", 100, 1);
        let ranged = Note::with_envelope(
            "alice",
            "tighten the whole beat",
            v2_anchor(100, Some((100, 180))),
            NoteStatus::Open,
            2,
            NoteKind::Comment,
            None,
            None,
            NoteVisibility::Public,
        );
        assert_eq!(point.anchor.key(), ranged.anchor.key(), "same merge bucket");
        let mut ours = NoteSet::default();
        ours.notes.insert(point.id.clone(), point);
        let mut theirs = NoteSet::default();
        theirs.notes.insert(ranged.id.clone(), ranged);
        let m = merge_notes(&NoteSet::default(), &ours, &theirs);
        assert_eq!(m.merged.len(), 2, "both survive as distinct notes");
        assert_eq!(
            m.conflicts.len(),
            1,
            "same author + same bucket + different bodies -> surfaced"
        );
        assert!(m.conflicts[0].reason.contains("same anchor"));

        // distinct ranges by the same author: both SURVIVE (distinct
        // ids); the fork is still surfaced — same author, same anchor
        let r2 = Note::with_envelope(
            "alice",
            "tighten the whole beat",
            v2_anchor(120, Some((120, 200))),
            NoteStatus::Open,
            3,
            NoteKind::Comment,
            None,
            None,
            NoteVisibility::Public,
        );
        let mut t2 = NoteSet::default();
        t2.notes.insert(r2.id.clone(), r2);
        let m2 = merge_notes(&NoteSet::default(), &t2, &theirs);
        assert_eq!(m2.merged.len(), 2, "distinct ranges: both survive");
        assert_eq!(
            m2.conflicts.len(),
            1,
            "same author + same anchor: fork surfaced"
        );
    }

    /// The v2 identity-neutral fields merge field-wise: one side moving a
    /// pin wins silently; both sides moving it differently surfaces a
    /// conflict instead of silently dropping either.
    #[test]
    fn pin_merges_field_wise() {
        let base_note = Note::with_envelope(
            "alice",
            "look here",
            v2_anchor(50, None),
            NoteStatus::Open,
            1,
            NoteKind::Pin,
            Some((0.5, 0.5)),
            None,
            NoteVisibility::Public,
        );
        let id = base_note.id.clone();
        let mut base = NoteSet::default();
        base.notes.insert(id.clone(), base_note.clone());

        // only THEIRS moved the pin -> theirs wins
        let ours = base.clone();
        let mut theirs = base.clone();
        theirs.notes.get_mut(&id).unwrap().pin = Some((0.25, 0.25));
        let m = merge_notes(&base, &ours, &theirs);
        assert_eq!(m.merged.notes[&id].pin, Some((0.25, 0.25)));
        assert!(m.conflicts.is_empty());

        // both moved it differently -> conflict, ours kept, nothing silent
        let mut ours2 = base.clone();
        ours2.notes.get_mut(&id).unwrap().pin = Some((0.9, 0.1));
        let m2 = merge_notes(&base, &ours2, &theirs);
        assert_eq!(m2.merged.notes[&id].pin, Some((0.9, 0.1)), "ours kept");
        assert_eq!(m2.conflicts.len(), 1);
        assert!(m2.conflicts[0].reason.contains("pin"));
    }

    /// An empty-body pin has a stable id (v1 required a body; v2 hashes
    /// the possibly-empty body the same way).
    #[test]
    fn empty_body_pin_has_stable_id() {
        let a = Note::with_envelope(
            "alice",
            "",
            v2_anchor(88, None),
            NoteStatus::Open,
            1,
            NoteKind::Pin,
            Some((0.5, 0.5)),
            None,
            NoteVisibility::Public,
        );
        let b = Note::with_envelope(
            "alice",
            "",
            v2_anchor(88, None),
            NoteStatus::Open,
            9,
            NoteKind::Pin,
            Some((0.5, 0.5)),
            None,
            NoteVisibility::Public,
        );
        assert!(a.is_v2());
        assert_eq!(a.id, b.id, "created_ms is not identity");
        assert_ne!(a.id.len(), 0);
    }

    /// Gate 5 (CSV half): the visibility filter and the v2 columns
    /// round-trip. `export_visible(Public)` is "what the client gets".
    #[test]
    fn csv_v2_columns_roundtrip_and_visibility_filter() {
        let public = note("alice", "client sees this", 10, 1);
        let internal = Note::with_envelope(
            "bob",
            "studio only: swap the ending",
            v2_anchor(20, Some((20, 60))),
            NoteStatus::Open,
            2,
            NoteKind::Comment,
            None,
            None,
            NoteVisibility::Internal,
        );
        let pin = Note::with_envelope(
            "carol",
            "",
            v2_anchor(30, None),
            NoteStatus::Open,
            3,
            NoteKind::Pin,
            Some((0.3, 0.7)),
            None,
            NoteVisibility::Public,
        );
        let set = NoteSet::from_notes([public.clone(), internal.clone(), pin.clone()]);

        let all = csv::export_visible(&set, None);
        assert_eq!(all.lines().count(), 4, "header + three rows");
        assert!(all.contains("internal"));
        assert!(
            all.contains("comment,60,"),
            "range_end column carries the 20..60 range"
        );

        let client = csv::export_visible(&set, Some(NoteVisibility::Public));
        assert!(!client.contains("studio only"), "internal never ships");
        assert!(client.contains("client sees this"));

        // round-trip: the v2 envelope re-derives the SAME ids (kind /
        // range / pin / visibility all survive; created_ms is the one
        // field CSV never carried)
        let back = csv::import(&all, "unknown", 24).unwrap();
        assert_eq!(back.len(), 3);
        for (id, orig) in [
            (&internal.id, &internal),
            (&pin.id, &pin),
            (&public.id, &public),
        ] {
            let n = &back.notes[id];
            assert_eq!(n.id, orig.id, "id re-derived identically");
            assert_eq!(n.kind, orig.kind);
            assert_eq!(n.visibility, orig.visibility);
            assert_eq!(n.pin, orig.pin);
            assert_eq!(n.attachment, orig.attachment);
            assert_eq!(n.anchor.range, orig.anchor.range);
            assert_eq!(n.body, orig.body);
        }
        assert_eq!(back.notes[&internal.id].anchor.range, Some((20, 60)));

        // an old-format CSV (v1 columns only) still imports
        let legacy = "frame,author,body\n10,alice,old note\n";
        let old = csv::import(legacy, "unknown", 24).unwrap();
        assert_eq!(old.len(), 1);
        assert_eq!(old.notes.values().next().unwrap().kind, NoteKind::Comment);
    }
}
