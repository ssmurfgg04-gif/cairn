//! Join codes — the swarm's admission ticket (ADR-0017 §7).
//!
//! A join code is the human-shareable form of the cluster secret. The person
//! hosting the signal server either picks a code or has one generated; every
//! other node must present *that same code* to join the swarm. Codes look
//! like:
//!
//! ```text
//! 7XQ3-M9AF-K2HD-8VTE-4NBR-J6WY-CFSM-QZ7E
//! ```
//!
//! **Format** — 20 bytes, Crockford Base32-encoded into 32 symbols (shown as
//! eight dash-separated groups of four):
//!
//! ```text
//! [ 18 CSPRNG bytes of entropy ][ 2-byte CRC-16/ARC checksum ]
//! ```
//!
//! 144 bits of entropy — the brute-force economics of a 24-character random
//! password, with an alphabet that excludes the transcription-error letters
//! (`I` `L` `O` `U` never appear; on input they are aliased to `1` `1` `0` /
//! rejected).
//!
//! **The two-layer check** — why both a local CRC and a remote HMAC:
//!
//! * The CRC is for *humans*: a mistyped code fails *locally, instantly, and
//!   with an actionable message* ("checksum failed — re-check the code you
//!   were sent") instead of a 2-second network timeout. CRC-16/ARC detects
//!   all bursts up to 16 bits, which covers every single-symbol typo and
//!   nearly all adjacent-transposition typos in a 32-symbol codeword.
//! * The remote HMAC binding (registrations are HMAC-SHA256'd under the
//!   cluster key) is for *adversaries*: a wrong-but-well-formed code is
//!   dropped **silently** by the signal server — no reply, no oracle. An
//!   attacker probing codes cannot distinguish "bad code" from "server
//!   down".
//!
//! **Key derivation** — the cluster key is never the code itself:
//!
//! ```text
//! cluster_key = blake3::derive_key("cairn-p2p-join/v1", code_bytes)
//! ```
//!
//! The context string binds the derived key to this purpose; a join code can
//! never be confused with (or collide semantically with) the per-pair session
//! KDF contexts in [`crate::crypto`].
//!
//! **End-to-end effect**: without the code a node cannot register, so it never
//! appears in any member's signal table, so its session HELLOs are
//! fail-closed-ignored by every peer ([`crate::swarm`]). Strangers are not
//! merely unable to *fetch* blocks — they cannot establish so much as a
//! session. Revoking a leaked code is cheap: the host restarts `cairn signal`
//! with a fresh `--join-code` (or lets one be generated); every node holding
//! the old code is locked out at the next registration.

#![forbid(unsafe_code)]

use std::fmt;

/// Bytes of pure entropy in a join code (before the 2-byte checksum).
const ENTROPY_LEN: usize = 18;
/// Total decoded length: entropy + CRC-16/ARC checksum.
pub const CODE_LEN: usize = ENTROPY_LEN + 2;
/// Symbols in the canonical display form (160 bits / 5 bits per symbol).
const SYMBOLS: usize = (CODE_LEN * 8) / 5; // 32

/// Crockford Base32 alphabet: 0-9 + A-Z minus I, L, O, U.
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// BLAKE3 KDF context binding join codes to cluster-key derivation.
const KDF_CONTEXT: &str = "cairn-p2p-join/v1";

/// Why a join code string was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JoinCodeError {
    /// Wrong number of symbols after normalization (dashes/spaces stripped).
    Length {
        /// Symbol count actually found.
        got: usize,
        /// Symbol count required (32).
        want: usize,
    },
    /// A character that is not a Crockford symbol and not a known alias.
    /// `ch` is the offending character as typed.
    InvalidChar {
        /// The offending character.
        ch: char,
        /// 1-based position in the normalized string.
        pos: usize,
    },
    /// Decoded fine but the CRC-16/ARC checksum does not match — almost
    /// always a typo in an otherwise valid-looking code.
    Checksum,
}

impl fmt::Display for JoinCodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            JoinCodeError::Length { got, want } => write!(
                f,
                "join code must be exactly {want} symbols (got {got} after removing dashes/spaces)"
            ),
            JoinCodeError::InvalidChar { ch, pos } => write!(
                f,
                "join code has an invalid character {ch:?} at position {pos} \
                 (allowed: 0-9 A-Z except I, L, O, U)"
            ),
            JoinCodeError::Checksum => write!(
                f,
                "join code checksum failed — re-check the code you were sent \
                 (a typo changed its meaning)"
            ),
        }
    }
}

impl std::error::Error for JoinCodeError {}

/// A parsed or generated join code. The cluster key is derived on demand;
/// the code itself never leaves this type except via its display form.
#[derive(Clone, PartialEq, Eq)]
pub struct JoinCode {
    code: [u8; CODE_LEN],
}

impl JoinCode {
    /// Generate a fresh, never-before-seen join code (CSPRNG-backed via
    /// orion's `secure_rand_bytes`, which is `getrandom` underneath).
    pub fn generate() -> Self {
        let mut code = [0u8; CODE_LEN];
        orion::util::secure_rand_bytes(&mut code[..ENTROPY_LEN])
            .expect("OS CSPRNG unavailable: cannot generate join codes");
        let crc = crc16_arc(&code[..ENTROPY_LEN]);
        code[ENTROPY_LEN..].copy_from_slice(&crc.to_le_bytes());
        JoinCode { code }
    }

    /// Parse a code as a human would type or paste it: case-insensitive,
    /// dashes/underscores/spaces ignored, Crockford aliases `I`→`1`, `L`→`1`,
    /// `O`→`0` accepted. Anything else (including `U`) is a hard error.
    ///
    /// Fails *locally and specifically* — [`JoinCodeError::Checksum`] means
    /// "well-formed but mistyped", which is the fast path to good UX; a code
    /// that parses but is simply *wrong* (not yours) still fails remotely and
    /// silently at the signal server.
    pub fn parse(s: &str) -> Result<Self, JoinCodeError> {
        let mut symbols = [0u8; SYMBOLS];
        let mut n = 0usize;
        for ch in s.chars() {
            let up = ch.to_ascii_uppercase();
            match up {
                // separators: nothing to accumulate (each match arm ends the
                // iteration, so an explicit continue would be redundant)
                '-' | '_' | ' ' | '\t' | '\r' | '\n' => {}
                'I' | 'L' => {
                    if n >= SYMBOLS {
                        return Err(JoinCodeError::Length {
                            got: n + 1,
                            want: SYMBOLS,
                        });
                    }
                    symbols[n] = 1;
                    n += 1;
                }
                'O' => {
                    if n >= SYMBOLS {
                        return Err(JoinCodeError::Length {
                            got: n + 1,
                            want: SYMBOLS,
                        });
                    }
                    symbols[n] = 0;
                    n += 1;
                }
                'U' => {
                    return Err(JoinCodeError::InvalidChar { ch, pos: n + 1 });
                }
                _ => {
                    let v = ALPHABET
                        .iter()
                        .position(|&a| a == up as u8)
                        .ok_or(JoinCodeError::InvalidChar { ch, pos: n + 1 })?;
                    if n >= SYMBOLS {
                        return Err(JoinCodeError::Length {
                            got: n + 1,
                            want: SYMBOLS,
                        });
                    }
                    symbols[n] = v as u8;
                    n += 1;
                }
            }
        }
        if n != SYMBOLS {
            return Err(JoinCodeError::Length {
                got: n,
                want: SYMBOLS,
            });
        }
        // 32 symbols x 5 bits = 160 bits = 20 bytes, MSB-first in 8-symbol
        // groups (v holds each group's 40 bits in its low bits)
        let mut code = [0u8; CODE_LEN];
        for (g, grp) in symbols.chunks(8).enumerate() {
            let mut v: u64 = 0;
            for &s in grp {
                v = (v << 5) | u64::from(s);
            }
            for i in (0..5).rev() {
                code[g * 5 + i] = (v & 0xff) as u8;
                v >>= 8;
            }
        }
        let (entropy, crc_bytes) = code.split_at(ENTROPY_LEN);
        let want = u16::from_le_bytes([crc_bytes[0], crc_bytes[1]]);
        if crc16_arc(entropy) != want {
            return Err(JoinCodeError::Checksum);
        }
        Ok(JoinCode { code })
    }

    /// Derive the 32-byte cluster key this code grants. The signal server
    /// and every admitted node derive the identical key from the identical
    /// code; the code itself never travels on the wire.
    pub fn cluster_key(&self) -> [u8; 32] {
        blake3::derive_key(KDF_CONTEXT, &self.code)
    }

    /// Raw code bytes (entropy + checksum). For tests and storage; the
    /// cluster key is the only form that should touch protocol code.
    pub fn as_bytes(&self) -> &[u8; CODE_LEN] {
        &self.code
    }

    /// Rebuild from raw bytes previously obtained via [`JoinCode::as_bytes`].
    /// The checksum is re-validated: stored bytes are no more trusted than
    /// typed ones.
    pub fn from_bytes(raw: &[u8]) -> Result<Self, JoinCodeError> {
        if raw.len() != CODE_LEN {
            return Err(JoinCodeError::Length {
                got: raw.len() * 8 / 5, // bytes are not symbols; report scaled
                want: SYMBOLS,
            });
        }
        let mut code = [0u8; CODE_LEN];
        code.copy_from_slice(raw);
        let (entropy, crc_bytes) = code.split_at(ENTROPY_LEN);
        let want = u16::from_le_bytes([crc_bytes[0], crc_bytes[1]]);
        if crc16_arc(entropy) != want {
            return Err(JoinCodeError::Checksum);
        }
        Ok(JoinCode { code })
    }

    /// The canonical display form: 32 symbols in 8 dash-separated groups.
    pub fn display(&self) -> String {
        self.to_string()
    }
}

impl fmt::Display for JoinCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let symbols = encode_symbols(&self.code);
        for (i, s) in symbols.iter().enumerate() {
            if i > 0 && i % 4 == 0 {
                f.write_str("-")?;
            }
            f.write_str(std::str::from_utf8(&[*s]).expect("alphabet is ascii"))?;
        }
        Ok(())
    }
}

impl fmt::Debug for JoinCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // join codes are credentials: render the full code (it is meant to be
        // shared with humans) but keep Debug visually distinct from a String
        write!(f, "JoinCode({self})")
    }
}

fn encode_symbols(code: &[u8; CODE_LEN]) -> [u8; SYMBOLS] {
    let mut out = [0u8; SYMBOLS];
    for (g, grp) in code.chunks(5).enumerate() {
        // pack 5 bytes (40 bits) into v's low bits, then slice 5-bit symbols
        // off the top MSB-first
        let mut v: u64 = 0;
        for &b in grp {
            v = (v << 8) | u64::from(b);
        }
        for i in (0..8).rev() {
            out[g * 8 + i] = ALPHABET[(v & 0x1f) as usize];
            v >>= 5;
        }
        debug_assert_eq!(v, 0, "40 bits fully consumed");
    }
    out
}

/// CRC-16/ARC: poly 0x8005 (reflected 0xA001), init 0x0000, reflected in/out,
/// xorout 0 — the classic "plain crc16". Detects all error bursts ≤ 16 bits.
fn crc16_arc(data: &[u8]) -> u16 {
    let mut crc: u16 = 0;
    for &b in data {
        crc ^= u16::from(b);
        for _ in 0..8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xA001;
            } else {
                crc >>= 1;
            }
        }
    }
    crc
}

/// Derive a cluster key directly from a typed code string (parse + KDF).
/// The one-liner the CLI uses on the join side.
pub fn cluster_key_from_str(s: &str) -> Result<[u8; 32], JoinCodeError> {
    Ok(JoinCode::parse(s)?.cluster_key())
}

/// Constant-time equality of two cluster keys (defense-in-depth: cluster
/// keys are compared in tests and tooling; keep the habit uniform).
pub fn cluster_key_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
    orion::util::secure_cmp(a, b).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_display_parse_roundtrip() {
        for _ in 0..64 {
            let code = JoinCode::generate();
            let s = code.display();
            assert_eq!(s.len(), 39, "8 groups x 4 symbols + 7 dashes");
            let back = JoinCode::parse(&s).unwrap();
            assert_eq!(back.as_bytes(), code.as_bytes());
            assert_eq!(back, code);
        }
    }

    #[test]
    fn display_never_uses_ambiguous_letters() {
        for _ in 0..64 {
            let s = JoinCode::generate().display();
            assert!(!s.contains('I') && !s.contains('L') && !s.contains('O') && !s.contains('U'));
        }
    }

    #[test]
    fn human_typing_is_forgiving() {
        let code = JoinCode::generate();
        let canonical = code.display();
        // lowercase, dots as separators, spaces, and the I/L/O aliases
        let sloppy = canonical
            .to_lowercase()
            .replace('-', " ")
            .replace('0', "o")
            .replace('1', "l");
        let back = JoinCode::parse(&sloppy).unwrap();
        assert_eq!(
            back.as_bytes(),
            code.as_bytes(),
            "alias + case + separator tolerant"
        );
    }

    #[test]
    fn single_symbol_typo_fails_checksum() {
        let code = JoinCode::generate();
        let s = code.display();
        // mutate every symbol position once; all must be rejected as
        // Checksum or InvalidChar (never silently accepted)
        for pos in (0..s.len()).filter(|i| s.as_bytes()[*i] != b'-') {
            let orig = s.as_bytes()[pos];
            // swap to a different valid symbol
            let repl: char = if orig == b'7' { '3' } else { '7' };
            let t = set_char(&s, pos, repl);
            match JoinCode::parse(&t) {
                Ok(c) => panic!("typo at {pos} accepted: {s} -> {t} = {}", c.display()),
                Err(JoinCodeError::Checksum) | Err(JoinCodeError::InvalidChar { .. }) => {}
                Err(other) => panic!("unexpected error for typo: {other:?}"),
            }
        }
    }

    fn set_char(s: &str, pos: usize, repl: char) -> String {
        s.chars()
            .enumerate()
            .map(|(i, c)| if i == pos { repl } else { c })
            .collect()
    }

    #[test]
    fn adjacent_transposition_usually_caught() {
        // swap two adjacent symbols in a group: CRC-16/ARC catches all
        // two-bit and virtually all two-symbol errors in this frame size
        let code = JoinCode::generate();
        let mut s: Vec<u8> = code.display().into_bytes();
        // find a pair of distinct adjacent non-dash symbols
        for i in 0..s.len() - 1 {
            if s[i] != b'-' && s[i + 1] != b'-' && s[i] != s[i + 1] {
                s.swap(i, i + 1);
                let t = String::from_utf8(s).unwrap();
                assert!(
                    JoinCode::parse(&t).is_err(),
                    "transposition at {i} accepted: {t}"
                );
                return;
            }
        }
    }

    #[test]
    fn wrong_length_rejected_with_count() {
        let s = JoinCode::generate().display();
        let short: String = s.chars().filter(|c| *c != '-').take(31).collect();
        match JoinCode::parse(&short) {
            Err(JoinCodeError::Length { got: 31, want: 32 }) => {}
            other => panic!("expected Length(31), got {other:?}"),
        }
        let long = format!("{s}0");
        assert!(matches!(
            JoinCode::parse(&long),
            Err(JoinCodeError::Length { .. })
        ));
    }

    #[test]
    fn u_and_symbols_outside_alphabet_rejected() {
        let s: String = JoinCode::generate()
            .display()
            .chars()
            .filter(|c| *c != '-')
            .collect();
        let with_u = format!("{}U", &s[..s.len() - 1]);
        assert!(matches!(
            JoinCode::parse(&with_u),
            Err(JoinCodeError::InvalidChar { ch: 'U', .. })
        ));
        // punctuation is not a separator
        let with_bang = format!("{}!", &s[..s.len() - 1]);
        assert!(matches!(
            JoinCode::parse(&with_bang),
            Err(JoinCodeError::InvalidChar { ch: '!', .. })
        ));
    }

    #[test]
    fn cluster_key_is_stable_and_distinct() {
        let a = JoinCode::generate();
        let b = JoinCode::generate();
        assert_ne!(
            a.as_bytes(),
            b.as_bytes(),
            "144-bit collision in two draws?!"
        );
        assert_eq!(a.cluster_key(), a.cluster_key(), "deterministic KDF");
        assert_ne!(a.cluster_key(), b.cluster_key());
        // the code is NOT the key: derivation is context-bound
        assert_ne!(&a.cluster_key()[..], a.as_bytes());
    }

    #[test]
    fn cluster_key_from_str_matches_parse_then_derive() {
        let code = JoinCode::generate();
        assert_eq!(
            cluster_key_from_str(&code.display()).unwrap(),
            code.cluster_key()
        );
        assert!(cluster_key_from_str("not-a-code").is_err());
    }

    #[test]
    fn bytes_roundtrip_revalidates() {
        let code = JoinCode::generate();
        let back = JoinCode::from_bytes(code.as_bytes()).unwrap();
        assert_eq!(back, code);
        // tampered stored bytes: flip one entropy bit
        let mut raw = *code.as_bytes();
        raw[0] ^= 1;
        assert!(matches!(
            JoinCode::from_bytes(&raw),
            Err(JoinCodeError::Checksum)
        ));
        assert!(matches!(
            JoinCode::from_bytes(&raw[..10]),
            Err(JoinCodeError::Length { .. })
        ));
    }

    #[test]
    fn constant_time_eq() {
        let a = JoinCode::generate();
        assert!(cluster_key_eq(&a.cluster_key(), &a.cluster_key()));
        let b = JoinCode::generate();
        assert!(!cluster_key_eq(&a.cluster_key(), &b.cluster_key()));
    }

    #[test]
    fn crc16_arc_known_vectors() {
        // standard check values for CRC-16/ARC
        assert_eq!(crc16_arc(b"123456789"), 0xBB3D);
        assert_eq!(crc16_arc(b""), 0x0000);
    }

    #[test]
    fn parse_rejects_huge_input_fast() {
        // 10k symbols of garbage: must not panic, must error
        let big = "A".repeat(10_000);
        assert!(matches!(
            JoinCode::parse(&big),
            Err(JoinCodeError::Length { .. })
        ));
    }
}
