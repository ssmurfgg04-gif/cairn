//! Timeline branches — git-for-video, the foolproof cut (ADR-0023 §4).
//!
//! The problem being killed: `Project_v3_experimental.prproj` → 15 files on
//! the drive, nobody remembers which one had the good transition.
//!
//! The model: a branch is a NAMED COPY of a timeline plus its parent
//! fingerprint, stored under `<root>/.cairn/branches/<name>/timeline.otio`
//! with the ledger at `.cairn/branches/branches.json`. The working timeline
//! file is NEVER touched by branch operations:
//! - `create` copies IN (a new file appears, nothing changes)
//! - `checkout` copies OUT to a fresh path (never clobbers an existing
//!   different file — the "Save As" killer)
//! - `merge` is the cairn-tl three-way merge with the branch's parent as
//!   base; output lands in `<target>.merged.otio` per ADR-0015 convention
//! - `cherry_pick` transfers ONE element (by uuid or name) from a branch
//!   into a target timeline — the "steal the one good transition" move
//! - `delete` is SOFT: the branch moves to `.cairn/branches/trash/` and
//!   stays recoverable via `restore`; only `purge --force` is forever
//!
//! Everything here is pure data + logic; the CLI owns file I/O. Local-first
//! (`.cairn*` is ignore-listed by sync, SPEC §10 — branches are the
//! editor's own sandbox; synced team branches are the named follow-up in
//! ADR-0023 §7).

use serde::{Deserialize, Serialize};

use crate::model::{Element, Kind, Timeline, TrackKind};

pub const SCHEMA: &str = "cairn-branches/v1";
/// Reserved names that can never be a branch (the trunk + the trash dir).
pub const RESERVED: &[&str] = &["main", "trash", "master"];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BranchState {
    #[default]
    Active,
    /// Soft-deleted: files live under `trash/<name>/`, recoverable.
    Trashed,
    /// Removed for good (`purge`). The entry remains so history/names stay
    /// honest — a re-created branch gets a NEW entry.
    Purged,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchEntry {
    pub name: String,
    pub author: String,
    pub note: String,
    /// blake3 digest of the PARENT timeline's canonical serialization —
    /// the merge base. If the parent bytes are unavailable at merge time,
    /// the merge REFUSES rather than guessing a base.
    pub parent_digest: String,
    /// Source timeline path (relative to root, informational).
    pub source_path: String,
    pub created_ms: i64,
    #[serde(default)]
    pub state: BranchState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trashed_at: Option<i64>,
}

/// The branch ledger: `.cairn/branches/branches.json`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchLedger {
    pub schema: String,
    /// BTreeMap → byte-stable serialization (the notes.rs discipline).
    pub branches: std::collections::BTreeMap<String, BranchEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BranchError {
    /// Invalid name (empty / separators / reserved / too long).
    BadName(String),
    /// A branch with this name already exists.
    Exists(String),
    /// No such branch (or not in the requested state).
    Missing(String),
    /// The branch is in a state that refuses this operation.
    WrongState(String),
    /// Malformed ledger bytes.
    Corrupt(String),
}

impl std::fmt::Display for BranchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BranchError::BadName(n) => write!(f, "invalid branch name `{n}`"),
            BranchError::Exists(n) => write!(f, "branch `{n}` already exists"),
            BranchError::Missing(n) => write!(f, "branch `{n}` not found"),
            BranchError::WrongState(n) => {
                write!(f, "branch `{n}` is not in a state that allows this")
            }
            BranchError::Corrupt(e) => write!(f, "corrupt branch ledger: {e}"),
        }
    }
}

impl BranchLedger {
    #[must_use]
    pub fn new() -> BranchLedger {
        BranchLedger {
            schema: SCHEMA.into(),
            branches: std::collections::BTreeMap::new(),
        }
    }

    pub fn from_json(bytes: &[u8]) -> Result<BranchLedger, BranchError> {
        let l: BranchLedger =
            serde_json::from_slice(bytes).map_err(|e| BranchError::Corrupt(e.to_string()))?;
        if l.schema != SCHEMA {
            return Err(BranchError::Corrupt(format!(
                "schema {:?} != {SCHEMA:?}",
                l.schema
            )));
        }
        Ok(l)
    }

    pub fn to_json(&self) -> Result<Vec<u8>, BranchError> {
        serde_json::to_vec_pretty(self).map_err(|e| BranchError::Corrupt(e.to_string()))
    }

    /// Validate a branch name. The rule set is deliberately tight: editors
    /// are not developers; a branch name is a label, not a path.
    pub fn validate_name(name: &str) -> Result<(), BranchError> {
        let bad = |n: &str| Err(BranchError::BadName(n.to_string()));
        if name.is_empty() || name.len() > 64 {
            return bad(name);
        }
        if RESERVED.contains(&name.to_lowercase().as_str()) {
            return bad(name);
        }
        if name.contains(['/', '\\', ':', '*', '?', '"', '<', '>', '|'])
            || name.starts_with('.')
            || name.ends_with('.')
            || name.trim() != name
        {
            return bad(name);
        }
        if name.chars().any(|c| c.is_control()) {
            return bad(name);
        }
        Ok(())
    }

    /// Register a new branch. Refuses duplicates and bad names. The CALLER
    /// copies the timeline bytes (this is the ledger transaction only).
    pub fn create(
        &mut self,
        name: &str,
        author: &str,
        note: &str,
        parent_digest: &str,
        source_path: &str,
        now_ms: i64,
    ) -> Result<(), BranchError> {
        Self::validate_name(name)?;
        if let Some(existing) = self.branches.get(name) {
            match existing.state {
                BranchState::Active | BranchState::Trashed => {
                    return Err(BranchError::Exists(name.into()))
                }
                BranchState::Purged => {} // name is free again
            }
        }
        self.branches.insert(
            name.to_string(),
            BranchEntry {
                name: name.into(),
                author: author.into(),
                note: note.into(),
                parent_digest: parent_digest.into(),
                source_path: source_path.into(),
                created_ms: now_ms,
                state: BranchState::Active,
                trashed_at: None,
            },
        );
        Ok(())
    }

    /// Soft-delete: Active → Trashed. The caller moves the files to
    /// `trash/<name>/`; the ledger records the transition. Two days of work
    /// must survive an editor's bad morning.
    pub fn trash(&mut self, name: &str, now_ms: i64) -> Result<(), BranchError> {
        let e = self
            .branches
            .get_mut(name)
            .ok_or_else(|| BranchError::Missing(name.into()))?;
        if e.state != BranchState::Active {
            return Err(BranchError::WrongState(name.into()));
        }
        e.state = BranchState::Trashed;
        e.trashed_at = Some(now_ms);
        Ok(())
    }

    /// Recover a trashed branch. The caller moves the files back.
    pub fn restore(&mut self, name: &str) -> Result<(), BranchError> {
        let e = self
            .branches
            .get_mut(name)
            .ok_or_else(|| BranchError::Missing(name.into()))?;
        if e.state != BranchState::Trashed {
            return Err(BranchError::WrongState(name.into()));
        }
        e.state = BranchState::Active;
        e.trashed_at = None;
        Ok(())
    }

    /// HARD delete: the entry stays (name history + honesty) but the state
    /// is Purged and the caller removes the files. Only reachable through
    /// an explicit `purge --force`.
    pub fn purge(&mut self, name: &str) -> Result<(), BranchError> {
        let e = self
            .branches
            .get_mut(name)
            .ok_or_else(|| BranchError::Missing(name.into()))?;
        if e.state == BranchState::Purged {
            return Err(BranchError::WrongState(name.into()));
        }
        e.state = BranchState::Purged;
        Ok(())
    }

    /// Active branches, in name order (BTreeMap).
    #[must_use]
    pub fn active(&self) -> Vec<&BranchEntry> {
        self.branches
            .values()
            .filter(|e| e.state == BranchState::Active)
            .collect()
    }

    #[must_use]
    pub fn get(&self, name: &str) -> Option<&BranchEntry> {
        self.branches.get(name)
    }
}

// ---------------------------------------------------------------------------
// Cherry-pick: transfer ONE element from a branch timeline into a target.
// ---------------------------------------------------------------------------

/// Cherry-pick a single element (by uuid — preferred — or name) from `source`
/// (the branch) into `target`. Returns the new timeline; `target` is never
/// mutated. Fails closed:
/// - element not found in the source
/// - element already present in the target (by the same reference)
///
/// Positioning: the element lands immediately AFTER the last target element
/// that also exists in the source BEFORE the picked element (a shared anchor
/// — the "it goes where it went in the experiment" instinct). With no shared
/// anchor it lands at the END of the first same-kind track (or the first
/// track). Deterministic, never surprising.
pub fn cherry_pick(
    source: &Timeline,
    target: &Timeline,
    element_ref: &str,
) -> Result<Timeline, String> {
    let (src_track, src_idx) = find_element(source, element_ref)
        .ok_or_else(|| format!("element `{element_ref}` not found in the branch timeline"))?;
    if find_element(target, element_ref).is_some() {
        return Err(format!(
            "element `{element_ref}` already exists in the target timeline"
        ));
    }
    let picked = source.tracks.children[src_track].children[src_idx].clone();

    let mut out = target.clone();
    // 1. find a shared anchor: elements of the target that also exist in the
    //    source BEFORE the picked element
    let source_before: Vec<&Element> = source.tracks.children[src_track].children[..src_idx]
        .iter()
        .collect();
    let mut anchor: Option<(usize, usize)> = None;
    'tracks: for (ti, tr) in out.tracks.children.iter().enumerate() {
        if !matches!(tr.kind, Kind::Track(_)) {
            continue;
        }
        for (ii, item) in tr.children.iter().enumerate().rev() {
            let key = el_key(item);
            if source_before.iter().any(|s| el_key(s) == key) {
                anchor = Some((ti, ii));
                break 'tracks;
            }
        }
    }
    match anchor {
        Some((ti, ii)) => {
            out.tracks.children[ti].children.insert(ii + 1, picked);
        }
        None => {
            // fall back: end of the first track of the same kind as the
            // source track, else the first track, else create one
            let want_kind = match source.tracks.children[src_track].kind {
                Kind::Track(k) => k,
                _ => TrackKind::Video,
            };
            let ti = out
                .tracks
                .children
                .iter()
                .position(|t| matches!(t.kind, Kind::Track(k) if k == want_kind))
                .or_else(|| {
                    out.tracks
                        .children
                        .iter()
                        .position(|t| matches!(t.kind, Kind::Track(_)))
                });
            match ti {
                Some(ti) => out.tracks.children[ti].children.push(picked),
                None => {
                    let track_name = match want_kind {
                        TrackKind::Video => "V1",
                        TrackKind::Audio => "A1",
                        TrackKind::Subtitle => "S1",
                    };
                    out.tracks.children.push(Element::container(
                        Kind::Track(want_kind),
                        track_name,
                        vec![picked],
                    ));
                }
            }
        }
    }
    Ok(out)
}

/// Identity key for cherry-pick matching: uuid (rung a) else name.
fn el_key(e: &Element) -> String {
    match e.cairn_uuid() {
        Some(u) => format!("uuid:{u}"),
        None => format!("name:{}", e.name),
    }
}

/// Find a top-level track item by uuid-or-name. Returns (track, index).
fn find_element(tl: &Timeline, element_ref: &str) -> Option<(usize, usize)> {
    for (ti, tr) in tl.tracks.children.iter().enumerate() {
        if !matches!(tr.kind, Kind::Track(_)) {
            continue;
        }
        for (ii, item) in tr.children.iter().enumerate() {
            let key = el_key(item);
            if key == format!("uuid:{element_ref}") || key == format!("name:{element_ref}") {
                return Some((ti, ii));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::*;

    fn tl_with(names: &[&str]) -> Timeline {
        let items: Vec<Element> = names
            .iter()
            .map(|n| {
                let mut e = Element::leaf(Kind::Clip, *n);
                e.stamp_uuid(&format!("uuid-{n}"));
                e
            })
            .collect();
        Timeline {
            name: "t".into(),
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

    #[test]
    fn ledger_lifecycle_create_trash_restore_purge() {
        let mut l = BranchLedger::new();
        l.create(
            "rough-cut",
            "alice",
            "the wild one",
            "deadbeef",
            "seq.otio",
            100,
        )
        .unwrap();
        assert_eq!(l.active().len(), 1);
        // duplicate refused
        assert_eq!(
            l.create("rough-cut", "a", "", "", "", 1),
            Err(BranchError::Exists("rough-cut".into()))
        );
        // reserved refused
        assert!(matches!(
            l.create("main", "a", "", "", "", 1),
            Err(BranchError::BadName(_))
        ));
        assert!(matches!(
            l.create("trash", "a", "", "", "", 1),
            Err(BranchError::BadName(_))
        ));
        assert!(matches!(
            l.create("../evil", "a", "", "", "", 1),
            Err(BranchError::BadName(_))
        ));
        assert!(matches!(
            l.create("a/b", "a", "", "", "", 1),
            Err(BranchError::BadName(_))
        ));
        assert!(matches!(
            l.create("", "a", "", "", "", 1),
            Err(BranchError::BadName(_))
        ));
        // trash → active gone, restore → back
        l.trash("rough-cut", 200).unwrap();
        assert_eq!(l.active().len(), 0);
        assert!(l.get("rough-cut").unwrap().trashed_at.is_some());
        l.restore("rough-cut").unwrap();
        assert_eq!(l.active().len(), 1);
        // double-trash refused
        l.trash("rough-cut", 300).unwrap();
        assert_eq!(
            l.trash("rough-cut", 400),
            Err(BranchError::WrongState("rough-cut".into()))
        );
        // purge → name free again
        l.purge("rough-cut").unwrap();
        l.create("rough-cut", "bob", "second life", "cafe", "seq.otio", 500)
            .unwrap();
        assert_eq!(l.active().len(), 1);
        assert_eq!(l.get("rough-cut").unwrap().author, "bob");
        // missing
        assert_eq!(l.trash("nope", 1), Err(BranchError::Missing("nope".into())));
        // round-trip
        let bytes = l.to_json().unwrap();
        let back = BranchLedger::from_json(&bytes).unwrap();
        assert_eq!(l, back);
    }

    #[test]
    fn cherry_pick_steals_the_good_transition() {
        let target = tl_with(&["a", "b", "c"]);
        // the experiment: same a,b,c + the gem
        let mut branch = tl_with(&["a", "b", "c", "gem"]);
        // simulate the gem being new: give it a fresh uuid not in target
        branch.tracks.children[0].children[3]
            .metadata
            .get_mut("cairn")
            .unwrap()["uuid"] = serde_json::json!("uuid-gem-2");

        let out = cherry_pick(&branch, &target, "uuid-gem-2").unwrap();
        let items = &out.tracks.children[0].children;
        assert_eq!(items.len(), 4);
        // positioned after the shared anchor "c" (last shared-before element)
        assert_eq!(items[3].name, "gem");
        // target untouched (purity)
        assert_eq!(target.tracks.children[0].children.len(), 3);
        // re-pick refused (already present)
        assert!(cherry_pick(&branch, &out, "uuid-gem-2").is_err());
        // unknown ref refused
        assert!(cherry_pick(&branch, &target, "uuid-nope").is_err());
    }

    #[test]
    fn cherry_pick_by_name_appends_without_shared_anchor() {
        // elements WITHOUT uuid stamps — name is the identity (rung b)
        let mk = |names: Vec<&str>| Timeline {
            name: "t".into(),
            global_start_time: None,
            metadata: JsonMap::new(),
            tracks: Element::container(
                Kind::Stack,
                "tracks",
                vec![Element::container(
                    Kind::Track(TrackKind::Video),
                    "V1",
                    names
                        .into_iter()
                        .map(|n| Element::leaf(Kind::Clip, n))
                        .collect(),
                )],
            ),
            extra: JsonMap::new(),
        };
        let target = mk(vec!["x"]);
        let branch = mk(vec!["a", "gem"]);
        let out = cherry_pick(&branch, &target, "gem").unwrap();
        // no shared anchor → end of first (video) track
        assert_eq!(out.tracks.children[0].children.len(), 2);
        assert_eq!(out.tracks.children[0].children[1].name, "gem");
    }
}
