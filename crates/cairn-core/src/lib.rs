//! Cairn core — pure library crate: chunking, hashing, manifests, compression, bloom filters,
//! error taxonomy, time/ids/path utilities. No I/O beyond mmap for large files (SPEC §6).
//!
//! Everything here must stay pure and deterministic: same bytes → same hashes (SPEC §6.4).

#![forbid(unsafe_code)]

pub mod bloom;
pub mod chunker;
pub mod clock;
pub mod compress;
pub mod error;
pub mod hash;
pub mod ids;
pub mod manifest;
pub mod normalize;
pub mod pack;
pub mod pathutil;

pub use error::{CairnError, ErrorKind, RetryClass};

/// Chunker pipeline version — changes here change chunk identities (protocol-breaking, ADR-0003).
pub const CHUNKER_VERSION: u8 = 1;

/// Manifest object serialization format version (SPEC §5.1, ADR-0004).
pub const MANIFEST_FORMAT_VERSION: u8 = 2;

/// Max entries per manifest object before fanning out (SPEC §5.1).
pub const MANIFEST_MAX_ENTRIES: usize = 8_192;

/// FastCDC parameters (SPEC §5.1/§6): min 1MB, avg 4MB (mask 2^22), max 16MB.
pub const CHUNK_MIN: usize = 1024 * 1024;
pub const CHUNK_AVG: usize = 4 * 1024 * 1024; // boundary mask 2^22
pub const CHUNK_MAX: usize = 16 * 1024 * 1024;

pub use chunker::{CHUNK_AVG_FINE, CHUNK_MAX_FINE, CHUNK_MIN_FINE};

/// Header cache: first 2MB + last 1MB per pointer (SPEC §5.1).
pub const HEADER_HEAD_BYTES: usize = 2 * 1024 * 1024;
pub const HEADER_TAIL_BYTES: usize = 1024 * 1024;

/// I1 latency target, cached header serve (SPEC §2).
pub const I1_TARGET_CACHED_MS: f64 = 50.0;

/// Compression sniff table (SPEC §6): media → none, text-ish → zstd3, NLE project files → dict.
pub fn compression_policy_for(path: &str) -> compress::Compression {
    compress::policy_for(path)
}
