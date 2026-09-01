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

/// Base64 decode — strict inverse of [`b64_decode`]'s encoder (RFC 4648 standard
/// alphabet, `=` padding only at the end). `None` on any invalid char, misplaced
/// padding, or impossible length. Used by the dev object endpoint to decode the
/// `x-amz-checksum-sha256` header the daemon sends (quirk S1: base64 on the wire).
#[must_use]
pub fn b64_decode(s: &str) -> Option<Vec<u8>> {
    if s.len() % 4 != 0 {
        return None;
    }
    let val = |c: u8| -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some(u32::from(c - b'A')),
            b'a'..=b'z' => Some(u32::from(c - b'a') + 26),
            b'0'..=b'9' => Some(u32::from(c - b'0') + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    };
    let bytes = s.as_bytes();
    // padding: only the final 1 or 2 chars may be '='
    let pad = bytes.iter().rev().take_while(|&&c| c == b'=').count();
    if pad > 2 {
        return None;
    }
    let body = &bytes[..bytes.len() - pad];
    if body.iter().any(|&c| val(c).is_none()) {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for chunk in bytes.chunks(4) {
        if chunk.len() != 4 {
            return None; // unreachable given the len%4 check, but stay strict
        }
        let n0 = val(chunk[0])?;
        let n1 = val(chunk[1])?;
        if chunk[2] == b'=' {
            if chunk[3] != b'=' || pad != 2 {
                return None;
            }
            out.push(((n0 << 2) | (n1 >> 4)) as u8);
        } else if chunk[3] == b'=' {
            if pad != 1 {
                return None;
            }
            let n2 = val(chunk[2])?;
            out.push(((n0 << 2) | (n1 >> 4)) as u8);
            out.push((((n1 & 0xF) << 4) | (n2 >> 2)) as u8);
        } else {
            let n2 = val(chunk[2])?;
            let n3 = val(chunk[3])?;
            out.push(((n0 << 2) | (n1 >> 4)) as u8);
            out.push((((n1 & 0xF) << 4) | (n2 >> 2)) as u8);
            out.push((((n2 & 0x3) << 6) | n3) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn b64_decode_roundtrips_and_is_strict() {
        // RFC 4648 §10 vectors, both directions
        for (raw, enc) in [
            (&b""[..], ""),
            (&b"f"[..], "Zg=="),
            (&b"fo"[..], "Zm8="),
            (&b"foo"[..], "Zm9v"),
            (&b"foob"[..], "Zm9vYg=="),
            (&b"fooba"[..], "Zm9vYmE="),
            (&b"foobar"[..], "Zm9vYmFy"),
        ] {
            assert_eq!(b64_encode(raw), enc);
            assert_eq!(b64_decode(enc).as_deref(), Some(raw));
        }
        // 32 raw bytes (the checksum shape): roundtrip both ways
        let raw: Vec<u8> = (0..32u8).collect();
        let enc = b64_encode(&raw);
        assert_eq!(enc.len(), 44);
        assert!(enc.ends_with('='));
        assert_eq!(b64_decode(&enc).as_deref(), Some(raw.as_slice()));
        // strictness
        assert_eq!(b64_decode("Zg="), None, "bad length");
        assert_eq!(b64_decode("Zg!="), None, "invalid char");
        assert_eq!(b64_decode("=Zg="), None, "misplaced padding");
        assert_eq!(b64_decode("Z=== Zm9v"), None, "space is not alphabet");
        assert_eq!(b64_decode("ZZ==Zm9v"), None, "padding mid-string");
        // a hex-encoded digest is a DIFFERENT byte string than the raw digest —
        // the exact failure mode the server's accept arm had (hex_decode on b64)
        let digest_raw = b64_decode(&b64_encode(&[7u8; 32])).expect("valid b64");
        assert_ne!(hex_encode(&[7u8; 32]).into_bytes(), digest_raw);
    }

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
