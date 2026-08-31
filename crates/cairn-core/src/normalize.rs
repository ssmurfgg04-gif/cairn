//! Chunk-input normalization (review round): compressed project containers defeat
//! content-addressed sync. A 5KB XML edit inside a gzip'd `.prproj` (or zip'd `.drp`)
//! re-randomizes the ENTIRE compressed stream — chunking the raw wrapper yields ~zero
//! reuse no matter how good the chunker is. The fix: sniff the container, chunk the
//! CANONICAL INNER payload, record the transform in the manifest (v2), and recompress on
//! serve. NLEs decompress on open, so wrapper byte-identity is irrelevant; the payload is
//! hash-verified as always.
//!
//! Flag-gated (`normalize_containers`, default OFF) until it soaks behind AttachRoot.

use crate::error::{CairnError, ErrorKind};

/// Container transform applied to a file's bytes before chunking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Transform {
    /// Store/chunk the raw bytes (default).
    #[default]
    None,
    /// gzip container (`.prproj`): chunk the decompressed stream, re-gzip on serve.
    Gzip,
    /// zip container (`.drp`): chunk the canonical single-member payload, re-zip on serve.
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
#[must_use]
pub fn sniff(buf: &[u8]) -> Transform {
    if buf.len() >= 2 && buf[0] == 0x1f && buf[1] == 0x8b {
        Transform::Gzip
    } else if buf.len() >= 4 && buf[0] == b'P' && buf[1] == b'K' && buf[2] == 3 && buf[3] == 4 {
        Transform::Zip
    } else {
        Transform::None
    }
}

fn err(what: &str) -> CairnError {
    CairnError::new(ErrorKind::Compression, format!("normalize: {what}"))
}

/// Extract the canonical inner payload (the bytes we actually chunk).
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
        Transform::Zip => {
            let mut cursor = std::io::Cursor::new(buf);
            let mut archive =
                zip::ZipArchive::new(&mut cursor).map_err(|e| err(&format!("zip: {e}")))?;
            // canonical payload: all members in archive order, concatenated. Recompression
            // rebuilds a single-member zip (member names are not content identity).
            let mut out = Vec::with_capacity(buf.len() * 2);
            for i in 0..archive.len() {
                let mut f = archive
                    .by_index(i)
                    .map_err(|e| err(&format!("zip member {i}: {e}")))?;
                std::io::Read::read_to_end(&mut f, &mut out)
                    .map_err(|e| err(&format!("zip read: {e}")))?;
            }
            Ok(out)
        }
    }
}

/// Rebuild the wrapper for serving/editors: payload → container bytes.
pub fn recompress(payload: &[u8], t: Transform, name: &str) -> Result<Vec<u8>, CairnError> {
    match t {
        Transform::None => Ok(payload.to_vec()),
        Transform::Gzip => {
            let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            std::io::Write::write_all(&mut enc, payload).map_err(|e| err(&format!("gzip: {e}")))?;
            enc.finish().map_err(|e| err(&format!("gzip finish: {e}")))
        }
        Transform::Zip => {
            let mut w = std::io::Cursor::new(Vec::new());
            {
                let mut zip = zip::ZipWriter::new(&mut w);
                let opts = zip::write::SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Deflated);
                let member = name.rsplit('/').next().unwrap_or(name);
                zip.start_file(member, opts)
                    .map_err(|e| err(&format!("zip: {e}")))?;
                std::io::Write::write_all(&mut zip, payload)
                    .map_err(|e| err(&format!("zip: {e}")))?;
                zip.finish().map_err(|e| err(&format!("zip finish: {e}")))?;
            }
            Ok(w.into_inner())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniff_gzip_zip_and_none() {
        assert_eq!(sniff(&[0x1f, 0x8b, 1, 2]), Transform::Gzip);
        assert_eq!(sniff(b"PK\x03\x04rest"), Transform::Zip);
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
    fn zip_roundtrip_recovers_payload_regardless_of_member_names() {
        let payload = b"<drp><timeline/></drp>".repeat(50);
        // "original" archive may have many members — canonical payload concatenates
        let multi = multi_member_zip(&payload, &["a.xml", "b.xml"]);
        assert_eq!(sniff(&multi), Transform::Zip);
        let canonical = decompress_inner(&multi, Transform::Zip).unwrap();
        assert_eq!(canonical, payload);
        // serve path: rebuild a single-member zip from the canonical payload
        let wrapper = recompress(&canonical, Transform::Zip, "resolve.drp").unwrap();
        assert_eq!(decompress_inner(&wrapper, Transform::Zip).unwrap(), payload);
    }

    fn multi_member_zip(payload: &[u8], names: &[&str]) -> Vec<u8> {
        let mut w = std::io::Cursor::new(Vec::new());
        {
            let mut zip = zip::ZipWriter::new(&mut w);
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            let chunks = payload.chunks(payload.len() / names.len().max(1) + 1);
            for (name, chunk) in names.iter().zip(chunks) {
                zip.start_file(*name, opts).unwrap();
                std::io::Write::write_all(&mut zip, chunk).unwrap();
            }
            zip.finish().unwrap();
        }
        w.into_inner()
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
