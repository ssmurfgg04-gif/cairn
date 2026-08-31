//! Compression policy + per-chunk codec (SPEC §6, ADR-0004).
//!
//! Chunking runs on RAW bytes; compression is applied per chunk with a per-file policy flag
//! recorded in the manifest. Media is verbatim; text-ish gets zstd-3; NLE project files get
//! zstd with a per-project dictionary trained on the previous version.

use std::collections::HashMap;
use std::sync::Mutex;

pub use crate::manifest::Compression;

/// Per-project trained dictionary (zstd raw dictionary bytes).
#[derive(Debug, Clone)]
pub struct ProjectDict {
    pub project_id: String,
    pub dict_hash: crate::hash::Hash,
    pub bytes: Vec<u8>,
}

/// Trains a small dictionary for a project from its previous file version(s).
/// zstd dictionary training on the previous version is exactly the "trained on the previous
/// version" policy of SPEC §6.
#[must_use]
pub fn train_project_dict(project_id: &str, previous_version: &[u8]) -> Option<ProjectDict> {
    if previous_version.len() < 1024 {
        return None; // too little signal; fall back to plain zstd-3
    }
    let sample: Vec<&[u8]> = previous_version.chunks(64 * 1024).collect();
    // legacy ZDICT trainer is sensitive to dict/sample ratio; empirically safe zone:
    // dict ≤ 4KB per 64KB of sample data (keeps NLE dict small and useful)
    let total: usize = previous_version.len();
    let dict_size = ((total / 16).min(64 * 1024)).max(1024);
    let dict = match zstd::dict::from_samples(&sample, dict_size) {
        Ok(d) => d,
        // training is opportunistic: fall back to a raw content dictionary (the previous
        // version's own bytes, truncated) which zstd accepts as a raw prefix dict
        Err(_) => {
            let mut raw = previous_version.to_vec();
            raw.truncate(64 * 1024);
            raw
        }
    };
    Some(ProjectDict {
        project_id: project_id.to_string(),
        dict_hash: crate::hash::Hash::of(&dict),
        bytes: dict,
    })
}

/// Extension sniff table (SPEC §6).
const MEDIA_EXTS: &[&str] = &[
    "braw", "prores", "mxf", "r3d", "wav", "mp4", "mov", "m4v", "avi", "mkv", "aac", "aif",
    "aiff", "bwf", "gpr", "arw", "dng", "cr2", "cr3", "nef",
];
const NLE_EXTS: &[&str] = &["prproj", "drp", "fcpxmld", "avp", "veg", "aep", "nle"];
const TEXTISH_EXTS: &[&str] = &[
    "json", "xml", "csv", "txt", "md", "yaml", "yml", "toml", "html", "css", "js", "ts", "edl",
    "srt", "fcpxml", "otio", "aar", "lutt",
];

fn ext_of(path: &str) -> String {
    let lower = path.to_lowercase();
    // handle multi-part suffixes like .fcpxmld
    lower.rsplit('.').next().unwrap_or("").to_string()
}

/// Decide the per-file compression policy (SPEC §6 sniff table).
#[must_use]
pub fn policy_for(path: &str) -> Compression {
    let ext = ext_of(path);
    if MEDIA_EXTS.contains(&ext.as_str()) {
        Compression::None
    } else if NLE_EXTS.contains(&ext.as_str()) {
        Compression::ZstdDict
    } else if TEXTISH_EXTS.contains(&ext.as_str()) {
        Compression::Zstd3
    } else {
        // conservative default: unknown binaries are stored verbatim
        Compression::None
    }
}

/// Dictionary registry (project_id → dict), shared between pipeline passes.
#[derive(Default)]
pub struct DictRegistry {
    inner: Mutex<HashMap<String, ProjectDict>>,
}

impl DictRegistry {
    /// Empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert/replace a project dictionary.
    pub fn put(&self, dict: ProjectDict) {
        self.inner.lock().expect("dict registry poisoned").insert(dict.project_id.clone(), dict);
    }

    /// Fetch a project dictionary.
    #[must_use]
    pub fn get(&self, project_id: &str) -> Option<ProjectDict> {
        self.inner.lock().expect("dict registry poisoned").get(project_id).cloned()
    }
}

/// Compress one raw chunk according to the file's policy. Returns stored bytes.
#[must_use]
pub fn compress_chunk(
    raw: &[u8],
    policy: Compression,
    dict: Option<&ProjectDict>,
) -> Result<Vec<u8>, crate::error::CairnError> {
    let out = match policy {
        Compression::None => raw.to_vec(),
        Compression::Zstd3 => zstd::bulk::compress(raw, 3).map_err(|e| crate::error::CairnError {
            kind: crate::error::ErrorKind::Compression,
            message: format!("zstd compress: {e}"),
        })?,
        Compression::ZstdDict => {
            match dict {
                Some(d) => zstd::bulk::Compressor::with_dictionary(3, &d.bytes)
                    .and_then(|mut c| c.compress(raw))
                    .map_err(|e| crate::error::CairnError {
                        kind: crate::error::ErrorKind::Compression,
                        message: format!("zstd dict compress: {e}"),
                    })?,
                // no dictionary available yet → degrade to plain zstd-3 (decoders receiving
                // dict_hash=None decode plain zstd)
                None => zstd::bulk::compress(raw, 3).map_err(|e| crate::error::CairnError {
                    kind: crate::error::ErrorKind::Compression,
                    message: format!("zstd compress: {e}"),
                })?,
            }
        }
    };
    Ok(out)
}

/// Decompress one stored chunk according to the file's policy.
#[must_use]
pub fn decompress_chunk(
    stored: &[u8],
    policy: Compression,
    dict: Option<&ProjectDict>,
) -> Result<Vec<u8>, crate::error::CairnError> {
    let out = match policy {
        Compression::None => stored.to_vec(),
        Compression::Zstd3 => zstd::bulk::decompress(stored, crate::CHUNK_MAX)
            .map_err(|e| crate::error::CairnError {
                kind: crate::error::ErrorKind::Compression,
                message: format!("zstd decompress: {e}"),
            })?,
        Compression::ZstdDict => match dict {
            Some(d) => zstd::bulk::Decompressor::with_dictionary(&d.bytes)
                .and_then(|mut dec| dec.decompress(stored, crate::CHUNK_MAX))
                .map_err(|e| crate::error::CairnError {
                    kind: crate::error::ErrorKind::Compression,
                    message: format!("zstd dict decompress: {e}"),
                })?,
            None => zstd::bulk::decompress(stored, crate::CHUNK_MAX)
                .map_err(|e| crate::error::CairnError {
                    kind: crate::error::ErrorKind::Compression,
                    message: format!("zstd decompress (no dict): {e}"),
                })?,
        },
    };
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::Hash;

    #[test]
    fn sniff_table_matches_spec() {
        assert_eq!(policy_for("A001_C001_07107.braw"), Compression::None);
        assert_eq!(policy_for("interview.mp4"), Compression::None);
        assert_eq!(policy_for("mix.wav"), Compression::None);
        assert_eq!(policy_for("timeline.prproj"), Compression::ZstdDict);
        assert_eq!(policy_for("project.drp"), Compression::ZstdDict);
        assert_eq!(policy_for("bag.fcpxmld"), Compression::ZstdDict);
        assert_eq!(policy_for("subtitles.srt"), Compression::Zstd3);
        assert_eq!(policy_for("session.json"), Compression::Zstd3);
        assert_eq!(policy_for("unknown.bin"), Compression::None);
    }

    #[test]
    fn zstd_roundtrip_and_integrity() {
        let raw = vec![7u8; 2 * 1024 * 1024];
        let stored = compress_chunk(&raw, Compression::Zstd3, None).unwrap();
        assert!(stored.len() < 100 * 1024, "repetitive chunk should compress hard");
        let back = decompress_chunk(&stored, Compression::Zstd3, None).unwrap();
        assert_eq!(raw, back);
        assert_eq!(Hash::of(&back), Hash::of(&raw));
    }

    #[test]
    fn dictionary_roundtrip() {
        // NLE project files are structured text (JSON/XML-ish): build a realistic prior version
        let mut prev = Vec::new();
        for i in 0..2000u32 {
            prev.extend_from_slice(
                format!(r#"{{"clip":{i},"track":{},"in":{},"out":{},"name":"shot_{i}.braw","gain":1.{},"flags":["color","audio"]}}"#,
                    i % 8, i * 25, i * 25 + 24, i % 10)
                .as_bytes(),
            );
        }
        let dict = train_project_dict("p1", &prev).expect("dict trains on structured sample");
        let dict2 = dict.clone();
        let raw: Vec<u8> = prev.iter().take(512 * 1024).cloned().collect();
        let stored = compress_chunk(&raw, Compression::ZstdDict, Some(&dict)).unwrap();
        let back = decompress_chunk(&stored, Compression::ZstdDict, Some(&dict2)).unwrap();
        assert_eq!(raw, back);
        assert!(stored.len() < raw.len(), "dict-compressed structured data must shrink");
    }

    #[test]
    fn verbatim_media_is_byte_identical() {
        let raw: Vec<u8> = (0..300_000).map(|i| (i % 97) as u8).collect();
        let stored = compress_chunk(&raw, Compression::None, None).unwrap();
        assert_eq!(stored, raw);
    }
}
