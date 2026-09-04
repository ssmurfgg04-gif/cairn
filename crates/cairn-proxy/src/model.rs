//! Proxy model: the profile (what a proxy IS), the entry (what was made),
//! and the index (the durable mapping source-digest → proxy).

use serde::{Deserialize, Serialize};

/// What a proxy is. Defaults are the editorial standard: 1080p H.264 in
/// an MP4 with the moov atom up front (faststart) so browsers can scrub
/// via HTTP range requests without downloading the whole file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProxyProfile {
    /// Long-edge pixel cap (e.g. 1080). Media already smaller is NOT
    /// upscaled (the transcoder clamps).
    pub max_height: u32,
    /// Target codec label, e.g. "h264" (informational for ffmpeg).
    pub codec: String,
    /// Constant-rate factor (quality; lower = better).
    pub crf: u32,
}

impl Default for ProxyProfile {
    fn default() -> Self {
        ProxyProfile {
            max_height: 1080,
            codec: "h264".into(),
            crf: 23,
        }
    }
}

impl ProxyProfile {
    /// The review-portal profile: 720p is the streaming sweet spot for
    /// guests on hotel/venue WiFi. The default 1080 profile exists for
    /// local editorial offline work; but a review proxy that is ~95% of
    /// the source (the first dogfood finding: 1080p in, 1080p out)
    /// defeats "remote reviewers pull MBs, not GBs" — 720 halves the
    /// bytes while staying comment-legible (frame-accurate TC, not
    /// pixel-peeping, is the portal's job).
    pub fn review() -> ProxyProfile {
        ProxyProfile {
            max_height: 720,
            codec: "h264".into(),
            crf: 23,
        }
    }
}

/// Lifecycle of one proxy entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum ProxyStatus {
    /// Generated and current (source digest matches).
    Ready,
    /// The source media changed since generation (digest mismatch).
    Stale,
    /// Generation attempted and failed (see `last_error`).
    Failed,
}

impl ProxyStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ProxyStatus::Ready => "READY",
            ProxyStatus::Stale => "STALE",
            ProxyStatus::Failed => "FAILED",
        }
    }
}

/// One indexed proxy.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProxyEntry {
    /// blake3 (hex, 32 chars) of the source media bytes at generation
    /// time — the staleness key.
    pub source_digest: String,
    /// Source media path, relative to the project root.
    pub media_rel: String,
    /// Proxy path, relative to the project root (under `.cairn/proxy-cache/`).
    pub proxy_rel: String,
    pub profile: ProxyProfile,
    pub bytes: u64,
    pub generated_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

impl ProxyEntry {
    pub fn status(&self, current_digest: &str) -> ProxyStatus {
        if self.source_digest == current_digest {
            ProxyStatus::Ready
        } else {
            ProxyStatus::Stale
        }
    }
}

/// The durable index: `.cairn/proxies.json`. BTreeMap keyed by source
/// digest → deterministic serialization, free merge semantics (a proxy is
/// a pure function of its source bytes; two machines that generated the
/// same proxy agree bit-for-bit).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProxyIndex {
    /// digest → entry
    pub proxies: std::collections::BTreeMap<String, ProxyEntry>,
}

impl ProxyIndex {
    pub const SCHEMA: &'static str = "cairn-proxy/v1";

    pub fn to_json(&self) -> Result<Vec<u8>, String> {
        serde_json::to_vec_pretty(self).map_err(|e| format!("serialize proxies: {e}"))
    }

    pub fn from_json(bytes: &[u8]) -> Result<ProxyIndex, String> {
        let f: ProxyIndex =
            serde_json::from_slice(bytes).map_err(|e| format!("parse proxies: {e}"))?;
        Ok(f)
    }

    /// Entries for one media path (a re-rendered file = new digest, so an
    /// older proxy for the same path coexists until pruned).
    pub fn for_media(&self, media_rel: &str) -> Vec<&ProxyEntry> {
        let mut v: Vec<&ProxyEntry> = self
            .proxies
            .values()
            .filter(|e| e.media_rel == media_rel)
            .collect();
        v.sort_by_key(|e| e.generated_at_ms);
        v
    }

    /// The freshest current proxy for a media path (digest match).
    pub fn current_for(&self, media_rel: &str, digest: &str) -> Option<&ProxyEntry> {
        self.proxies
            .values()
            .find(|e| e.media_rel == media_rel && e.source_digest == digest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(digest: &str, media: &str, ms: i64) -> ProxyEntry {
        ProxyEntry {
            source_digest: digest.into(),
            media_rel: media.into(),
            proxy_rel: format!(".cairn/proxy-cache/{digest}.mp4"),
            profile: ProxyProfile::default(),
            bytes: 1000,
            generated_at_ms: ms,
            last_error: None,
        }
    }

    #[test]
    fn index_roundtrips_and_looks_up_by_media_and_digest() {
        let mut idx = ProxyIndex::default();
        idx.proxies
            .insert("aa".into(), entry("aa", "cuts/v1.mov", 100));
        idx.proxies
            .insert("bb".into(), entry("bb", "cuts/v1.mov", 200)); // re-render
        idx.proxies
            .insert("cc".into(), entry("cc", "cuts/v2.mov", 300));

        assert_eq!(idx.for_media("cuts/v1.mov").len(), 2);
        // chronological
        assert_eq!(idx.for_media("cuts/v1.mov")[0].source_digest, "aa");
        let cur = idx.current_for("cuts/v1.mov", "bb").unwrap();
        assert_eq!(cur.source_digest, "bb");
        assert!(idx.current_for("cuts/v1.mov", "zz").is_none());

        let bytes = idx.to_json().unwrap();
        let back = ProxyIndex::from_json(&bytes).unwrap();
        assert_eq!(back, idx);
    }

    #[test]
    fn status_flips_on_digest_change() {
        let e = entry("aa", "m.mov", 1);
        assert_eq!(e.status("aa"), ProxyStatus::Ready);
        assert_eq!(e.status("ff"), ProxyStatus::Stale);
        assert_eq!(ProxyStatus::Stale.as_str(), "STALE");
    }

    #[test]
    fn default_profile_is_1080_h264_faststart_convention() {
        let p = ProxyProfile::default();
        assert_eq!(p.max_height, 1080);
        assert_eq!(p.codec, "h264");
        assert_eq!(p.crf, 23);
    }
}
