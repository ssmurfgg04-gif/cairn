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
/// (element uuid or name — the identity ladder's first rungs).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoteAnchor {
    /// Clip uuid / name-path key (identity ladder rung a/b), if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clip: Option<String>,
    /// Frame number (exact integer at `rate`).
    pub frame: i128,
    /// Frames per second the frame number counts in (e.g. 24, 25, 30000/1001).
    pub rate: i128,
}

impl NoteAnchor {
    /// The merge key: clip identity if present, else the frame.
    #[must_use]
    pub fn key(&self) -> String {
        match &self.clip {
            Some(c) => format!("clip:{c}"),
            None => format!("frame:{}@{}", self.frame, self.rate),
        }
    }
}

/// One review note.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Note {
    /// Content-derived id (blake3 of anchor‖body‖author, 16 hex chars).
    pub id: String,
    pub author: String,
    pub body: String,
    pub status: NoteStatus,
    pub anchor: NoteAnchor,
    pub created_ms: i64,
}

impl Note {
    /// Compute the content-derived id (blake3 of anchor‖body‖author).
    #[must_use]
    pub fn derive_id(anchor: &NoteAnchor, body: &str, author: &str) -> String {
        let mut material = Vec::new();
        material.extend_from_slice(anchor.key().as_bytes());
        material.push(0x1F);
        material.extend_from_slice(body.as_bytes());
        material.push(0x1F);
        material.extend_from_slice(author.as_bytes());
        let h = blake3::hash(&material);
        let mut out = String::with_capacity(16);
        for b in &h.as_bytes()[..8] {
            out.push_str(&format!("{b:02x}"));
        }
        out
    }

    /// Build a note (id derived from content).
    #[must_use]
    pub fn new(
        author: impl Into<String>,
        body: impl Into<String>,
        anchor: NoteAnchor,
        status: NoteStatus,
        created_ms: i64,
    ) -> Note {
        let author = author.into();
        let body = body.into();
        let id = Note::derive_id(&anchor, &body, &author);
        Note {
            id,
            author,
            body,
            status,
            anchor,
            created_ms,
        }
    }
}

/// An ordered set of notes (BTreeMap: deterministic serialization for free).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Clone, Debug, PartialEq, Eq)]
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
            // construction — only status/created can differ
            (Some(_base), Some(o), Some(t)) => {
                if o == t {
                    merged.notes.insert(o.id.clone(), o.clone());
                } else {
                    match merge_status(o.status, t.status) {
                        Ok(s) => {
                            let mut n = o.clone();
                            n.status = s;
                            n.created_ms = o.created_ms.max(t.created_ms);
                            merged.notes.insert(n.id.clone(), n);
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

// ---- CSV ---------------------------------------------------------------------

/// CSV export/import for review-tool interop (Frame.io-style exports).
///
/// Columns: `id,frame,clip,author,status,body` — plus a `timecode` column on
/// export for humans. Import accepts `Frame Number` as an alias for `frame`
/// (the header real review tools emit), `Timecode` for tc, and derives
/// missing ids from content. Rate defaults to 24 when no `rate` column.
pub mod csv {
    use super::{Note, NoteAnchor, NoteSet, NoteStatus};

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
    pub fn export(set: &NoteSet) -> String {
        let mut rows: Vec<&Note> = set.notes.values().collect();
        rows.sort_by(|a, b| {
            (a.anchor.frame, a.anchor.rate, &a.id).cmp(&(b.anchor.frame, b.anchor.rate, &b.id))
        });
        let mut out = String::from("id,frame,rate,timecode,clip,author,status,body\n");
        for n in rows {
            let clip = n.anchor.clip.as_deref().unwrap_or("");
            let tc = timecode(n.anchor.frame, n.anchor.rate);
            let body = csv_escape(&n.body);
            out.push_str(&format!(
                "{},{},{},{},{},{},{},{}\n",
                n.id,
                n.anchor.frame,
                n.anchor.rate,
                tc,
                csv_escape(clip),
                csv_escape(&n.author),
                n.status.as_str(),
                body
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
            let body = get(body_col);
            if body.trim().is_empty() {
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
            let anchor = NoteAnchor {
                clip: {
                    let c = get(clip_col);
                    (!c.trim().is_empty()).then_some(c)
                },
                frame,
                rate,
            };
            let mut note = Note::new(author, body, anchor, status, 0);
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
                },
                NoteStatus::Resolved,
                3,
            ),
        ]);
        let csv_text = csv::export(&set);
        assert!(csv_text.starts_with("id,frame,rate,timecode,clip,author,status,body\n"));
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
}
