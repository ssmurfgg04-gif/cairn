//! AAF/OMF handoff tracking (ADR-0020 §6): the picture/audio version
//! contract. Editors do not deliver final audio — they export an AAF or
//! OMF for the sound team, and the quiet killer is the sound team
//! cutting against the WRONG cut. The handoff ledger binds every export
//! to:
//!
//! * the **file digest** (blake3) of the exported AAF/OMF — re-exports
//!   are detected, tampering is detected, and "which AAF did you get?"
//!   has one answer;
//! * the **timeline fingerprint** (cairn-tl `content_fingerprint`) of the
//!   cut the export was made from — the picture-lock binding;
//! * the **snapshot** the export was made from, when recorded.
//!
//! `verify` recomputes both: the AAF on disk must still hash to the
//! recorded digest, and (when a current timeline is supplied) the
//! timeline fingerprint must still match — a mismatch is "the cut moved
//! after the handoff", exactly the revolt-prevention signal.
//!
//! The ledger is `.cairn/handoffs.json` — a synced deterministic project
//! file like review/members/proxy state.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::model::Timeline;

/// Which interchange flavor was exported.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum HandoffKind {
    Aaf,
    Omf,
}

impl HandoffKind {
    pub fn as_str(self) -> &'static str {
        match self {
            HandoffKind::Aaf => "AAF",
            HandoffKind::Omf => "OMF",
        }
    }

    /// Sniff the container: AAF is an SSA (Structured Storage) file whose
    /// header begins with the CFB magic D0 CF 11 E0; OMF is a IFF-style
    /// container whose header chunk is `OMFI`.
    pub fn sniff(bytes: &[u8]) -> Option<HandoffKind> {
        if bytes.starts_with(&[0xD0, 0xCF, 0x11, 0xE0]) {
            Some(HandoffKind::Aaf)
        } else if bytes.starts_with(b"OMFI") {
            Some(HandoffKind::Omf)
        } else {
            None
        }
    }
}

/// One recorded handoff.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandoffManifest {
    /// blake3 hex of the exported file at record time.
    pub file_digest: String,
    /// Exported file size.
    pub file_bytes: u64,
    /// The flavor (AAF/OMF) — sniffed or declared.
    pub kind: HandoffKind,
    /// Export path, relative to the project root (where the export
    /// landed when the editor recorded it).
    pub file_rel: String,
    /// cairn-tl content fingerprint of the timeline the export was cut
    /// from (the picture-lock binding).
    pub timeline_fingerprint: String,
    /// Snapshot/commit hash the export was made from, if recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<String>,
    pub exported_by: String,
    pub exported_at_ms: i64,
    /// Free-form note ("dialogue stems only", "24-bit 48k").
    #[serde(default)]
    pub note: String,
}

/// The ledger: `.cairn/handoffs.json` — map keyed by file digest so the
/// same export recorded twice converges (idempotent), while any
/// genuinely different export lands under a new key.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandoffFile {
    pub handoffs: BTreeMap<String, HandoffManifest>,
}

pub const SCHEMA: &str = "cairn-handoffs/v1";

impl HandoffFile {
    pub fn to_json(&self) -> Result<Vec<u8>, String> {
        serde_json::to_vec_pretty(self).map_err(|e| format!("serialize handoffs: {e}"))
    }

    pub fn from_json(bytes: &[u8]) -> Result<HandoffFile, String> {
        serde_json::from_slice(bytes).map_err(|e| format!("parse handoffs: {e}"))
    }

    /// Record (or re-record — idempotent by digest) a handoff.
    pub fn record(&mut self, m: HandoffManifest) {
        self.handoffs.insert(m.file_digest.clone(), m);
    }

    /// The newest handoff recorded for a timeline fingerprint.
    pub fn latest_for_timeline(&self, fingerprint: &str) -> Option<&HandoffManifest> {
        self.handoffs
            .values()
            .filter(|h| h.timeline_fingerprint == fingerprint)
            .max_by_key(|h| h.exported_at_ms)
    }
}

/// What `verify` found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HandoffStatus {
    /// Digest matches, timeline matches. Sound can trust it.
    Current,
    /// The AAF/OMF file no longer hashes to the recorded digest —
    /// re-export or corruption.
    FileChanged,
    /// The timeline fingerprint moved since the handoff — the cut the
    /// sound team has is NOT picture lock.
    TimelineMoved,
}

/// Verify a recorded handoff against the world:
/// * `file_bytes_now` — the bytes of the exported file as it exists now
///   (None: the file is missing);
/// * `current_timeline` — the timeline as it exists now (the fingerprint
///   is recomputed; None: skip the picture-lock check).
pub fn verify(
    manifest: &HandoffManifest,
    file_bytes_now: Option<&[u8]>,
    current_timeline: Option<&Timeline>,
) -> HandoffStatus {
    if let Some(bytes) = file_bytes_now {
        let digest = blake3_digest(bytes);
        if digest != manifest.file_digest {
            return HandoffStatus::FileChanged;
        }
    }
    if let Some(tl) = current_timeline {
        let fp = timeline_digest(tl);
        if fp != manifest.timeline_fingerprint {
            return HandoffStatus::TimelineMoved;
        }
    }
    HandoffStatus::Current
}

/// blake3 hex (the workspace's content-addressing primitive).
pub fn blake3_digest(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// The timeline digest for a parsed timeline: blake3 over the canonical
/// serialization — the WHOLE tree (kind, ranges, media, children, names)
/// participates, which is what a sound-team binding needs: anything that
/// would change the exported AAF changes the digest.
pub fn timeline_digest(tl: &Timeline) -> String {
    match crate::canon::serialize(tl) {
        Ok(canon) => blake3::hash(canon.as_bytes()).to_hex().to_string(),
        Err(_) => "unserializable".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Element, JsonMap, Kind};

    fn timeline_named(name: &str) -> Timeline {
        Timeline {
            name: name.into(),
            global_start_time: None,
            metadata: JsonMap::new(),
            tracks: Element::container(Kind::Stack, "tracks", Vec::new()),
            extra: JsonMap::new(),
        }
    }

    fn manifest(fp: &str, bytes: &[u8], ms: i64) -> HandoffManifest {
        HandoffManifest {
            file_digest: blake3_digest(bytes),
            file_bytes: bytes.len() as u64,
            kind: HandoffKind::sniff(bytes).unwrap_or(HandoffKind::Aaf),
            file_rel: "handoffs/sound.aaf".into(),
            timeline_fingerprint: fp.into(),
            snapshot: None,
            exported_by: "editor-a".into(),
            exported_at_ms: ms,
            note: "dialogue stems".into(),
        }
    }

    #[test]
    fn sniffs_aaf_and_omf_magics() {
        assert_eq!(
            HandoffKind::sniff(&[0xD0, 0xCF, 0x11, 0xE0, 1]),
            Some(HandoffKind::Aaf)
        );
        assert_eq!(HandoffKind::sniff(b"OMFIMEDIA"), Some(HandoffKind::Omf));
        assert_eq!(HandoffKind::sniff(b"garbage"), None);
    }

    #[test]
    fn ledger_roundtrips_and_is_idempotent_by_digest() {
        let mut f = HandoffFile::default();
        let aaf = b"\xD0\xCF\x11\xE0 fake-aaf-body";
        f.record(manifest("fp-1", aaf, 100));
        let n = f.handoffs.len();
        f.record(manifest("fp-1", aaf, 500)); // same digest -> replace
        assert_eq!(f.handoffs.len(), n);
        let bytes = f.to_json().unwrap();
        let back = HandoffFile::from_json(&bytes).unwrap();
        assert_eq!(back, f);
        // latest_for_timeline picks the newest by timestamp
        let m = back.latest_for_timeline("fp-1").unwrap();
        assert_eq!(m.exported_at_ms, 500);
        assert_eq!(m.kind, HandoffKind::Aaf);
    }

    #[test]
    fn verify_catches_file_change_and_timeline_move() {
        let cut_v1 = timeline_named("cut");
        let fp1 = timeline_digest(&cut_v1);
        let aaf = b"\xD0\xCF\x11\xE0 aaf-1";
        let m = manifest(&fp1, aaf, 1);

        // all current
        assert_eq!(verify(&m, Some(aaf), Some(&cut_v1)), HandoffStatus::Current);
        // file re-exported/tampered
        let aaf2 = b"\xD0\xCF\x11\xE0 aaf-2";
        assert_eq!(
            verify(&m, Some(aaf2), Some(&cut_v1)),
            HandoffStatus::FileChanged
        );
        // picture moved — the sound-team revolt signal. Any tree change
        // (here: a new clip in the stack) moves the digest.
        let mut cut_v2 = timeline_named("cut");
        cut_v2.tracks = crate::model::Element::container(
            Kind::Stack,
            "tracks",
            vec![crate::model::Element::leaf(
                crate::model::Kind::Clip,
                "new-shot",
            )],
        );
        assert_ne!(timeline_digest(&cut_v2), fp1);
        assert_eq!(
            verify(&m, Some(aaf), Some(&cut_v2)),
            HandoffStatus::TimelineMoved
        );
        // file gone: digest check skipped, timeline still current
        assert_eq!(verify(&m, None, Some(&cut_v1)), HandoffStatus::Current);
    }

    #[test]
    fn corrupt_ledger_fails_closed() {
        assert!(HandoffFile::from_json(b"{ nope").is_err());
    }
}
