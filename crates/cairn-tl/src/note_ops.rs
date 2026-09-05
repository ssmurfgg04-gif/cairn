//! The 3-step no-AI recipe (ADR-0023 §3): client notes → mechanical edit ops.
//!
//! Step 1 (already exists): the client pins a frame in the review portal —
//! `NoteAnchor { frame, rate }` is exact.
//! Step 2 (this module): the note BODY is read like a robot reading a
//! spreadsheet — keywords for the mechanical vocabulary only:
//!   "cut 2 seconds off the end"  → `TrimOut { 2s }`
//!   "delete this clip"           → `Delete`
//!   "replace with clip B"        → `Replace { "clip B" }`
//!   "make the audio quieter"     → `Gain { -3 dB }`
//! Step 3 (renderers): the ops are packaged as a changelist — JSON (the
//! authoritative, machine-applyable form), CMX3600 EDL, and FCP7 xmeml.
//!
//! THE LINE (drawn hard): "make it more cinematic" / "make it pop" parses to
//! NOTHING. The robot never guesses creative intent — those notes are
//! surfaced to the human with their timestamp, untouched. Parsing is
//! deterministic and side-effect-free: re-parsing the same body always
//! yields the same ops, so nothing needs storing.
//!
//! Applying is a SEPARATE, explicit act (`apply_changelist`): the editor
//! previews and confirms. Nothing here ever touches a file.

use crate::model::{Element, Kind, MediaKind, Timeline, TrackKind};
use crate::notes::{csv::timecode, NoteSet};
use crate::rational::Rational;
use serde::{Deserialize, Serialize};

/// One mechanical edit parsed from a client note. Magnitudes are carried as
/// human-facing decimal STRINGS (round-trippable through JSON) and parsed
/// back to exact rationals at apply time.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum MechOp {
    /// "cut/trim N (seconds|frames) off the end" — out point moves earlier.
    TrimOut {
        seconds: String,
    },
    TrimIn {
        seconds: String,
    },
    Delete,
    Replace {
        target: String,
    },
    Gain {
        db: String,
    },
}

/// Parse result for one note body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NoteParse {
    /// At least one sentence parsed to a mechanical op.
    Mechanical(Vec<MechOp>),
    /// Nothing mechanical found — creative/aesthetic; the human decides.
    Creative,
}

impl MechOp {
    /// The rational magnitude in seconds (trims) or dB (gain).
    fn seconds_of(&self) -> Option<Rational> {
        match self {
            MechOp::TrimOut { seconds, .. } | MechOp::TrimIn { seconds } => parse_rational(seconds),
            MechOp::Gain { db } => parse_rational(db),
            _ => None,
        }
    }

    /// One-line human summary (reports, portal chips, EDL comments).
    #[must_use]
    pub fn summary(&self) -> String {
        match self {
            MechOp::TrimOut { seconds, .. } => {
                format!("trim {}s off the end (out point)", seconds)
            }
            MechOp::TrimIn { seconds } => format!("trim {}s off the start (in point)", seconds),
            MechOp::Delete => "delete this clip".into(),
            MechOp::Replace { target } => format!("replace media with {target}"),
            MechOp::Gain { db } => format!("audio gain {db} dB"),
        }
    }
}

fn parse_rational(s: &str) -> Option<Rational> {
    let t = s.trim();
    if let Some(frac) = t.strip_prefix("num/") {
        // internal canonical form "num/123/1000" — never user-facing
        let mut it = frac.split('/');
        let n: i128 = it.next()?.parse().ok()?;
        let d: i128 = it.next()?.parse().ok()?;
        return Rational::new(n, d).ok();
    }
    if let Ok(v) = t.parse::<f64>() {
        if v.is_finite() {
            return crate::rational::f64_to_rational(v).ok();
        }
    }
    None
}

fn rat_string(r: Rational) -> String {
    // human-facing: integer when whole, else up to 3 decimals with trailing
    // zeros trimmed ("2", "1.5", "0.25"); the exact rational rides in JSON
    let f = r.to_f64_approx();
    if (f - f.round()).abs() < 1e-9 {
        format!("{}", f.round() as i64)
    } else {
        let s = format!("{f:.3}");
        let s = s.trim_end_matches('0');
        s.trim_end_matches('.').to_string()
    }
}

/// The full parsed changelist for a NoteSet.
#[derive(Clone, Debug, Default)]
pub struct Changelist {
    /// Notes with at least one mechanical op, with the parsed ops attached.
    pub mechanical: Vec<MechItem>,
    /// Notes the robot refused to interpret — creative call for the human.
    pub creative: Vec<CreativeItem>,
}

/// A mechanical note + its parsed ops + anchor.
#[derive(Clone, Debug)]
pub struct MechItem {
    pub note_id: String,
    pub author: String,
    pub frame: i128,
    pub rate: i128,
    pub body: String,
    pub ops: Vec<MechOp>,
    /// Sentences that did NOT parse (kept verbatim — no silent loss).
    pub remainder: Vec<String>,
}

/// A creative note — highlighted + timestamped, editor decides.
#[derive(Clone, Debug)]
pub struct CreativeItem {
    pub note_id: String,
    pub author: String,
    pub frame: i128,
    pub rate: i128,
    pub body: String,
}

impl Changelist {
    /// Build the changelist for a whole note set. Deterministic: note order
    /// is the BTreeMap order (byte-stable), parsing is pure and rate-aware
    /// (frame-unit magnitudes resolve at the note's anchor rate).
    #[must_use]
    pub fn from_notes(set: &NoteSet) -> Changelist {
        let mut out = Changelist::default();
        for note in set.notes.values() {
            match parse_note_at(&note.body, note.anchor.rate) {
                NoteParse::Mechanical(ops) if !ops.is_empty() => {
                    let remainder = creative_remainder(&note.body);
                    out.mechanical.push(MechItem {
                        note_id: note.id.clone(),
                        author: note.author.clone(),
                        frame: note.anchor.frame,
                        rate: note.anchor.rate,
                        body: note.body.clone(),
                        ops,
                        remainder,
                    });
                }
                _ => out.creative.push(CreativeItem {
                    note_id: note.id.clone(),
                    author: note.author.clone(),
                    frame: note.anchor.frame,
                    rate: note.anchor.rate,
                    body: note.body.clone(),
                }),
            }
        }
        out
    }

    /// The authoritative machine-applyable JSON form.
    #[must_use]
    pub fn to_json(&self, title: &str) -> serde_json::Value {
        serde_json::json!({
            "schema": "cairn-changelist/v1",
            "title": title,
            "mechanical": self.mechanical.iter().map(|m| serde_json::json!({
                "note_id": m.note_id,
                "author": m.author,
                "frame": m.frame,
                "rate": m.rate,
                "body": m.body,
                "ops": m.ops.iter().map(|op| serde_json::json!({
                    "kind": op_kind(op),
                    "summary": op.summary(),
                    // exact rational seconds/db as num/den strings
                    "seconds": op.seconds_of().map(rat_json),
                })).collect::<Vec<_>>(),
                "remainder": m.remainder,
            })).collect::<Vec<_>>(),
            "creative": self.creative.iter().map(|c| serde_json::json!({
                "note_id": c.note_id,
                "author": c.author,
                "frame": c.frame,
                "rate": c.rate,
                "body": c.body,
                "timecode": timecode(c.frame, c.rate),
            })).collect::<Vec<_>>(),
        })
    }
}

fn rat_json(r: Rational) -> serde_json::Value {
    serde_json::json!({
        "num": r.num,
        "den": r.den,
    })
}

fn op_kind(op: &MechOp) -> &'static str {
    match op {
        MechOp::TrimOut { .. } => "trim_out",
        MechOp::TrimIn { .. } => "trim_in",
        MechOp::Delete => "delete",
        MechOp::Replace { .. } => "replace",
        MechOp::Gain { .. } => "gain",
    }
}

/// Parse a note body at the note's frame rate — frame-unit amounts
/// ("cut 12 frames off the end") resolve to exact seconds here. Sentence-split;
/// each sentence may yield one op; any sentence that does not parse is kept
/// (not silently dropped) — the caller decides how to surface it. A body with
/// zero mechanical sentences is Creative.
#[must_use]
pub fn parse_note_at(body: &str, rate: i128) -> NoteParse {
    let mut ops = Vec::new();
    for sentence in split_sentences(body) {
        if let Some(op) = parse_sentence_at(&sentence, rate) {
            ops.push(op);
        }
        if ops.len() >= 8 {
            break; // bounded: a note is not an essay
        }
    }
    if ops.is_empty() {
        NoteParse::Creative
    } else {
        NoteParse::Mechanical(ops)
    }
}

/// Rate-free parse: frame-unit magnitudes cannot resolve without a rate and
/// are left unparsed (the honest refusal, not a guess).
#[must_use]
pub fn parse_note(body: &str) -> NoteParse {
    parse_note_at(body, 0)
}

fn creative_remainder(body: &str) -> Vec<String> {
    split_sentences(body)
        .into_iter()
        .filter(|s| parse_sentence(s).is_none())
        .collect()
}

/// Rate-aware sentence parse: frame amounts convert via `rate`.
fn parse_sentence_at(sentence: &str, rate: i128) -> Option<MechOp> {
    let base = parse_sentence(sentence);
    if let Some(op) = &base {
        return Some(op.clone());
    }
    // frame-unit magnitude: "cut 12 frames off the end"
    let s = sentence.to_lowercase();
    if !s.split_whitespace().any(|w| w == "frame" || w == "frames") {
        return None;
    }
    let mut words: Vec<&str> = s.split_whitespace().collect();
    // strip the frame words so find_amount sees the number
    words.retain(|w| *w != "frame" && *w != "frames");
    if !starts_with_any(
        &words,
        &["cut", "trim", "shorten", "shave", "chop", "take", "tighten"],
    ) {
        return None;
    }
    let (n, _) = find_amount(&words)?;
    let rate_r = Rational::new(rate.max(1), 1).ok()?;
    let secs = n.checked_div(rate_r).ok()?;
    let secs_s = rat_string(secs);
    let head = contains_any(
        &words,
        &["start", "beginning", "front", "head", "top", "in"],
    ) && !contains_any(&words, &["end", "tail", "back", "out"]);
    if head {
        Some(MechOp::TrimIn { seconds: secs_s })
    } else {
        Some(MechOp::TrimOut { seconds: secs_s })
    }
}

/// Sentence-split that does NOT break decimal numbers: '.' between two
/// digits is a decimal point, not a sentence end ("trim 1.5 seconds" must
/// survive intact). Newlines, '!', '?', ';' always split.
fn split_sentences(body: &str) -> Vec<String> {
    let chars: Vec<char> = body.chars().collect();
    let mut out = Vec::new();
    let mut cur = String::new();
    for (i, &c) in chars.iter().enumerate() {
        let is_decimal_dot = c == '.' && {
            let prev_digit = i > 0 && chars[i - 1].is_ascii_digit();
            let next_digit = i + 1 < chars.len() && chars[i + 1].is_ascii_digit();
            prev_digit && next_digit
        };
        if (c == '.' && !is_decimal_dot) || c == '!' || c == '?' || c == ';' || c == '\n' {
            if !cur.trim().is_empty() {
                out.push(cur.trim().to_string());
            }
            cur.clear();
        } else {
            cur.push(c);
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

/// The robot's keyword table. One sentence → at most one op. Anything the
/// table cannot decide is `None` — the creative line.
fn parse_sentence(sentence: &str) -> Option<MechOp> {
    let s = sentence.to_lowercase();
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let words: Vec<&str> = s.split_whitespace().collect();

    // ---- replace ----
    if starts_with_any(&words, &["replace", "swap", "substitute"]) {
        let idx = find_word(&words, &["with", "for"])?;
        let target: Vec<&str> = words[idx + 1..].to_vec();
        // drop trailing noise ("instead", "please", "thanks")
        let mut target: String = target.join(" ");
        for noise in [" please", " thanks", " thank you", " instead"] {
            if target.ends_with(noise) {
                let n = target.len() - noise.len();
                target.truncate(n);
            }
        }
        let target = target.trim().trim_end_matches('.').trim().to_string();
        if target.is_empty() {
            return None; // "replace it with..." without a target — creative
        }
        return Some(MechOp::Replace { target });
    }

    // ---- delete ----
    if starts_with_any(&words, &["delete", "remove", "kill", "drop"])
        && contains_any(&words, &["this", "the", "it"])
        && !contains_any(
            &words,
            &["second", "seconds", "sec", "secs", "frame", "frames"],
        )
    {
        return Some(MechOp::Delete);
    }
    if contains_seq(&words, &["cut", "out"]) && !contains_number(&words) {
        // "cut this clip out"
        if contains_any(&words, &["this", "the"]) {
            return Some(MechOp::Delete);
        }
    }

    // ---- gain ----
    if let Some(up) = gain_direction(&words) {
        let db = explicit_db(&words).unwrap_or(3);
        let mag = Rational::new(i128::from(db * if up { 1 } else { -1 }), 1).ok()?;
        return Some(MechOp::Gain {
            db: rat_string(mag),
        });
    }

    // ---- trim (cut/trim/shorten ... N ... off [the] end|start) ----
    if starts_with_any(
        &words,
        &["cut", "trim", "shorten", "shave", "chop", "take", "tighten"],
    ) || contains_seq(&words, &["cut", "off"])
        || contains_seq(&words, &["trim", "off"])
    {
        let (amount, frames) = find_amount(&words)?;
        if frames {
            // frame-unit magnitudes need the note's rate — see parse_sentence_at
            return None;
        }
        let head = contains_any(
            &words,
            &["start", "beginning", "front", "head", "top", "in"],
        ) && !contains_any(&words, &["end", "tail", "back", "out"]);
        if head {
            return Some(MechOp::TrimIn {
                seconds: rat_string(amount),
            });
        }
        // default: the end (the overwhelmingly common client intent —
        // "cut 2 seconds" means off the tail)
        return Some(MechOp::TrimOut {
            seconds: rat_string(amount),
        });
    }

    None
}

#[allow(clippy::needless_pass_by_value)]
fn starts_with_any(words: &[&str], heads: &[&str]) -> bool {
    words.first().is_some_and(|w| heads.contains(w))
}
fn contains_any(words: &[&str], needles: &[&str]) -> bool {
    words.iter().any(|w| needles.contains(w))
}
fn contains_seq(words: &[&str], seq: &[&str]) -> bool {
    'outer: for start in 0..words.len().saturating_sub(seq.len() - 1) {
        for (i, n) in seq.iter().enumerate() {
            if words[start + i] != *n {
                continue 'outer;
            }
        }
        return true;
    }
    false
}
fn find_word(words: &[&str], needles: &[&str]) -> Option<usize> {
    words.iter().position(|w| needles.contains(w))
}

/// Word-number vocabulary (robot-grade, no cleverness).
fn word_number(w: &str) -> Option<i64> {
    match w {
        "half" | "1/2" => Some(0), // handled specially (returns 1/2 below)
        "quarter" => Some(0),
        "one" => Some(1),
        "two" => Some(2),
        "three" => Some(3),
        "four" => Some(4),
        "five" => Some(5),
        "six" => Some(6),
        "seven" => Some(7),
        "eight" => Some(8),
        "nine" => Some(9),
        "ten" => Some(10),
        _ => None,
    }
}

fn contains_number(words: &[&str]) -> bool {
    words.iter().any(|w| {
        w.parse::<f64>().is_ok() || word_number(w).is_some() || *w == "half" || *w == "quarter"
    })
}

/// Find the magnitude: a number (digits/decimal), a word number, "half",
/// "a quarter". Returns (rational, is_frames).
fn find_amount(words: &[&str]) -> Option<(Rational, bool)> {
    let is_frames = contains_any(words, &["frame", "frames"]);
    for w in words {
        if let Ok(v) = w.trim_matches(',').parse::<f64>() {
            if v.is_finite() && v > 0.0 {
                return crate::rational::f64_to_rational(v)
                    .ok()
                    .map(|r| (r, is_frames));
            }
        }
        match *w {
            "half" => {
                return Rational::new(1, 2).ok().map(|r| (r, is_frames));
            }
            "quarter" => {
                return Rational::new(1, 4).ok().map(|r| (r, is_frames));
            }
            _ => {}
        }
        if let Some(n) = word_number(w) {
            if n > 0 {
                return Rational::new(i128::from(n), 1).ok().map(|r| (r, is_frames));
            }
        }
    }
    None
}

fn gain_direction(words: &[&str]) -> Option<bool> {
    if contains_any(words, &["quieter", "softer", "quiet", "duck", "tone"])
        || contains_seq(words, &["turn", "down"])
        || contains_seq(words, &["lower", "the"])
        || (contains_any(words, &["lower"])
            && contains_any(words, &["audio", "music", "sound", "volume", "levels"]))
    {
        return Some(false);
    }
    if contains_any(words, &["louder", "boost", "pump"])
        || contains_seq(words, &["turn", "up"])
        || contains_seq(words, &["raise", "the"])
    {
        return Some(true);
    }
    None
}

fn explicit_db(words: &[&str]) -> Option<i64> {
    // "by 6 db" / "-3db" / "6 decibels"
    for (i, w) in words.iter().enumerate() {
        if *w == "db" || *w == "dbs" || *w == "decibels" || w.ends_with("db") {
            if let Some(n) = i.checked_sub(1) {
                let cand = words[n].trim_start_matches('-');
                if let Ok(v) = cand.parse::<f64>() {
                    if v.is_finite() && v > 0.0 && v <= 60.0 {
                        let neg = words[n].starts_with('-');
                        return Some(if neg { -(v as i64) } else { v as i64 });
                    }
                }
            }
            let stripped = w.trim_end_matches("db");
            if let Ok(v) = stripped.parse::<f64>() {
                if v.is_finite() && v > 0.0 && v <= 60.0 {
                    return Some(v as i64);
                }
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Apply: ops → a NEW timeline (never in-place; the caller writes the file,
// and only after the editor explicitly confirmed).
// ---------------------------------------------------------------------------

/// Per-item apply outcome.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApplyStatus {
    Applied,
    /// The op could not be resolved mechanically (no clip at that frame,
    /// un-timestamped track layout, no media to replace). Recorded, never
    /// silently dropped — the editor handles these by hand.
    Unresolved(String),
}

#[derive(Clone, Debug)]
pub struct AppliedItem {
    pub note_id: String,
    pub summary: String,
    pub status: ApplyStatus,
}

/// Apply a changelist to a timeline. Returns the new timeline plus the
/// per-item ledger. Items resolve in frame order (deterministic).
///
/// TWO-PASS, identity-based (the bug this prevents: a trim shifts every
/// downstream clip, so resolving later notes against the MID-APPLY timeline
/// mis-targets them — the merge engine's "ops follow their element" rule,
/// applied here):
///   pass 1: resolve each note's frame → element REFERENCE (uuid or name)
///           against the SOURCE timeline, exactly as the client saw it;
///   pass 2: apply the ops to the working copy by reference — trims can
///           shift positions freely, the targets never drift.
pub fn apply_changelist(tl: &Timeline, items: &[MechItem]) -> (Timeline, Vec<AppliedItem>) {
    let mut out = tl.clone();
    let mut ledger = Vec::new();
    // frame order, then note id — stable regardless of NoteSet iteration
    let mut ordered: Vec<&MechItem> = items.iter().collect();
    ordered.sort_by(|a, b| (a.frame, &a.note_id).cmp(&(b.frame, &b.note_id)));
    // pass 1: pin each item's target against the SOURCE doc
    let mut pinned: Vec<(&MechItem, Option<String>)> = Vec::new();
    for item in &ordered {
        let target = element_at(tl, item.frame, item.rate)
            .map(|(ti, ii)| el_ref(&tl.tracks.children[ti].children[ii]));
        pinned.push((item, target));
    }
    // pass 2: apply by reference on the working copy
    for (item, target) in pinned {
        for op in &item.ops {
            let summary = format!("{} — {}", op.summary(), item.body);
            let status = match &target {
                None => {
                    ApplyStatus::Unresolved("no clip at that frame (in the published cut)".into())
                }
                Some(r) => apply_one(&mut out, op, r),
            };
            ledger.push(AppliedItem {
                note_id: item.note_id.clone(),
                summary,
                status,
            });
        }
    }
    (out, ledger)
}

/// Stable element reference: uuid when stamped (rung a), else name (rung b).
fn el_ref(el: &Element) -> String {
    match el.cairn_uuid() {
        Some(u) => format!("uuid:{u}"),
        None => format!("name:{}", el.name),
    }
}

/// Find an element by the reference form of [`el_ref`] and apply the op to
/// it, wherever it currently lives. `None` = the element is gone (deleted by
/// an earlier op — recorded, never silent).
fn apply_one(tl: &mut Timeline, op: &MechOp, target: &str) -> ApplyStatus {
    // locate by reference on the CURRENT doc (identity follows the element)
    let found = find_by_ref(tl, target);
    let Some((t_idx, i_idx)) = found else {
        return ApplyStatus::Unresolved("clip was removed by an earlier op".into());
    };
    match op {
        MechOp::Delete => {
            tl.tracks.children[t_idx].children.remove(i_idx);
            ApplyStatus::Applied
        }
        MechOp::TrimOut { seconds, .. } | MechOp::TrimIn { seconds } => {
            let Some(n) = parse_rational(seconds) else {
                return ApplyStatus::Unresolved(format!("unparseable magnitude {seconds:?}"));
            };
            let el = &mut tl.tracks.children[t_idx].children[i_idx];
            trim_element(el, matches!(op, MechOp::TrimIn { .. }), n)
        }
        MechOp::Replace { target } => {
            let el = &mut tl.tracks.children[t_idx].children[i_idx];
            let Some(media) = el.media.as_mut() else {
                return ApplyStatus::Unresolved("element has no media to replace".into());
            };
            // Clip.2: replace the ACTIVE entry; legacy Clip.1: the single ref
            if let Some(entry) = media.references.get_mut(&media.active_key) {
                entry.target_url = Some(target.clone());
            } else {
                media.target_url = Some(target.clone());
                media.kind = MediaKind::External;
            }
            ApplyStatus::Applied
        }
        MechOp::Gain { db } => {
            let Some(n) = parse_rational(db) else {
                return ApplyStatus::Unresolved(format!("unparseable gain {db:?}"));
            };
            let el = &mut tl.tracks.children[t_idx].children[i_idx];
            el.effects.push(crate::model::Effect {
                schema: "Effect.1".into(),
                name: "cairn gain".into(),
                effect_name: "Gain".into(),
                enabled: true,
                metadata: {
                    let mut m = crate::model::JsonMap::new();
                    m.insert(
                        "cairn/gain-db".into(),
                        serde_json::json!({ "num": n.num, "den": n.den }),
                    );
                    m
                },
                extra: crate::model::JsonMap::new(),
            });
            ApplyStatus::Applied
        }
    }
}

/// Find a top-level track item by uuid-or-name reference.
fn find_by_ref(tl: &Timeline, reference: &str) -> Option<(usize, usize)> {
    for (ti, tr) in tl.tracks.children.iter().enumerate() {
        if !matches!(tr.kind, Kind::Track(_)) {
            continue;
        }
        for (ii, item) in tr.children.iter().enumerate() {
            if el_ref(item) == reference {
                return Some((ti, ii));
            }
        }
    }
    None
}

fn trim_element(el: &mut Element, head: bool, amount: Rational) -> ApplyStatus {
    // effective source range: source_range, else media available_range
    let eff_start_secs;
    let eff_dur_secs;
    if let Some(r) = &el.source_range {
        let Ok(s) = r.start.seconds() else {
            return ApplyStatus::Unresolved("source range start unrepresentable".into());
        };
        let Ok(d) = r.duration.seconds() else {
            return ApplyStatus::Unresolved("source range duration unrepresentable".into());
        };
        eff_start_secs = s;
        eff_dur_secs = d;
    } else if let Some(av) = el.media.as_ref().and_then(|m| m.available_range.as_ref()) {
        let Ok(s) = av.start.seconds() else {
            return ApplyStatus::Unresolved("available range start unrepresentable".into());
        };
        let Ok(d) = av.duration.seconds() else {
            return ApplyStatus::Unresolved("available range duration unrepresentable".into());
        };
        eff_start_secs = s;
        eff_dur_secs = d;
    } else {
        return ApplyStatus::Unresolved(
            "clip has no source/available range — trim cannot be computed".into(),
        );
    }

    let (new_start, new_dur) = if head {
        let start = eff_start_secs.checked_add(amount);
        let dur = eff_dur_secs.checked_sub(amount);
        (start, dur)
    } else {
        let dur = eff_dur_secs.checked_sub(amount);
        (Ok(eff_start_secs), dur)
    };
    let (Ok(new_start), Ok(new_dur)) = (new_start, new_dur) else {
        return ApplyStatus::Unresolved("trim magnitude out of rational range".into());
    };
    if new_dur.is_zero() {
        return ApplyStatus::Unresolved(
            "trim exceeds clip duration — clamped by the boundary, editor decides".into(),
        );
    }
    // a negative duration would flip the clip — refuse mechanically
    if new_dur.num < 0 {
        return ApplyStatus::Unresolved("trim would invert the clip (duration < 0)".into());
    }

    // write back: preserve the original range's rate
    let rate = el.source_range.as_ref().map(|r| r.start.rate).or_else(|| {
        el.media
            .as_ref()
            .and_then(|m| m.available_range.as_ref())
            .map(|r| r.start.rate)
    });
    let Some(rate) = rate else {
        return ApplyStatus::Unresolved("no rate to express the new range in".into());
    };
    let Ok(start_tv) = crate::model::TimeVal::from_seconds(new_start, rate) else {
        return ApplyStatus::Unresolved("new start unrepresentable".into());
    };
    let Ok(dur_tv) = crate::model::TimeVal::from_seconds(new_dur, rate) else {
        return ApplyStatus::Unresolved("new duration unrepresentable".into());
    };
    el.source_range = Some(crate::model::TimeRange {
        start: start_tv,
        duration: dur_tv,
    });
    ApplyStatus::Applied
}

/// Find the (track index, item index) of the element covering `frame` at
/// `rate` on the first track that resolves. Video tracks preferred, then
/// audio, then any. Track layout must be fully duration-typed — an
/// unknown-duration item makes positions after it unknowable, so that track
/// is skipped (honest refusal, no guessing).
fn element_at(tl: &Timeline, frame: i128, rate: i128) -> Option<(usize, usize)> {
    let Ok(t) = Rational::new(frame, rate) else {
        return None;
    };
    let mut by_kind: Vec<(u8, usize)> = Vec::new(); // (kind rank, track idx)
    for (i, tr) in tl.tracks.children.iter().enumerate() {
        let rank = match tr.kind {
            Kind::Track(TrackKind::Video) => 0,
            Kind::Track(TrackKind::Audio) => 1,
            Kind::Track(TrackKind::Subtitle) => 2,
            _ => 3,
        };
        by_kind.push((rank, i));
    }
    by_kind.sort_unstable();
    for (_, ti) in by_kind {
        let mut pos = Rational::ZERO;
        for (ii, item) in tl.tracks.children[ti].children.iter().enumerate() {
            let Some(d) = item_duration_secs(item) else {
                // position math breaks after this item — try next track
                break;
            };
            let end = pos.checked_add(d).ok()?;
            if t.cmp_exact(pos) != std::cmp::Ordering::Less
                && t.cmp_exact(end) == std::cmp::Ordering::Less
            {
                // gaps have no media to edit — but a delete on a gap is still
                // legal; the caller's op decides. Only skip contentless gaps
                // for TRIM/Gain/Replace by checking has_content there.
                return Some((ti, ii));
            }
            pos = end;
        }
    }
    None
}

fn item_duration_secs(el: &Element) -> Option<Rational> {
    if let Some(r) = &el.source_range {
        return r.duration.seconds().ok();
    }
    if let Some(av) = el.media.as_ref().and_then(|m| m.available_range.as_ref()) {
        return av.duration.seconds().ok();
    }
    // Gap with no range: OTIO treats these as zero-length in this model —
    // unknowable, honestly refused.
    None
}

// ---------------------------------------------------------------------------
// EDL (CMX3600) rendering — the cut-list interchange form.
// ---------------------------------------------------------------------------

/// Render the mechanical items as a CMX3600-style EDL at the first note's
/// rate. Events carry the record span (timeline position) of the affected
/// clip; comments carry the note. Creative notes ride as `* COMMENT` blocks
/// so nothing is lost in the export.
#[must_use]
pub fn changelist_edl(cl: &Changelist, title: &str) -> String {
    let rate = cl
        .mechanical
        .first()
        .map(|m| m.rate)
        .or_else(|| cl.creative.first().map(|c| c.rate))
        .unwrap_or(24)
        .max(1);
    let mut out = String::new();
    out.push_str(&format!("TITLE: {title}\n"));
    out.push_str("* CAIRN CHANGE LIST V1 — mechanical client notes, frame-anchored\n");
    let mut ev = 0u32;
    for m in &cl.mechanical {
        ev += 1;
        let tc = timecode(m.frame, rate);
        for op in &m.ops {
            out.push_str(&format!("{ev:03}  AX       C        {tc} {tc} {tc} {tc}\n"));
            out.push_str(&format!(
                "* OP: {} (note {}, {})\n",
                op.summary(),
                m.note_id,
                m.author
            ));
            out.push_str(&format!("* FROM CLIP NAME: {}\n", m.body));
        }
    }
    for c in &cl.creative {
        ev += 1;
        let tc = timecode(c.frame, rate);
        out.push_str(&format!("{ev:03}  AX       C        {tc} {tc} {tc} {tc}\n"));
        out.push_str(&format!(
            "* CREATIVE NOTE (frame {tc}): {} — {}\n",
            c.author, c.body
        ));
    }
    out
}

// ---------------------------------------------------------------------------
// FCP7 xmeml rendering — markers the NLE can import and pin.
// ---------------------------------------------------------------------------

/// Render the changelist as an FCP7 XML sequence whose markers carry the
/// mechanical ops (importable by Premiere/Resolve/FCP — same posture as
/// `markers::notes_to_fcpxml`: review-in-NLE, apply via the JSON form).
#[must_use]
pub fn changelist_fcpxml(
    cl: &Changelist,
    timebase: i64,
    ntsc: bool,
    sequence_name: &str,
) -> String {
    let esc = |s: &str| {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
            .replace('\'', "&apos;")
    };
    let mut out = String::new();
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.push_str("<!DOCTYPE xmeml>\n");
    out.push_str("<xmeml version=\"4\">\n");
    out.push_str(&format!(
        "<sequence id=\"cairn-changelist\">\n<name>{}</name>\n<rate><timebase>{}</timebase><ntsc>{}</ntsc></rate>\n",
        esc(sequence_name),
        timebase,
        if ntsc { "TRUE" } else { "FALSE" }
    ));
    let mut items: Vec<(&MechItem, &MechOp)> = cl
        .mechanical
        .iter()
        .flat_map(|m| m.ops.iter().map(move |op| (m, op)))
        .collect();
    items.sort_by_key(|(m, _)| (m.frame, std::cmp::Reverse(0))); // frame order
    for (m, op) in items {
        let frame = m.frame.max(0);
        out.push_str(&format!(
            "<marker>\n<comment>[ACTION] {}</comment>\n<name>{} · {}</name>\n<start>{}</start>\n<duration>1</duration>\n</marker>\n",
            esc(&op.summary()),
            esc(&m.author),
            esc(&m.body),
            frame
        ));
    }
    for c in &cl.creative {
        let frame = c.frame.max(0);
        out.push_str(&format!(
            "<marker>\n<comment>[CREATIVE] {}</comment>\n<name>{} · {}</name>\n<start>{}</start>\n<duration>1</duration>\n</marker>\n",
            esc(&c.body),
            esc(&c.author),
            esc(&c.body),
            frame
        ));
    }
    out.push_str("</sequence>\n</xmeml>\n");
    out
}

/// Convenience: build the changelist from a NoteSet (the CLI's one call).
#[must_use]
pub fn from_noteset(set: &NoteSet) -> Changelist {
    Changelist::from_notes(set)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notes::{Note, NoteAnchor, NoteStatus};

    fn note(body: &str, frame: i128) -> Note {
        Note::new(
            "client",
            body,
            NoteAnchor {
                clip: None,
                frame,
                rate: 24,
                range: None,
            },
            NoteStatus::Open,
            1700000000,
        )
    }

    #[test]
    fn robot_reads_the_spreadsheet() {
        // the exact table from the recipe
        let cases: &[(&str, MechOp)] = &[
            (
                "Cut 2 seconds off the end",
                MechOp::TrimOut {
                    seconds: "2".into(),
                },
            ),
            (
                "Cut 2 seconds off the end of this shot",
                MechOp::TrimOut {
                    seconds: "2".into(),
                },
            ),
            (
                "Trim 1.5 seconds off the end",
                MechOp::TrimOut {
                    seconds: "1.5".into(),
                },
            ),
            (
                "cut half a second off the start",
                MechOp::TrimIn {
                    seconds: "0.5".into(),
                },
            ),
            ("Delete this clip", MechOp::Delete),
            ("Remove the clip", MechOp::Delete),
            (
                "Replace with clip B",
                MechOp::Replace {
                    target: "clip b".into(),
                },
            ),
            (
                "Make the audio quieter here",
                MechOp::Gain { db: "-3".into() },
            ),
            ("lower the music by 6 db", MechOp::Gain { db: "-6".into() }),
            ("boost the audio 4db", MechOp::Gain { db: "4".into() }),
        ];
        for (body, want) in cases {
            let got = parse_note(body);
            let NoteParse::Mechanical(ops) = got else {
                panic!("`{body}` parsed as creative — expected mechanical");
            };
            assert_eq!(ops.len(), 1, "one op for `{body}`");
            assert_eq!(&ops[0], want, "body: {body}");
        }
    }

    #[test]
    fn the_creative_line_is_hard() {
        for body in [
            "make it more cinematic",
            "make it pop",
            "needs more energy",
            "i don't know, something feels off",
            "",
            "replace with",
        ] {
            assert_eq!(parse_note(body), NoteParse::Creative, "body: `{body}`");
        }
    }

    #[test]
    fn multi_sentence_notes_split() {
        let p = parse_note("Cut 2 seconds off the end. Also make the audio quieter.");
        let NoteParse::Mechanical(ops) = p else {
            panic!("expected mechanical");
        };
        assert_eq!(ops.len(), 2);
        assert!(matches!(ops[0], MechOp::TrimOut { .. }));
        assert!(matches!(ops[1], MechOp::Gain { .. }));
    }

    #[test]
    fn changelist_splits_and_keeps_remainder() {
        let set = NoteSet::from_notes([
            note("Cut 2 seconds off the end. Make it pop.", 240),
            note("more cinematic please", 480),
        ]);
        let cl = Changelist::from_notes(&set);
        assert_eq!(cl.mechanical.len(), 1);
        assert_eq!(cl.creative.len(), 1);
        assert_eq!(
            cl.mechanical[0].remainder,
            vec!["Make it pop".to_string()],
            "unparsed sentence kept verbatim"
        );
        // JSON is self-describing
        let j = cl.to_json("v3");
        assert_eq!(j["schema"], "cairn-changelist/v1");
        assert_eq!(j["mechanical"][0]["frame"], 240);
        assert_eq!(j["creative"][0]["frame"], 480);
    }

    #[test]
    fn edl_and_fcpxml_render() {
        let set = NoteSet::from_notes([
            note("Cut 2 seconds off the end", 240),
            note("make it pop", 480),
        ]);
        let cl = Changelist::from_notes(&set);
        let edl = changelist_edl(&cl, "v3 review");
        assert!(edl.contains("TITLE: v3 review"));
        assert!(edl.contains("* OP: trim 2s off the end"));
        assert!(edl.contains("* CREATIVE NOTE"));
        let xml = changelist_fcpxml(&cl, 24, false, "v3 review");
        assert!(xml.contains("<xmeml version=\"4\">"));
        assert!(xml.contains("[ACTION] trim 2s off the end (out point)"));
        assert!(xml.contains("[CREATIVE] make it pop"));
    }

    #[test]
    fn apply_trims_delete_and_refuses_gracefully() {
        use crate::model::*;
        // 3 clips of 2s each at 24fps on one video track
        let mk = |name: &str| {
            let mut e = Element::leaf(Kind::Clip, name);
            e.source_range = Some(TimeRange {
                start: TimeVal {
                    value: Rational::new(0, 1).unwrap(),
                    rate: Rational::new(24, 1).unwrap(),
                },
                duration: TimeVal {
                    value: Rational::new(48, 1).unwrap(),
                    rate: Rational::new(24, 1).unwrap(),
                },
            });
            e
        };
        let track = Element::container(
            Kind::Track(TrackKind::Video),
            "V1",
            vec![mk("a"), mk("b"), mk("c")],
        );
        let tl = Timeline {
            name: "t".into(),
            global_start_time: None,
            metadata: crate::model::JsonMap::new(),
            tracks: Element::container(Kind::Stack, "tracks", vec![track]),
            extra: crate::model::JsonMap::new(),
        };

        // frame 72 = 3s = middle of clip "b". A 2s trim on a 2s clip hits
        // the clamp rule (trim ≥ duration) → Unresolved, recorded honestly.
        let items = vec![MechItem {
            note_id: "n1".into(),
            author: "client".into(),
            frame: 72,
            rate: 24,
            body: "Cut 2 seconds off the end".into(),
            ops: vec![MechOp::TrimOut {
                seconds: "2".into(),
            }],
            remainder: vec![],
        }];
        let (out, ledger) = apply_changelist(&tl, &items);
        assert!(matches!(ledger[0].status, ApplyStatus::Unresolved(_)));
        // unresolved → timeline unchanged (clone-back, no partial edit)
        let b = &out.tracks.children[0].children[1];
        assert_eq!(b.source_range.as_ref().unwrap().duration.value.num, 48);
        // 1s trim applies: duration 2s → 1s
        let items2 = vec![MechItem {
            note_id: "n1".into(),
            author: "client".into(),
            frame: 72,
            rate: 24,
            body: "Cut 1 second off the end".into(),
            ops: vec![MechOp::TrimOut {
                seconds: "1".into(),
            }],
            remainder: vec![],
        }];
        let (out2, ledger2) = apply_changelist(&tl, &items2);
        assert!(matches!(ledger2[0].status, ApplyStatus::Applied));
        let b2 = &out2.tracks.children[0].children[1];
        // 1s at 24fps = 24 frames exactly (integer assert, no float compare)
        assert_eq!(b2.source_range.as_ref().unwrap().duration.value.num, 24);
        // source timeline untouched (purity)
        assert_eq!(
            tl.tracks.children[0].children[1]
                .source_range
                .as_ref()
                .unwrap()
                .duration
                .value
                .num,
            48
        );

        // delete on an empty-frame note → unresolved, recorded
        let items3 = vec![MechItem {
            note_id: "n2".into(),
            author: "client".into(),
            frame: 100_000,
            rate: 24,
            body: "Delete this clip".into(),
            ops: vec![MechOp::Delete],
            remainder: vec![],
        }];
        let (_, ledger3) = apply_changelist(&tl, &items3);
        assert!(matches!(ledger3[0].status, ApplyStatus::Unresolved(_)));
    }
}
