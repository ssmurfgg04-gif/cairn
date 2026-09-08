//! Cairn forward error correction (priority #3, relay reliability).
//!
//! What this is: k data shards + 1 XOR parity shard per group. Any single
//! lost shard in a group is rebuilt from the rest — no retransmit round
//! trip. That covers the dominant relay failure mode (isolated drops on
//! Wi-Fi/cellular) at 1/(k+1) overhead (k=8 → 12.5%).
//!
//! What this is NOT (yet): full LDPC/belief-propagation for burst losses.
//! Roadmap: replace `ParityGroup` internals with an LDPC code (regular
//! (3,6) ensemble, ~50 sum-product iterations) behind the SAME
//! `encode_group`/`recover` API, so callers don't change. The QUIC layer
//! already isolates framing from coding, which is where LDPC would slot in.

#![forbid(unsafe_code)]

use cairn_core::CairnError;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum FecError {
    #[error("need at least 1 data shard, got {0}")]
    Empty(usize),
    #[error("shard length mismatch: expected {expected}, got {got}")]
    LengthMismatch { expected: usize, got: usize },
    #[error("more than one shard missing — single parity cannot rebuild")]
    Unrecoverable,
    #[error("{0}")]
    Other(String),
}

impl From<FecError> for CairnError {
    fn from(e: FecError) -> Self {
        CairnError::new(cairn_core::ErrorKind::Internal, e.to_string())
    }
}

/// One coded group: k data shards + 1 parity shard, all equal length.
pub struct ParityGroup {
    /// Number of data shards per group.
    pub k: usize,
}

impl ParityGroup {
    pub fn new(k: usize) -> Result<Self, FecError> {
        if k == 0 {
            return Err(FecError::Empty(k));
        }
        Ok(Self { k })
    }

    /// Split `data` into k shards (zero-padded) and compute the XOR parity.
    /// Returns (shards[0..k], parity). `orig_len` lets `recover` trim padding.
    pub fn encode(&self, data: &[u8]) -> (Vec<Vec<u8>>, Vec<u8>, usize) {
        let orig_len = data.len();
        let shard_len = data.len().div_ceil(self.k);
        let mut shards = vec![vec![0u8; shard_len]; self.k];
        for (i, b) in data.iter().enumerate() {
            shards[i / shard_len][i % shard_len] = *b;
        }
        let mut parity = vec![0u8; shard_len];
        for s in &shards {
            for (p, b) in parity.iter_mut().zip(s.iter()) {
                *p ^= *b;
            }
        }
        (shards, parity, orig_len)
    }

    /// Rebuild when exactly one of shards[..k]+parity is missing.
    /// `present[i]` = Some(bytes) or None. Returns the k data shards.
    pub fn recover(&self, present: Vec<Option<Vec<u8>>>) -> Result<Vec<Vec<u8>>, FecError> {
        if present.len() != self.k + 1 {
            return Err(FecError::Other(format!(
                "expected {} shards, got {}",
                self.k + 1,
                present.len()
            )));
        }
        let missing: Vec<usize> = present
            .iter()
            .enumerate()
            .filter_map(|(i, s)| if s.is_none() { Some(i) } else { None })
            .collect();
        if missing.len() > 1 {
            return Err(FecError::Unrecoverable);
        }
        let shard_len = present.iter().flatten().next().map(Vec::len).unwrap_or(0);
        for s in present.iter().flatten() {
            if s.len() != shard_len {
                return Err(FecError::LengthMismatch {
                    expected: shard_len,
                    got: s.len(),
                });
            }
        }
        if missing.is_empty() {
            return Ok(present
                .into_iter()
                .map(Option::unwrap)
                .take(self.k)
                .collect());
        }
        // Rebuild: XOR of all present shards (missing one included as zero).
        let mut rebuilt = vec![0u8; shard_len];
        for s in present.iter().flatten() {
            for (r, b) in rebuilt.iter_mut().zip(s.iter()) {
                *r ^= *b;
            }
        }
        let missing_idx = missing[0];
        let mut rebuilt_opt = Some(rebuilt);
        let mut out = Vec::with_capacity(self.k);
        for (i, opt) in present.into_iter().take(self.k).enumerate() {
            if i == missing_idx {
                out.push(rebuilt_opt.take().unwrap_or_default());
            } else {
                out.push(opt.unwrap_or_default());
            }
        }
        Ok(out)
    }

    /// Join k shards and trim to the original length.
    pub fn join(shards: &[Vec<u8>], orig_len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(orig_len);
        for s in shards {
            let need = orig_len.saturating_sub(out.len());
            out.extend_from_slice(&s[..need.min(s.len())]);
            if out.len() >= orig_len {
                break;
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn roundtrip_no_loss() {
        let g = ParityGroup::new(8).unwrap();
        let data: Vec<u8> = (0..10_000u64)
            .map(|i| ((i * 2_654_435_761u64) % 251) as u8)
            .collect();
        let (shards, parity, orig) = g.encode(&data);
        let mut present: Vec<Option<Vec<u8>>> = shards.into_iter().map(Some).collect();
        present.push(Some(parity));
        let rec = g.recover(present).unwrap();
        assert_eq!(ParityGroup::join(&rec, orig), data);
    }

    #[test]
    fn recovers_any_single_data_loss() {
        let g = ParityGroup::new(4).unwrap();
        let data: Vec<u8> = (0..5_000u64).map(|i| ((i * 97) % 251) as u8).collect();
        let (shards, parity, orig) = g.encode(&data);
        for lost in 0..4 {
            let mut present: Vec<Option<Vec<u8>>> = shards.clone().into_iter().map(Some).collect();
            present.push(Some(parity.clone()));
            present[lost] = None;
            let rec = g.recover(present).unwrap();
            assert_eq!(ParityGroup::join(&rec, orig), data, "lost shard {lost}");
        }
    }

    #[test]
    fn recovers_lost_parity_trivially() {
        let g = ParityGroup::new(4).unwrap();
        let data = b"hello relay world, this is a test payload".to_vec();
        let (shards, _parity, orig) = g.encode(&data);
        let mut present: Vec<Option<Vec<u8>>> = shards.into_iter().map(Some).collect();
        present.push(None); // parity lost — data intact
        let rec = g.recover(present).unwrap();
        assert_eq!(ParityGroup::join(&rec, orig), data);
    }

    #[test]
    fn two_losses_are_unrecoverable() {
        let g = ParityGroup::new(4).unwrap();
        let (shards, parity, _orig) = g.encode(b"0123456789abcdef");
        let mut present: Vec<Option<Vec<u8>>> = shards.into_iter().map(Some).collect();
        present.push(Some(parity));
        present[0] = None;
        present[2] = None;
        assert!(matches!(g.recover(present), Err(FecError::Unrecoverable)));
    }

    #[test]
    fn rejects_empty_k() {
        assert!(ParityGroup::new(0).is_err());
    }

    proptest! {
        #[test]
        fn prop_single_loss_always_recovers(
            data in prop::collection::vec(any::<u8>(), 1..8192),
            k in 1usize..=16,
            lost_idx in 0usize..=16,
        ) {
            let g = ParityGroup::new(k).unwrap();
            let (shards, parity, orig) = g.encode(&data);
            let mut present: Vec<Option<Vec<u8>>> = shards.into_iter().map(Some).collect();
            present.push(Some(parity));

            let actual_lost = lost_idx % (k + 1);
            present[actual_lost] = None;

            let rec = g.recover(present).unwrap();
            let rebuilt = ParityGroup::join(&rec, orig);
            prop_assert_eq!(rebuilt, data);
        }
    }
}
