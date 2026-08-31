//! FastCDC content-defined chunking (SPEC §5.1/§6, ADR-0003).
//!
//! Ported approach: FastCDC 2016 paper + restic's BSD-2 chunker (Gear table generation, rolling
//! boundary discipline). See THIRD_PARTY.md. Gear table is versioned via `CHUNKER_VERSION` —
//! changing it changes every chunk identity (protocol-breaking).
//!
//! Parameters are contractual (SPEC §5.1): min 1MB, avg 4MB (boundary mask 2^22), max 16MB.
//! Boundary condition: `gear & MASK == 0` inside `[min, max)`. Insertion-stability (CDC
//! guarantee) is property-tested: bytes before an insertion cut identically.

use crate::{CHUNK_AVG, CHUNK_MAX, CHUNK_MIN};

/// 64-bit Gear boundary mask: 2^22 → expected average chunk 4MB on random data.
pub const BOUNDARY_MASK: u64 = (1 << 22) - 1;

/// Fixed 256-entry Gear table (splitmix64 stream, documented + deterministic).
/// Ported approach from restic's chunker table generation (see THIRD_PARTY.md).
#[rustfmt::skip]
pub fn gear_table() -> [u64; 256] {
    let mut t = [0u64; 256];
    let mut state = 0x853c_49e6_748f_ea9bu64; // fixed seed, never change (CHUNKER_VERSION=1)
    for slot in t.iter_mut() {
        // splitmix64 step
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        *slot = z ^ (z >> 31);
    }
    t
}

/// One chunk cut by the CDC pipeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkSpan {
    /// Offset of the chunk within the stream.
    pub offset: u64,
    /// Chunk length (raw bytes).
    pub len: u32,
}

/// Streaming FastCDC chunker. Feed buffers with `push`; collect completed cuts.
/// The whole-stream BLAKE3 and per-chunk BLAKE3 are computed by the caller in the same pass
/// (SPEC §6 "single pass") — this type only decides boundaries.
#[derive(Debug)]
pub struct FastCdc {
    min: usize,
    max: usize,
    mask: u64,
    table: [u64; 256],
    /// Rolling gear hash carried across `push` calls.
    gear: u64,
    /// Bytes consumed since last cut.
    since_cut: u64,
    /// Absolute offset of the pending chunk start.
    chunk_start: u64,
    /// Absolute offset of all bytes seen so far.
    abs: u64,
}

impl Default for FastCdc {
    fn default() -> Self {
        Self::new(CHUNK_MIN, CHUNK_AVG, CHUNK_MAX)
    }
}

impl FastCdc {
    /// New chunker with contractual defaults available via `FastCdc::default()`.
    #[must_use]
    pub fn new(min: usize, avg: usize, max: usize) -> Self {
        // mask = 2^ceil(log2(avg)) - 1 → avg 4MB ⇒ 2^22 - 1 (SPEC: boundary mask 2^22)
        let mask = if avg == CHUNK_AVG {
            BOUNDARY_MASK
        } else {
            let bits = usize::BITS - usize::leading_zeros(avg.next_power_of_two() - 1);
            (1u64 << bits) - 1
        };
        FastCdc {
            min,
            max,
            mask,
            table: gear_table(),
            gear: 0,
            since_cut: 0,
            chunk_start: 0,
            abs: 0,
        }
    }

    /// Feed the next buffer; returns completed chunk spans (offsets relative to stream start).
    pub fn push(&mut self, data: &[u8], out: &mut Vec<ChunkSpan>) {
        for &b in data {
            self.gear = (self.gear << 1).wrapping_add(self.table[usize::from(b)]);
            self.since_cut += 1;
            self.abs += 1;
            if self.since_cut >= self.min as u64 && (self.gear & self.mask) == 0
                || self.since_cut >= self.max as u64
            {
                out.push(ChunkSpan {
                    offset: self.chunk_start,
                    len: self.since_cut as u32,
                });
                self.chunk_start = self.abs;
                self.since_cut = 0;
            }
        }
    }

    /// Finish the stream: emit the trailing chunk if any bytes remain.
    pub fn finish(&mut self, out: &mut Vec<ChunkSpan>) {
        if self.since_cut > 0 {
            out.push(ChunkSpan {
                offset: self.chunk_start,
                len: self.since_cut as u32,
            });
            self.chunk_start = self.abs;
            self.since_cut = 0;
        }
    }

    /// Convenience: chunk an in-memory buffer in one call.
    #[must_use]
    pub fn cut(buf: &[u8]) -> Vec<ChunkSpan> {
        let mut c = FastCdc::default();
        let mut spans = Vec::new();
        c.push(buf, &mut spans);
        c.finish(&mut spans);
        spans
    }
}

/// Spans + per-chunk hashes + whole-stream hash in one pass over an in-memory buffer
/// (SPEC §6 "stream once"). For files >100MB the caller streams via `FastCdc` directly.
#[derive(Debug, Clone)]
pub struct StreamHash {
    /// Spans in order.
    pub spans: Vec<ChunkSpan>,
    /// Per-chunk BLAKE3 hashes (raw content), in file order.
    pub chunk_hashes: Vec<crate::hash::Hash>,
    /// Whole-stream BLAKE3 (raw content).
    pub stream_hash: crate::hash::Hash,
    /// Frozen file_hash construction: BLAKE3(concat chunk hashes in file order) (SPEC §5.1).
    pub file_hash: crate::hash::Hash,
}

impl StreamHash {
    /// Single-pass hash+chunk of an in-memory buffer.
    #[must_use]
    pub fn compute(buf: &[u8]) -> Self {
        let spans = FastCdc::cut(buf);
        let mut chunk_hashes = Vec::with_capacity(spans.len());
        let mut stream = blake3::Hasher::new();
        stream.update(buf);
        for s in &spans {
            let start = s.offset as usize;
            chunk_hashes.push(crate::hash::Hash::of(&buf[start..start + s.len as usize]));
        }
        let file_hash = crate::hash::Hash::file_hash_from_chunk_hashes(&chunk_hashes);
        StreamHash {
            spans,
            chunk_hashes,
            stream_hash: crate::hash::Hash::from_bytes(stream.finalize().into()),
            file_hash,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::Hash;

    fn avg_len(spans: &[ChunkSpan]) -> usize {
        let total: u64 = spans.iter().map(|s| u64::from(s.len)).sum();
        (total / spans.len() as u64) as usize
    }

    use rand::SeedableRng;

    #[test]
    fn empty_and_tiny_inputs() {
        assert!(FastCdc::cut(b"").is_empty());
        let spans = FastCdc::cut(b"tiny");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].len, 4);
    }

    #[test]
    fn size_distribution_on_random_data() {
        // 128MB of pseudo-random data → average chunk near 4MB (mask 2^22), all within bounds.
        let mut r = rand::rngs::StdRng::seed_from_u64(42);
        use rand::RngCore;
        let mut buf = vec![0u8; 128 * 1024 * 1024];
        r.fill_bytes(&mut buf);
        let spans = FastCdc::cut(&buf);
        assert_eq!(spans.first().unwrap().offset, 0);
        assert!(
            (8..=80).contains(&spans.len()),
            "got {} chunks",
            spans.len()
        );
        let avg = avg_len(&spans);
        assert!(
            avg > 2 * 1024 * 1024 && avg < 8 * 1024 * 1024,
            "avg chunk {avg} outside expected band"
        );
        assert!(spans.iter().all(|s| s.len as usize >= CHUNK_MIN
            || spans.len() == 1
            || s.offset as usize + s.len as usize == buf.len()));
        assert!(spans.iter().all(|s| s.len as usize <= CHUNK_MAX));
        // spans tile the buffer exactly
        let mut off = 0u64;
        for s in &spans {
            assert_eq!(s.offset, off);
            off += u64::from(s.len);
        }
        assert_eq!(off, buf.len() as u64);
    }

    #[test]
    fn determinism_same_bytes_same_cuts() {
        let buf: Vec<u8> = (0..8 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
        assert_eq!(FastCdc::cut(&buf), FastCdc::cut(&buf));
    }

    #[test]
    fn spans_before_insertion_are_stable() {
        // CDC guarantee: a byte inserted mid-stream leaves all boundaries BEFORE it unchanged.
        let buf: Vec<u8> = (0..32 * 1024 * 1024)
            .map(|i| ((i * 7) % 255) as u8)
            .collect();
        let a = FastCdc::cut(&buf);
        assert!(a.len() >= 2, "test data must produce a cut");
        // insert exactly after the first cut so the prefix assertion is never vacuous
        let at = a[0].offset as usize + a[0].len as usize;
        let mut mutated = buf.clone();
        mutated.insert(at, 0xAB);
        let b = FastCdc::cut(&mutated);
        let a_before: Vec<ChunkSpan> = a
            .iter()
            .take_while(|s| s.offset + u64::from(s.len) <= at as u64)
            .cloned()
            .collect();
        let b_before: Vec<ChunkSpan> = b
            .iter()
            .take_while(|s| s.offset + u64::from(s.len) <= at as u64)
            .cloned()
            .collect();
        assert_eq!(a_before, b_before);
        // and reuse across versions stays high: identical prefix bytes
        let prefix_bytes: u64 = b_before.iter().map(|s| u64::from(s.len)).sum();
        assert!(prefix_bytes as f64 / buf.len() as f64 > 0.15);
    }

    #[test]
    fn streaming_matches_one_shot() {
        let buf: Vec<u8> = (0..24 * 1024 * 1024)
            .map(|i| ((i * 31 + 5) % 256) as u8)
            .collect();
        let one_shot = FastCdc::cut(&buf);
        let mut streamed = FastCdc::default();
        let mut spans = Vec::new();
        for part in buf.chunks(777_777) {
            streamed.push(part, &mut spans);
        }
        streamed.finish(&mut spans);
        assert_eq!(one_shot, spans);
    }

    #[test]
    fn stream_hash_file_hash_matches_frozen_construction() {
        let buf: Vec<u8> = (0..10 * 1024 * 1024)
            .map(|i| (i as u8).wrapping_mul(13))
            .collect();
        let sh = StreamHash::compute(&buf);
        assert_eq!(sh.chunk_hashes.len(), sh.spans.len());
        let expect = Hash::file_hash_from_chunk_hashes(&sh.chunk_hashes);
        assert_eq!(sh.file_hash, expect);
    }
}
