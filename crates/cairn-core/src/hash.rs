//! BLAKE3-256 content hashes (SPEC §5.1: all hashes are BLAKE3-256, hex-encoded on the wire).

use std::fmt;

/// A 32-byte BLAKE3 digest. Construction of higher-level hashes (`file_hash`, `manifest_hash`)
/// is frozen in SPEC §5.1/§6 and ADR-0004 — never change silently.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Hash(pub [u8; 32]);

impl Hash {
    /// Hash raw bytes.
    #[must_use]
    pub fn of(bytes: &[u8]) -> Self {
        Hash(blake3::hash(bytes).into())
    }

    /// Hash from raw 32 bytes.
    #[must_use]
    pub const fn from_bytes(b: [u8; 32]) -> Self {
        Hash(b)
    }

    /// Incremental hashing for the frozen `file_hash` construction: BLAKE3 over the
    /// concatenation of chunk hashes in file order.
    #[must_use]
    pub fn file_hash_from_chunk_hashes(chunk_hashes: &[Hash]) -> Self {
        let mut h = blake3::Hasher::new();
        for c in chunk_hashes {
            h.update(&c.0);
        }
        Hash(h.finalize().into())
    }

    /// Lowercase hex.
    #[must_use]
    pub fn hex(&self) -> String {
        hex_encode(&self.0)
    }

    /// Parse from lowercase hex (lenient about case).
    #[must_use]
    pub fn from_hex(s: &str) -> Option<Self> {
        let b = hex_decode(s)?;
        Self::from_slice(&b)
    }

    /// From exactly 32 bytes.
    #[must_use]
    pub const fn from_slice(b: &[u8]) -> Option<Self> {
        if b.len() == 32 {
            let mut out = [0u8; 32];
            let mut i = 0;
            while i < 32 {
                out[i] = b[i];
                i += 1;
            }
            Some(Hash(out))
        } else {
            None
        }
    }

    /// Two-char shard prefix for storage keys (`t{tenant}/c/{ab}/{hash}`).
    #[must_use]
    pub fn shard(&self) -> String {
        self.hex()[..2].to_string()
    }
}

impl fmt::Display for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.hex())
    }
}

impl fmt::Debug for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Hash({})", self.hex())
    }
}

/// Hex encode (lowercase).
#[must_use]
pub fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from_digit(u32::from(b >> 4), 16).unwrap_or('0'));
        s.push(char::from_digit(u32::from(b & 0xF), 16).unwrap_or('0'));
    }
    s
}

/// Hex decode (accepts upper/lower); `None` on odd length or non-hex.
#[must_use]
pub fn hex_decode(s: &str) -> Option<Vec<u8>> {
    let b = s.as_bytes();
    if b.len() % 2 != 0 {
        return None;
    }
    let val = |c: u8| -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    };
    let mut out = Vec::with_capacity(b.len() / 2);
    for pair in b.chunks(2) {
        out.push((val(pair[0])? << 4) | val(pair[1])?);
    }
    Some(out)
}

const B64_ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Base64 (RFC 4648 standard alphabet, `=` padding). Used for the
/// `x-amz-checksum-sha256` header, which S3 specifies as base64 — MinIO rejects
/// hex values with `InvalidArgument: Invalid checksum provided` (quirk S1,
/// proven against the pinned MinIO: hex 400, base64 200, wrong-base64 400
/// `XAmzContentChecksumMismatch`).
#[must_use]
pub fn b64_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = chunk.get(1).map_or(0, |v| u32::from(*v));
        let b2 = chunk.get(2).map_or(0, |v| u32::from(*v));
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(B64_ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(B64_ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            B64_ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            B64_ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn b64_matches_rfc4648_known_answers() {
        // RFC 4648 §10 test vectors
        assert_eq!(b64_encode(b""), "");
        assert_eq!(b64_encode(b"f"), "Zg==");
        assert_eq!(b64_encode(b"fo"), "Zm8=");
        assert_eq!(b64_encode(b"foo"), "Zm9v");
        assert_eq!(b64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(b64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(b64_encode(b"foobar"), "Zm9vYmFy");
        // DE AD BE EF — classic known answer
        assert_eq!(b64_encode(&[0xDE, 0xAD, 0xBE, 0xEF]), "3q2+7w==");
        // SHA-256-sized input (32 bytes → 44 chars, one '=' pad)
        let d: Vec<u8> = (0..32u8).collect();
        let b64 = b64_encode(&d);
        assert_eq!(b64.len(), 44);
        assert!(b64.ends_with('='));
        // base64 of a hex digest must equal base64 of the raw bytes
        let round = hex_decode(&hex_encode(&d)).unwrap();
        assert_eq!(b64_encode(&round), b64);
    }

    #[test]
    fn hash_is_stable() {
        let a = Hash::of(b"cairn");
        assert_eq!(a, Hash::of(b"cairn"));
        assert_eq!(a.hex().len(), 64);
        assert_eq!(Hash::from_hex(&a.hex()), Some(a));
    }

    #[test]
    fn file_hash_construction_is_order_sensitive() {
        let c1 = Hash::of(b"chunk-one");
        let c2 = Hash::of(b"chunk-two");
        let f1 = Hash::file_hash_from_chunk_hashes(&[c1, c2]);
        let f2 = Hash::file_hash_from_chunk_hashes(&[c2, c1]);
        assert_ne!(f1, f2);
        // equals hashing the raw concatenated 64 bytes
        let mut raw = [0u8; 64];
        raw[..32].copy_from_slice(&c1.0);
        raw[32..].copy_from_slice(&c2.0);
        assert_eq!(f1, Hash::of(&raw));
    }

    #[test]
    fn hex_roundtrip() {
        let b = vec![0u8, 1, 254, 255, 0x0a];
        assert_eq!(hex_decode(&hex_encode(&b)).unwrap(), b);
        assert!(hex_decode("zz").is_none());
        assert!(hex_decode("abc").is_none());
    }
}
