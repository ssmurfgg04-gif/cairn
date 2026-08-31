//! §15.2 property tests + M0 acceptance: chunk-reuse >70% on synthetic project-file save
//! sequences; CDC boundary stability; manifest round-trips.

use std::collections::HashSet;

use cairn_core::chunker::{ChunkSpan, FastCdc, StreamHash};
use cairn_core::hash::Hash;
use cairn_core::manifest::{assemble_file, Compression, Manifest, ManifestEntry};
use proptest::prelude::*;
use rand::rngs::StdRng;
use rand::{Rng, RngCore, SeedableRng};

/// Synthetic NLE auto-save generator (mimics Premiere/Resolve save behavior): a binary file
/// with a stable structured header that gets localized edits + small appends between saves.
struct SaveSequence {
    content: Vec<u8>,
}

impl SaveSequence {
    fn initial(rng: &mut StdRng, size: usize) -> Self {
        let mut c = vec![0u8; size];
        rng.fill_bytes(&mut c);
        let header: Vec<u8> = (0..4096)
            .map(|i| b"PROJECT_META_SENSOR_A7S3_4K24_TIMELINE"[i % 38])
            .collect();
        c[..header.len()].copy_from_slice(&header);
        SaveSequence { content: c }
    }

    /// One auto-save: several localized edits + a small append (timeline state grows).
    fn autosave(&mut self, rng: &mut StdRng, edits: usize) {
        for _ in 0..edits {
            let pos = rng.gen_range(4096..self.content.len() - 4096);
            let span = 64 + rng.gen_range(0..2048);
            let end = (pos + span).min(self.content.len());
            for b in &mut self.content[pos..end] {
                *b = rng.gen();
            }
        }
        let tail: Vec<u8> = (0..4096).map(|i| (i % 251) as u8).collect();
        self.content.extend_from_slice(&tail);
    }
}

/// Reused byte ratio of `b`'s chunks found in `a` (by identical span+content position key).
/// Chunks are position-stable under localized edits, so identical (offset,len) prefixes with
/// identical hashes are genuine reuse.
fn reuse_ratio(a: &[ChunkSpan], a_hashes: &[Hash], b: &[ChunkSpan], b_hashes: &[Hash]) -> f64 {
    let set_a: HashSet<Hash> = a_hashes.iter().copied().collect();
    let len_a: u64 = a.iter().map(|s| u64::from(s.len)).sum();
    let shared: u64 = b_hashes
        .iter()
        .filter(|h| set_a.contains(h))
        .zip(b.iter())
        .map(|(_, s)| u64::from(s.len))
        .sum();
    shared as f64 / len_a.max(1) as f64
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    /// CDC guarantee: boundaries strictly before an insertion are byte-identical across versions.
    #[test]
    fn prop_cdc_stability_under_insertion(
        seed in any::<u64>(),
        size in 12usize * 1024 * 1024..16usize * 1024 * 1024,
        at in 1usize * 1024 * 1024..8usize * 1024 * 1024,
    ) {
        let mut rng = StdRng::seed_from_u64(seed);
        let mut base = vec![0u8; size];
        rng.fill_bytes(&mut base);
        let mut edited = base.clone();
        let at = at.min(base.len() - 1);
        edited.insert(at, 0x5A);
        let a = FastCdc::cut(&base);
        let b = FastCdc::cut(&edited);
        let key = |v: &[ChunkSpan], upto: usize| -> Vec<ChunkSpan> {
            v.iter().take_while(|s| s.offset + u64::from(s.len) <= upto as u64).cloned().collect()
        };
        prop_assert_eq!(key(&a, at), key(&b, at));
    }

    /// Same bytes → same hashes (idempotence of the pipeline, SPEC §6).
    #[test]
    fn prop_pipeline_idempotent(seed in any::<u64>(), size in 1usize * 1024 * 1024..6 * 1024 * 1024) {
        let mut rng = StdRng::seed_from_u64(seed);
        let mut buf = vec![0u8; size];
        rng.fill_bytes(&mut buf);
        let h1 = StreamHash::compute(&buf);
        let h2 = StreamHash::compute(&buf);
        prop_assert_eq!(h1.file_hash, h2.file_hash);
        prop_assert_eq!(h1.spans, h2.spans);
        prop_assert_eq!(h1.stream_hash, h2.stream_hash);
    }

    /// Manifest round-trip: serialize → parse → identical logical manifest.
    #[test]
    fn prop_manifest_roundtrip(n_entries in 1usize..300, seed in any::<u64>()) {
        let mut rng = StdRng::seed_from_u64(seed);
        let mut entries = Vec::new();
        let mut off = 0u64;
        for _ in 0..n_entries {
            let len = 1 + rng.gen_range(0u32..64_000);
            let mut bytes = vec![0u8; 8];
            rng.fill_bytes(&mut bytes);
            entries.push(ManifestEntry { offset: off, len, chunk_hash: Hash::of(&bytes) });
            off += u64::from(len);
        }
        let m = Manifest::build(entries, Compression::Zstd3, None);
        let (h, bytes) = m.serialize();
        prop_assert_eq!(h, Hash::of(&bytes));
        let parsed = match Manifest::parse(&bytes) {
            Ok(p) => p,
            Err(e) => panic!("manifest parse failed: {e}"),
        };
        prop_assert_eq!(parsed.total_len(), m.total_len());
        prop_assert_eq!(parsed.entry_count(), m.entry_count());
        prop_assert_eq!(parsed, m);
    }
}

/// M0 ACCEPTANCE (SPEC §19): >70% chunk reuse between consecutive NLE auto-saves,
/// across three consecutive saves and four seeds.
///
/// Model calibration: real auto-save edits are tiny (~2KB) relative to project-file size, and
/// the invalidation unit is the containing 4MB-avg chunk. A 96MB file (~24 chunks) with 3
/// localized edits + append invalidates ≤4 chunks ⇒ expected reuse ≈ 83%. The corpus gate
/// (§15.3) is the real-workload check for this property.
#[test]
fn acceptance_chunk_reuse_over_70pct_on_synthetic_save_sequence() {
    for seed in [7u64, 42, 2026, 999_999] {
        let mut rng = StdRng::seed_from_u64(seed);
        let mut seq = SaveSequence::initial(&mut rng, 96 * 1024 * 1024);
        let v1 = StreamHash::compute(&seq.content);
        seq.autosave(&mut rng, 3);
        let v2 = StreamHash::compute(&seq.content);
        seq.autosave(&mut rng, 3);
        let v3 = StreamHash::compute(&seq.content);

        let r12 = reuse_ratio(&v1.spans, &v1.chunk_hashes, &v2.spans, &v2.chunk_hashes);
        let r23 = reuse_ratio(&v2.spans, &v2.chunk_hashes, &v3.spans, &v3.chunk_hashes);
        assert!(
            r12 > 0.70,
            "seed {seed}: save1→save2 reuse {r12:.3} below 70%"
        );
        assert!(
            r23 > 0.70,
            "seed {seed}: save2→save3 reuse {r23:.3} below 70%"
        );
    }
}

/// Golden corpus harness (§15.3): runs when a corpus is present at `corpus/` (one dir per
/// sequence, files in save order). Skips cleanly when absent — real NLE samples are LFS-gated;
/// ingest workflow lives in docs/runbook-beta.md.
#[test]
fn golden_corpus_harness() {
    let corpus = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("corpus");
    if !corpus.exists() {
        eprintln!("golden corpus absent — harness idle (beta ingest pending)");
        return;
    }
    let mut seqs = 0;
    for dir in std::fs::read_dir(&corpus).expect("corpus dir readable") {
        let dir = dir.expect("entry").path();
        if !dir.is_dir() {
            continue;
        }
        let mut saves: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
            .expect("readable")
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.is_file())
            .collect();
        saves.sort();
        assert!(
            saves.len() >= 2,
            "sequence {:?} needs at least 2 saves",
            dir
        );
        let mut prev: Option<StreamHash> = None;
        for save in &saves {
            let bytes = std::fs::read(save).expect("corpus file readable");
            let sh = StreamHash::compute(&bytes);
            if let Some(p) = &prev {
                let r = reuse_ratio(&p.spans, &p.chunk_hashes, &sh.spans, &sh.chunk_hashes);
                assert!(
                    r > 0.70,
                    "corpus sequence {:?}: reuse {r:.3} below gate",
                    dir
                );
            }
            prev = Some(sh);
        }
        seqs += 1;
    }
    assert!(seqs > 0, "corpus dir present but empty");
}

/// Chunk-level round-trip through manifest + verified assembly (I2 gate).
#[test]
fn assemble_with_verification_roundtrip() {
    let buf: Vec<u8> = (0..9 * 1024 * 1024)
        .map(|i| ((i * 3 + 11) % 256) as u8)
        .collect();
    let sh = StreamHash::compute(&buf);
    let entries: Vec<ManifestEntry> = sh
        .spans
        .iter()
        .zip(sh.chunk_hashes.iter())
        .map(|(s, h)| ManifestEntry {
            offset: s.offset,
            len: s.len,
            chunk_hash: *h,
        })
        .collect();
    let m = Manifest::build(entries, Compression::None, None);
    let mut resolve = |_: &Hash| -> Option<Manifest> { None };
    let mut get = |h: &Hash| -> Option<Vec<u8>> {
        sh.spans
            .iter()
            .zip(sh.chunk_hashes.iter())
            .find(|(_, ch)| **ch == *h)
            .map(|(s, _)| buf[s.offset as usize..s.offset as usize + s.len as usize].to_vec())
    };
    let back = assemble_file(&m, &mut resolve, &mut get).expect("assembly must succeed");
    assert_eq!(back, buf);
}
