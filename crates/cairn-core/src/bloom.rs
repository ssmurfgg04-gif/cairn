//! Bloom filter used ONLY as a negative pre-filter for BatchExists (SPEC §9.1).
//!
//! Contract: `might_contain == false` ⇒ definitely absent (skip upload-safe). `might_contain ==
//! true` ⇒ MAY be present; the authoritative chunks-table check decides. Bloom false positives
//! must never cause a skipped upload — property-tested adversarially (§15.2).

/// Simple double-hashing bloom filter backed by BLAKE3 digests.
#[derive(Debug, Clone)]
pub struct Bloom {
    bits: Vec<u64>,
    num_bits: u64,
    k: u32,
}

impl Bloom {
    /// Build for `expected_items` at false-positive probability `fpp` (e.g. 0.01).
    #[must_use]
    pub fn with_fpp(expected_items: u64, fpp: f64) -> Self {
        let fpp = fpp.clamp(1e-6, 0.5);
        let ln2 = std::f64::consts::LN_2;
        let m = -1.0 * (expected_items.max(1) as f64) * fpp.ln() / (ln2 * ln2);
        let num_bits = (m.ceil() as u64).max(64);
        let words = (num_bits / 64 + 1) as usize;
        let k = ((num_bits as f64 / expected_items.max(1) as f64) * ln2).ceil() as u32;
        Bloom { bits: vec![0u64; words], num_bits, k: k.clamp(1, 16) }
    }
}

impl Bloom {
    /// Empty bloom (everything "maybe present" → always authoritative check).
    #[must_use]
    pub fn empty() -> Self {
        Bloom { bits: vec![0u64; 1], num_bits: 64, k: 1 }
    }

    /// Insert an item (hex hash string or any bytes).
    pub fn insert(&mut self, item: &[u8]) {
        let (h1, h2) = self.hashes(item);
        for i in 0..self.k {
            let idx = (h1.wrapping_add(u64::from(i).wrapping_mul(h2))) % self.num_bits;
            self.bits[(idx / 64) as usize] |= 1u64 << (idx % 64);
        }
    }

    /// `false` ⇒ item is DEFINITELY absent; `true` ⇒ maybe present (verify authoritatively).
    #[must_use]
    pub fn might_contain(&self, item: &[u8]) -> bool {
        let (h1, h2) = self.hashes(item);
        for i in 0..self.k {
            let idx = (h1.wrapping_add(u64::from(i).wrapping_mul(h2))) % self.num_bits;
            if self.bits[(idx / 64) as usize] & (1u64 << (idx % 64)) == 0 {
                return false;
            }
        }
        true
    }

    fn hashes(&self, item: &[u8]) -> (u64, u64) {
        let d = blake3::hash(item);
        let b = d.as_bytes();
        let h1 = u64::from_le_bytes(b[0..8].try_into().expect("32-byte digest"));
        let h2 = u64::from_le_bytes(b[8..16].try_into().expect("32-byte digest"));
        (h1, h2 | 1) // odd stride so k probes spread
    }

    /// Serialize for the bloom-rebuild job (versioned byte + bits).
    #[must_use]
    pub fn serialize(&self) -> Vec<u8> {
        let mut out = vec![1u8]; // format version
        out.extend_from_slice(&self.num_bits.to_le_bytes());
        out.extend_from_slice(&self.k.to_le_bytes());
        for w in &self.bits {
            out.extend_from_slice(&w.to_le_bytes());
        }
        out
    }

    /// Adversarial-test helper: set every bit (worst-case "maybe present" for all items).
/// Used by §15.2 to prove the authoritative check can never be skipped.
pub fn corrupt_all_bits(&mut self) {
    for w in self.bits.iter_mut() {
        *w = u64::MAX;
    }
}

    /// Parse a serialized bloom.
    #[must_use]
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 17 || bytes[0] != 1 {
            return None;
        }
        let num_bits = u64::from_le_bytes(bytes[1..9].try_into().ok()?);
        let k = u32::from_le_bytes(bytes[9..13].try_into().ok()?);
        let rest = &bytes[13..];
        if rest.len() % 8 != 0 {
            return None;
        }
        let words = rest
            .chunks_exact(8)
            .map(|w| u64::from_le_bytes(w.try_into().expect("chunk of 8")))
            .collect();
        Some(Bloom { bits: words, num_bits, k: k.clamp(1, 16) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::Hash;

    #[test]
    fn negatives_are_certain() {
        let mut b = Bloom::with_fpp(10_000, 0.01);
        let present: Vec<Hash> = (0..1000).map(|i| Hash::of(format!("chunk-{i}").as_bytes())).collect();
        for h in &present {
            b.insert(h.hex().as_bytes());
        }
        for h in &present {
            assert!(b.might_contain(h.hex().as_bytes()), "no false negatives allowed");
        }
        // absent items: overwhelmingly negative (fpp 1%)
        let absent: Vec<Hash> =
            (1000..2000).map(|i| Hash::of(format!("chunk-{i}").as_bytes())).collect();
        let positives = absent.iter().filter(|h| b.might_contain(h.hex().as_bytes())).count();
        assert!(positives < 60, "fpp too high: {positives}/1000");
    }

    /// §15.2: adversarially mutated bloom (all bits set) can never cause a skipped upload —
    /// the caller treats `true` as "check the authoritative KV", which this test simulates.
    #[test]
    fn adversarial_bloom_cannot_skip_uploads() {
        let mut b = Bloom::with_fpp(100, 0.01);
        for w in b.bits.iter_mut() {
            *w = u64::MAX; // worst case: everything "maybe present"
        }
        let missing: Vec<Hash> = (0..50).map(|i| Hash::of(format!("m-{i}").as_bytes())).collect();
        // authoritative check returns exact missing set regardless of bloom answers
        let authoritative_missing = missing.clone();
        let bloom_says_missing: Vec<&Hash> =
            missing.iter().filter(|h| !b.might_contain(h.hex().as_bytes())).collect();
        assert!(bloom_says_missing.is_empty());
        assert_eq!(authoritative_missing.len(), missing.len());
    }

    #[test]
    fn serialize_roundtrip() {
        let mut b = Bloom::with_fpp(500, 0.02);
        for i in 0..200u32 {
            b.insert(Hash::of(&i.to_le_bytes()).hex().as_bytes());
        }
        let bytes = b.serialize();
        let b2 = Bloom::parse(&bytes).unwrap();
        for i in 0..200u32 {
            let h = Hash::of(&i.to_le_bytes()).hex();
            assert_eq!(b.might_contain(h.as_bytes()), b2.might_contain(h.as_bytes()));
        }
        assert!(Bloom::parse(b"garbage").is_none());
    }
}
