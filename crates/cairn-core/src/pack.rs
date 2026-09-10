//! Pack object format v1 (SPEC §12, ADR-0012): ported concepts from git packfile+idx
//! (versioned byte, verified read-back). Parser is pure + fuzz-targeted (§15.5).
//!
//! Layout: `"CPCK" | ver=1 | u32 count | (u32 len, hash 32, data)*`

use crate::hash::Hash;
use crate::{CairnError, ErrorKind};

/// Pack magic.
pub const PACK_MAGIC: &[u8; 4] = b"CPCK";
/// Pack format version byte (changes are protocol changes, ADR-0012).
pub const PACK_VERSION: u8 = 1;

/// Serialize a pack from `(hash_hex, bytes)` pairs.
#[must_use]
pub fn build_pack(objects: &[(String, Vec<u8>)]) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(PACK_MAGIC);
    buf.push(PACK_VERSION);
    buf.extend_from_slice(&(objects.len() as u32).to_le_bytes());
    for (hash, bytes) in objects {
        buf.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        if let Some(h) = Hash::from_hex(hash) {
            buf.extend_from_slice(&h.0);
        } else {
            buf.extend_from_slice(&[0u8; 32]);
        }
        buf.extend_from_slice(bytes);
    }
    buf
}

/// Parse a pack into (hash_hex → bytes). Never panics on arbitrary input (fuzz gate).
pub fn parse_pack(bytes: &[u8]) -> Result<Vec<(String, Vec<u8>)>, CairnError> {
    let err = || CairnError::new(ErrorKind::ManifestFormat, "pack parse failed");
    if bytes.len() < 9 || &bytes[0..4] != PACK_MAGIC || bytes[4] != PACK_VERSION {
        return Err(err());
    }
    let n = u32::from_le_bytes([bytes[5], bytes[6], bytes[7], bytes[8]]) as usize;
    let mut pos = 9;
    // Each entry consumes at least 36 bytes of input (4 len + 32 hash), so a
    // valid pack of this size cannot hold more entries than the input admits.
    // Cap the pre-allocation at that input-derived bound — the raw count is
    // untrusted, and previously drove Vec::with_capacity to a ~30GB request
    // on a 9-byte input (pack_index_parse fuzz target, run #152: count
    // 0x2824bf00 = 673M entries x 48B tuple = ASan OOM abort).
    let cap = n.min(bytes.len().saturating_sub(pos) / 36);
    let mut out = Vec::with_capacity(cap);
    for _ in 0..n {
        if pos + 36 > bytes.len() {
            return Err(err());
        }
        let len = u32::from_le_bytes([bytes[pos], bytes[pos + 1], bytes[pos + 2], bytes[pos + 3]])
            as usize;
        let hash = Hash::from_slice(&bytes[pos + 4..pos + 36]).ok_or_else(err)?;
        pos += 36;
        if pos + len > bytes.len() {
            return Err(err());
        }
        out.push((hash.hex(), bytes[pos..pos + len].to_vec()));
        pos += len;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_bounds() {
        let objs = vec![
            (Hash::of(b"a").hex(), b"alpha".to_vec()),
            (Hash::of(b"b").hex(), b"beta-longer".to_vec()),
        ];
        let pack = build_pack(&objs);
        assert_eq!(parse_pack(&pack).unwrap(), objs);
        assert!(parse_pack(b"").is_err());
        assert!(parse_pack(&pack[..8]).is_err());
        // truncated body → bounds-checked
        assert!(parse_pack(&pack[..pack.len() - 1]).is_err());
    }

    #[test]
    fn parse_huge_count_does_not_oom() {
        // Regression (pack_index_parse fuzz target, run #152): a 9-byte
        // input "CPCK v1 count=0x2824bf00" made Vec::with_capacity(count)
        // request ~30GB (673M entries x 48B tuple) and ASan aborted with
        // out-of-memory. The count must be capped at the input-derived
        // bound; the parse itself has to return Err.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(PACK_MAGIC);
        bytes.push(PACK_VERSION);
        bytes.extend_from_slice(&0x2824bf00u32.to_le_bytes());
        assert_eq!(bytes.len(), 9);
        // no entries at all — must Err, never pre-allocate 673M slots
        assert!(parse_pack(&bytes).is_err());
        // same for a truncated single-entry pack with a huge count
        bytes.extend_from_slice(&[0u8; 8]);
        assert!(parse_pack(&bytes).is_err());
    }
}
