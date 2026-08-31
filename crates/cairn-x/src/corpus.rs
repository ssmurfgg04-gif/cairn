//! Golden corpus seed generator (§15.3, runbook-beta).
//!
//! Real NLE autosave sequences are LFS-gated (they carry studio IP) — this
//! generator produces DETERMINISTIC synthetic stand-ins with the same
//! statistical shape: structured project header, a large media index, and
//! per-save localized edits + appends. Every byte is a pure function of the
//! seed, so CI can regenerate the corpus and verify the committed
//! `manifest.json` (BLAKE3 per file) without shipping binaries in git.

use cairn_core::chunker::StreamHash;
use std::path::PathBuf;

/// SplitMix64 — tiny, stable, well-distributed; NOT cryptographic (no need:
/// corpus bytes only need determinism, and BLAKE3 provides the integrity).
struct SplitMix64(u64);

impl SplitMix64 {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }

    fn fill(&mut self, buf: &mut [u8]) {
        for chunk in buf.chunks_mut(8) {
            let v = self.next().to_le_bytes();
            chunk.copy_from_slice(&v[..chunk.len()]);
        }
    }
}

/// Build the save-N version of a synthetic project file.
///
/// Shape per save:
/// - header: JSON-ish text with save counter + timestamp + editor state hash
/// - timeline: mostly stable structure with small in-place tweaks
/// - media index: large seeded region; a moving 1–2MB window is rewritten
/// - render log: append-only section that grows each save
fn build_save(seed: u64, seq: usize, save: usize, base_bytes: usize) -> Vec<u8> {
    let mut rng = SplitMix64(seed ^ (0x9E37_79B9 ^ (seq as u64) << 21) ^ save as u64);
    let mut v = Vec::with_capacity(base_bytes + save * 64 * 1024);

    // -- header (few KB): edited every save (version bump, timestamp, state)
    v.extend_from_slice(
        format!(
            "{{\"cairn-corpus-seed\":true,\"seq\":{seq},\"save\":{save},\
             \"application\":\"synthetic-nle\",\"version\":\"4.0.{save}\",\
             \"state_hash\":\"{:016x}{:016x}\",\n",
            rng.next(),
            rng.next()
        )
        .as_bytes(),
    );
    let header_pad = 6 * 1024;
    let mut pad = vec![0u8; header_pad];
    SplitMix64(seed ^ 0x5EED_0001 ^ (seq as u64) << 11).fill(&mut pad);
    v.extend_from_slice(&pad);
    v.extend_from_slice(b"\n\"timeline\": [\n");
    for i in 0..64 {
        v.extend_from_slice(
            format!(
                "  {{\"clip\":{i},\"in\":{},\"out\":{},\"gain\":{}}},\n",
                rng.next() % 1_000_000,
                rng.next() % 1_000_000,
                (rng.next() % 100) as i64 - 60
            )
            .as_bytes(),
        );
    }
    v.extend_from_slice(b"],\n\"media_index\": \"");

    // -- media index (the bulk): stable seeded region; a moving window is
    //    rewritten each save (an editor scrubbed + re-rendered part of it).
    let media_end = base_bytes.saturating_sub(v.len() + 256 * 1024);
    let mut media = vec![0u8; media_end];
    let mut base_rng = SplitMix64(seed ^ 0xABCD_1234 ^ (seq as u64) << 7);
    base_rng.fill(&mut media);
    // mutation window: ~0.4% of the media region (an editor nudging one clip's
    // cached render), anchored at a stable position with tiny per-save jitter —
    // real autosaves re-write nearly-identical regions, not random sweeps.
    let window = (media_end / 400).max(4096);
    let anchor = media_end * 3 / 5;
    let jitter = (rng.next() % 4096) as usize;
    let start = (anchor + jitter).min(media_end - window);
    let mut w = vec![0u8; window];
    rng.fill(&mut w);
    media[start..start + window].copy_from_slice(&w);
    v.extend_from_slice(&media);

    // -- render log: append-only (grows every save — the dominant delta)
    v.extend_from_slice(b"\",\n\"render_log\": [\n");
    for i in 0..(save + 1) * 48 {
        v.extend_from_slice(
            format!(
                "  {{\"t\":{i},\"node\":\"{:016x}\",\"ms\":{}}},\n",
                rng.next(),
                rng.next() % 40_000
            )
            .as_bytes(),
        );
    }
    v.extend_from_slice(b"]}\n");
    v
}

/// Generate `sequences` × `saves` files + manifest. Returns per-sequence
/// minimum consecutive-save reuse (printed for the ingest record).
pub fn generate(
    out_dir: &std::path::Path,
    sequences: usize,
    saves: usize,
    base_bytes: usize,
    seed: u64,
) -> anyhow::Result<()> {
    std::fs::create_dir_all(out_dir)?;
    let mut manifest = serde_json::Map::new();
    manifest.insert(
        "generator".into(),
        serde_json::json!("cairn-x corpus-gen (deterministic synthetic NLE autosaves)"),
    );
    manifest.insert("seed".into(), serde_json::json!(seed));
    manifest.insert("saves_per_sequence".into(), serde_json::json!(saves));
    let mut files = Vec::new();
    let mut reuse_report = Vec::new();

    for seq in 0..sequences {
        let name = format!("seed-corpus-{seq:03}");
        let dir = out_dir.join(&name);
        std::fs::create_dir_all(&dir)?;
        let mut prev: Option<StreamHash> = None;
        let mut min_reuse = f64::MAX;
        for save in 0..saves {
            let path = dir.join(format!("{:02}.dat", save + 1));
            let bytes = build_save(seed, seq, save, base_bytes);
            let sh = StreamHash::compute(&bytes);
            if let Some(p) = &prev {
                let r = reuse_ratio_of(p, &sh);
                min_reuse = min_reuse.min(r);
            }
            let file_hash_hex = sh.file_hash.hex();
            prev = Some(sh);
            std::fs::write(&path, &bytes)?;
            files.push(serde_json::json!({
                "path": format!("{name}/{:02}.dat", save + 1),
                "bytes": bytes.len(),
                "blake3": file_hash_hex,
            }));
        }
        tracing::info!(sequence = %name, min_reuse = format!("{min_reuse:.3}"), "sequence generated");
        reuse_report.push(serde_json::json!({"sequence": name, "min_consecutive_reuse": format!("{min_reuse:.3}")}));
    }

    manifest.insert("files".into(), serde_json::json!(files));
    manifest.insert("reuse_report".into(), serde_json::json!(reuse_report));
    let manifest_path = out_dir.join("manifest.json");
    std::fs::write(
        &manifest_path,
        serde_json::to_string_pretty(&serde_json::Value::Object(manifest))?,
    )?;
    tracing::info!(manifest = %manifest_path.display(), "corpus manifest written");
    Ok(())
}

/// Consecutive-save reuse via the same span/hash model the property tests use:
/// fraction of bytes in save N whose chunks were also present in save N-1.
fn reuse_ratio_of(prev: &StreamHash, cur: &StreamHash) -> f64 {
    // Chunk-hash set intersection weighted by chunk length.
    use std::collections::HashSet;
    let prev_hashes: HashSet<_> = prev.chunk_hashes.iter().collect();
    let mut reused = 0u64;
    let mut total = 0u64;
    for (h, span) in cur.chunk_hashes.iter().zip(cur.spans.iter()) {
        total += u64::from(span.len);
        if prev_hashes.contains(h) {
            reused += u64::from(span.len);
        }
    }
    if total == 0 {
        1.0
    } else {
        reused as f64 / total as f64
    }
}

/// CLI glue.
pub fn run(
    out: &str,
    sequences: usize,
    saves: usize,
    base_mb: usize,
    seed: u64,
) -> anyhow::Result<()> {
    generate(
        &PathBuf::from(out),
        sequences,
        saves,
        base_mb * 1024 * 1024,
        seed,
    )
}
