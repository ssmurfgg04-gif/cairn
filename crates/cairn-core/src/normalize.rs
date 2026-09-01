//! Chunk-input normalization (review round): compressed project containers defeat
//! content-addressed sync. A 5KB XML edit inside a gzip'd `.prproj` re-randomizes the
//! ENTIRE compressed stream — chunking the raw wrapper yields ~zero reuse no matter how
//! good the chunker is. The fix: sniff the container, chunk the CANONICAL INNER payload,
//! record the transform in the manifest (v2), and recompress on serve. NLEs decompress on
//! open, so wrapper byte-identity is irrelevant; the payload is hash-verified as always.
//!
//! **Scope: GZIP-ONLY** (review round 3). The zip branch was scoped OUT: `.drp` (Resolve)
//! is a MULTI-ENTRY zip archive — there is no single inner payload to chunk, and a
//! multi-entry wrapper cannot be rebuilt from one concatenated payload without storing
//! the entry table. The old zip path (concatenate members, rebuild single-member zip) was
//! therefore WRONG in the wild, not just weak. Until a per-entry-table codec is designed
//! and proven, zip containers sync as opaque bytes (correct, zero reuse) and the Zip
//! transform arms reject loudly instead of silently corrupting. The `Zip` wire tag stays
//! parseable so v2 manifests remain forward-compatible.
//!
//! Real-container evidence: `tests/data/BMW27.blend` — a REAL Blender Foundation
//! production file (already gzip-compressed by Blender itself, `1f 8b` magic, inner
//! payload starts with `BLENDER-v`). The round-trip test exercises the full pipeline on
//! those real bytes.
//!
//! Flag-gated (`normalize_containers`, default OFF) until it soaks behind AttachRoot.

use crate::error::{CairnError, ErrorKind};

/// Container transform applied to a file's bytes before chunking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Transform {
    /// Store/chunk the raw bytes (default).
    #[default]
    None,
    /// gzip container (`.prproj`, compressed `.blend`): chunk the decompressed stream,
    /// re-gzip on serve.
    Gzip,
    /// zip container (`.drp`): SCOPED OUT — multi-entry archives cannot be rebuilt
    /// from a concatenated payload. The tag stays parseable for v2 forward
    /// compatibility; the codec arms reject loudly.
    Zip,
}

impl Transform {
    /// Wire/format tag (manifest v2).
    #[must_use]
    pub const fn tag(self) -> u8 {
        match self {
            Transform::None => 0,
            Transform::Gzip => 1,
            Transform::Zip => 2,
        }
    }

    /// Inverse of [`Transform::tag`].
    #[must_use]
    pub const fn from_tag(t: u8) -> Option<Self> {
        match t {
            0 => Some(Transform::None),
            1 => Some(Transform::Gzip),
            2 => Some(Transform::Zip),
            _ => None,
        }
    }
}

/// Sniff a container by magic: gzip `1f 8b`, zip `PK\x03\x04`.
///
/// zip sniffs as [`Transform::None`] (review round 3 scoping): a multi-entry zip
/// (.drp) has no single inner payload, and "treat as zip" was wrong in the wild.
/// Opaque bytes are CORRECT — they just get zero wrapper-reuse, honestly.
#[must_use]
pub fn sniff(buf: &[u8]) -> Transform {
    if buf.len() >= 2 && buf[0] == 0x1f && buf[1] == 0x8b {
        Transform::Gzip
    } else {
        // zip (`PK\x03\x04`) deliberately NOT claimed: scoped out until a
        // per-entry-table codec exists (see module docs)
        Transform::None
    }
}

fn err(what: &str) -> CairnError {
    CairnError::new(ErrorKind::Compression, format!("normalize: {what}"))
}

/// Extract the canonical inner payload (the bytes we actually chunk).
///
/// The Zip arm REJECTS loudly (review round 3): multi-entry archives cannot be rebuilt
/// from one concatenated payload — see module docs. Never silently corrupt.
pub fn decompress_inner(buf: &[u8], t: Transform) -> Result<Vec<u8>, CairnError> {
    match t {
        Transform::None => Ok(buf.to_vec()),
        Transform::Gzip => {
            let mut out = Vec::new();
            let mut dec = flate2::read::GzDecoder::new(buf);
            std::io::Read::read_to_end(&mut dec, &mut out)
                .map_err(|e| err(&format!("gzip: {e}")))?;
            Ok(out)
        }
        Transform::Zip => Err(err(
            "zip normalization is scoped OUT (multi-entry archives have no single inner \
             payload and cannot be rebuilt without the entry table); the file syncs as \
             opaque bytes instead",
        )),
    }
}

/// Rebuild the wrapper for serving/editors: payload → container bytes.
/// The Zip arm REJECTS loudly (scoped out — see module docs).
pub fn recompress(payload: &[u8], t: Transform, _name: &str) -> Result<Vec<u8>, CairnError> {
    match t {
        Transform::None => Ok(payload.to_vec()),
        Transform::Gzip => {
            let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            std::io::Write::write_all(&mut enc, payload).map_err(|e| err(&format!("gzip: {e}")))?;
            enc.finish().map_err(|e| err(&format!("gzip finish: {e}")))
        }
        Transform::Zip => Err(err(
            "zip normalization is scoped OUT (see module docs); serve the file as opaque bytes",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniff_gzip_only_zip_scoped_to_none() {
        assert_eq!(sniff(&[0x1f, 0x8b, 1, 2]), Transform::Gzip);
        // review round 3: zip is scoped OUT — multi-entry archives (like .drp) have no
        // single inner payload; they sync as opaque bytes instead (correct, zero reuse)
        assert_eq!(sniff(b"PK\x03\x04rest"), Transform::None);
        assert_eq!(sniff(b"<xml/>"), Transform::None);
        assert_eq!(sniff(&[]), Transform::None);
    }

    #[test]
    fn gzip_roundtrip_recovers_payload() {
        let payload = b"<?xml version=\"1.0\"?><project>hello cairn</project>".repeat(100);
        let wrapper = recompress(&payload, Transform::Gzip, "scene.prproj").unwrap();
        assert_eq!(sniff(&wrapper), Transform::Gzip);
        let back = decompress_inner(&wrapper, Transform::Gzip).unwrap();
        assert_eq!(back, payload);
    }

    #[test]
    fn zip_transform_arms_reject_loudly() {
        // scoped OUT (review round 3): the arms must REJECT, never silently mangle
        let err1 = decompress_inner(b"PK\x03\x04whatever", Transform::Zip).unwrap_err();
        assert!(err1.message.contains("scoped OUT"), "{}", err1.message);
        let err2 = recompress(b"payload", Transform::Zip, "resolve.drp").unwrap_err();
        assert!(err2.message.contains("scoped OUT"), "{}", err2.message);
        // wire tag stays parseable for v2 forward compatibility
        assert_eq!(Transform::Zip.tag(), 2);
        assert_eq!(Transform::from_tag(2), Some(Transform::Zip));
    }

    #[test]
    fn localized_inner_edit_recompresses_to_different_wrapper_but_same_inner_tail() {
        // the whole POINT of normalization: wrapper bytes avalanche, inner chunks don't
        let xml = format!("<project>\n{}\n</project>", "<clip id=\"n\"/>".repeat(2000));
        let wrapper1 = recompress(xml.as_bytes(), Transform::Gzip, "a.prproj").unwrap();
        let xml2 = format!(
            "<project>\n{}\n<clip id=\"new\"/>\n</project>",
            "<clip id=\"n\"/>".repeat(2000)
        );
        let wrapper2 = recompress(xml2.as_bytes(), Transform::Gzip, "a.prproj").unwrap();
        assert_ne!(wrapper1, wrapper2, "wrapper bytes must differ");
        let inner1 = decompress_inner(&wrapper1, Transform::Gzip).unwrap();
        let inner2 = decompress_inner(&wrapper2, Transform::Gzip).unwrap();
        // the vast majority of inner PREFIX bytes are identical (localized edit)
        let same = inner1
            .iter()
            .zip(inner2.iter())
            .filter(|(a, b)| a == b)
            .count();
        assert!(same as f64 / inner1.len() as f64 > 0.99);
    }
}
