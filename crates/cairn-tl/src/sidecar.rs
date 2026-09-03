//! `.cairn-timeline` sidecar manifest (ADR-0015 §1.1): one per timeline
//! document, synced like any file. The v2 merge pins versions from the
//! sidecar — mixed formats/versions across base/ours/theirs REFUSE (C10)
//! before any parse: honesty over guessing.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidecarError(pub String);

impl std::fmt::Display for SidecarError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "sidecar: {}", self.0)
    }
}

impl std::error::Error for SidecarError {}

/// The sidecar manifest. `otio_version` pins the schema family the doc was
/// captured under (python-otio release, e.g. "0.18.1"); `fcpxml_major/minor`
/// pin the bridge input version. `content_blake3` is the hash of the
/// canonical bytes this sidecar describes (identity pin for the base).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Sidecar {
    /// "otio-json" | "fcpxml"
    pub format: String,
    /// python-otio release family the capture validated against.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub otio_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fcpxml_major: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fcpxml_minor: Option<u32>,
    /// The adapter build that produced the stamps (e.g. "cairn-tl/4.0.0").
    pub adapter: String,
    /// BLAKE3 (hex) of the canonical serialized bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_blake3: Option<String>,
    /// ISO-8601 UTC capture time.
    pub captured_utc: String,
}

pub const ADAPTER: &str = concat!("cairn-tl/", env!("CARGO_PKG_VERSION"));

impl Sidecar {
    /// Build for a canonical OTIO document (hash of the canonical bytes).
    pub fn for_otio(canonical_bytes: &[u8]) -> Sidecar {
        Sidecar {
            format: "otio-json".into(),
            otio_version: Some("0.18.1".into()),
            fcpxml_major: None,
            fcpxml_minor: None,
            adapter: ADAPTER.into(),
            content_blake3: Some(blake3_of(canonical_bytes)),
            captured_utc: now_utc(),
        }
    }

    /// Build for an ingested FCPXML document.
    pub fn for_fcpxml(major: u32, minor: u32, canonical_bytes: &[u8]) -> Sidecar {
        Sidecar {
            format: "fcpxml".into(),
            otio_version: None,
            fcpxml_major: Some(major),
            fcpxml_minor: Some(minor),
            adapter: ADAPTER.into(),
            content_blake3: Some(blake3_of(canonical_bytes)),
            captured_utc: now_utc(),
        }
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }

    pub fn parse(text: &str) -> Result<Sidecar, SidecarError> {
        serde_json::from_str(text).map_err(|e| SidecarError(format!("cannot parse: {e}")))
    }
}

/// The merge gate: mixed versions across base/ours/theirs refuse (C10).
/// Same-format, same-version triples pass; anything else names the mismatch.
pub fn check_mergeable(
    base: &Sidecar,
    ours: &Sidecar,
    theirs: &Sidecar,
) -> Result<(), SidecarError> {
    fn pin(s: &Sidecar) -> (&str, Option<&str>, Option<u32>, Option<u32>) {
        (
            s.format.as_str(),
            s.otio_version.as_deref(),
            s.fcpxml_major,
            s.fcpxml_minor,
        )
    }
    let (b, o, t) = (pin(base), pin(ours), pin(theirs));
    if o != b {
        return Err(SidecarError(format!(
            "ours {} vs base {} — mixed capture versions refuse the merge (C10)",
            describe(o),
            describe(b)
        )));
    }
    if t != b {
        return Err(SidecarError(format!(
            "theirs {} vs base {} — mixed capture versions refuse the merge (C10)",
            describe(t),
            describe(b)
        )));
    }
    Ok(())
}

fn describe(pin: (&str, Option<&str>, Option<u32>, Option<u32>)) -> String {
    match pin {
        ("otio-json", Some(v), _, _) => format!("otio-json (otio {v})"),
        ("fcpxml", _, Some(ma), Some(mi)) => format!("fcpxml {ma}.{mi}"),
        (f, _, _, _) => f.to_string(),
    }
}

fn blake3_of(bytes: &[u8]) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"cairn-tl-sidecar-v1");
    hasher.update(bytes);
    hasher.finalize().to_hex().to_string()
}

fn now_utc() -> String {
    // no chrono dependency: unix seconds (merge tooling, not human display)
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("unix:{secs}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let sc = Sidecar::for_otio(b"{}");
        assert_eq!(sc.format, "otio-json");
        assert_eq!(sc.adapter, ADAPTER);
        let back = Sidecar::parse(&sc.to_json()).unwrap();
        assert_eq!(back, sc);
    }

    #[test]
    fn version_gate_refuses_mixed() {
        let b = Sidecar::for_otio(b"x");
        let o = Sidecar::for_otio(b"y");
        let t = Sidecar::for_fcpxml(1, 11, b"z");
        assert!(check_mergeable(&b, &o, &t).is_err(), "mixed formats refuse");
        let same = Sidecar::for_otio(b"x");
        assert!(check_mergeable(&b, &same, &same).is_ok());
        // different CONTENT hash is fine (that is the point of merging)
        let other = Sidecar::for_otio(b"different");
        assert!(check_mergeable(&b, &other, &other).is_ok());
        // mixed otio versions refuse
        let mut old = Sidecar::for_otio(b"x");
        old.otio_version = Some("0.15.0".into());
        assert!(check_mergeable(&b, &old, &same).is_err());
    }

    #[test]
    fn content_hash_is_content_addressed() {
        assert_eq!(
            Sidecar::for_otio(b"abc").content_blake3,
            Sidecar::for_otio(b"abc").content_blake3
        );
        assert_ne!(
            Sidecar::for_otio(b"abc").content_blake3,
            Sidecar::for_otio(b"abd").content_blake3
        );
    }
}
