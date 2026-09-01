//! REAL-container evidence (review round 3): a production Blender file round-trips
//! through chunk-input normalization. `tests/data/BMW27.blend` is the Blender
//! Foundation's classic BMW27 benchmark scene, served gzip-compressed by Blender's own
//! demo server (`1f 8b` magic, inner payload starts with `BLENDER-v`) — the same
//! single-stream-gzip shape as a compressed `.prproj`. This is the first test in the
//! repo that runs the normalization pipeline on bytes no synthetic generator produced.
//!
//! The save-shaped MUTATION model remains synthetic (real successive saves of a
//! production project are not published); the container and payload are real.

use std::collections::HashSet;

use cairn_core::chunker::StreamHash;
use cairn_core::hash::Hash;
use cairn_core::normalize;

const REAL_BLEND: &[u8] = include_bytes!("data/BMW27.blend");

#[test]
fn real_blender_file_sniffs_as_gzip() {
    assert_eq!(normalize::sniff(REAL_BLEND), normalize::Transform::Gzip);
}

#[test]
fn real_blender_inner_payload_is_a_blend_file() {
    let inner = normalize::decompress_inner(REAL_BLEND, normalize::Transform::Gzip)
        .expect("real Blender gzip stream must decompress");
    assert!(inner.len() > 6_000_000, "BMW27 inner is ~6.1MB, got {}", inner.len());
    assert!(
        inner.starts_with(b"BLENDER"),
        "inner payload must be a real .blend, got {:?}",
        &inner[..12.min(inner.len())]
    );
}

#[test]
fn real_blender_save_sequence_round_trips_with_chunk_identity_reuse() {
    let inner1 = normalize::decompress_inner(REAL_BLEND, normalize::Transform::Gzip).unwrap();

    // ---- save sequence on the REAL payload: a localized edit + a small append ----
    // (the established save-shape; Blender rewrites structs near the edit point and
    // grows the file at the tail)
    let mut inner2 = inner1.clone();
    let edit_at = inner2.len() / 2;
    inner2[edit_at..edit_at + 512].copy_from_slice(&[0xA5u8; 512]);
    inner2.extend_from_slice(b"<cairn-save-marker>v2</cairn-save-marker>");

    // ---- chunk-identity reuse across the save (the CDC delta story, real bytes) ----
    // Byte-weighted: what re-uploads is the bytes of chunks whose hash changed. With
    // 1-4-16MB FastCDC params a 6.1MB project file yields only ~3 chunks, so a
    // chunk-COUNT ratio is too coarse here — bytes are the honest cost unit.
    // the engine chunks TRANSFORMED content with the fine profile (see chunker.rs
    // CHUNK_*_FINE) — mirror that here; this is the exact path a real .blend takes
    let sh1 = StreamHash::compute_with(
        &inner1,
        cairn_core::CHUNK_MIN_FINE,
        cairn_core::CHUNK_AVG_FINE,
        cairn_core::CHUNK_MAX_FINE,
    );
    let sh2 = StreamHash::compute_with(
        &inner2,
        cairn_core::CHUNK_MIN_FINE,
        cairn_core::CHUNK_AVG_FINE,
        cairn_core::CHUNK_MAX_FINE,
    );
    let total = sh2.chunk_hashes.len();
    assert!(total >= 2, "real payload must produce multiple chunks");
    let reused: HashSet<Hash> = sh1.chunk_hashes.iter().copied().collect();
    let reused_bytes: u64 = sh2
        .chunk_hashes
        .iter()
        .zip(sh2.spans.iter())
        .filter(|(h, _)| reused.contains(*h))
        .map(|(_, s)| u64::from(s.len))
        .sum();
    let total_bytes: u64 = sh2.spans.iter().map(|s| u64::from(s.len)).sum();
    let reuse = reused_bytes as f64 / total_bytes.max(1) as f64;
    let reused_chunks = sh2
        .chunk_hashes
        .iter()
        .filter(|h| reused.contains(*h))
        .count();
    assert!(
        reuse > 0.70,
        "real-container byte-reuse {reuse:.3} ({reused_bytes}/{total_bytes} bytes, \
         {reused_chunks}/{total} chunks) below the 0.70 gate"
    );

    // ---- serve path: recompress → gzip-decode → byte-identical inner ----
    let wrapper2 =
        normalize::recompress(&inner2, normalize::Transform::Gzip, "BMW27.blend").unwrap();
    assert_eq!(normalize::sniff(&wrapper2), normalize::Transform::Gzip);
    let back = normalize::decompress_inner(&wrapper2, normalize::Transform::Gzip).unwrap();
    assert_eq!(back, inner2, "wrapper rebuild must preserve the inner payload");
}

#[test]
fn real_blender_raw_chunking_would_have_avalanched() {
    // the REASON normalization exists, measured on real bytes: chunking the raw wrapper
    // across a save yields ~zero reuse — the compressed stream re-randomizes entirely.
    let inner1 = normalize::decompress_inner(REAL_BLEND, normalize::Transform::Gzip).unwrap();
    let mut inner2 = inner1.clone();
    let edit_at = inner2.len() / 2;
    inner2[edit_at..edit_at + 512].copy_from_slice(&[0xA5u8; 512]);

    let raw1 = normalize::recompress(&inner1, normalize::Transform::Gzip, "a.blend").unwrap();
    let raw2 = normalize::recompress(&inner2, normalize::Transform::Gzip, "a.blend").unwrap();
    let sh_raw1 = StreamHash::compute(&raw1);
    let sh_raw2 = StreamHash::compute(&raw2);
    let reused_raw = sh_raw2
        .chunk_hashes
        .iter()
        .filter(|h| sh_raw1.chunk_hashes.contains(h))
        .count();
    assert!(
        reused_raw * 10 <= sh_raw2.chunk_hashes.len(),
        "expected raw-wrapper reuse to collapse (<10%), got {}/{}",
        reused_raw,
        sh_raw2.chunk_hashes.len()
    );
}
