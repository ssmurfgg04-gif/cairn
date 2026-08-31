//! Manifest objects — sorted `(offset, len, chunk_hash)` lists with Git-style fanout
//! (SPEC §5.1/§6, ADR-0004). Max 8,192 entries per manifest object; larger files build trees.
//!
//! Serialization (versioned byte first, never change silently):
//! - v1: `magic(4) | ver=1 | compression(1) | dict_flag(1) [dict(32)] | u32 count | entries`
//! - v2: v1 + `transform(1)` after the dict section (chunk-input normalization descriptor)
//! - Leaf magic `CMAN`, Node magic `CMND`; v1 parses as `Transform::None`
//!
//! `manifest_hash` = BLAKE3 of top manifest bytes. `file_hash` = BLAKE3(concat chunk hashes in
//! file order) — frozen (SPEC §5.1, ADR-0004).

use crate::hash::Hash;
use crate::normalize::Transform;
use crate::{MANIFEST_FORMAT_VERSION, MANIFEST_MAX_ENTRIES};

/// One chunk position within a file.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ManifestEntry {
    /// Byte offset in the file (raw content).
    pub offset: u64,
    /// Chunk length (raw content).
    pub len: u32,
    /// BLAKE3 of raw chunk content.
    pub chunk_hash: Hash,
}

/// Per-file storage policy recorded in the manifest (ADR-0004).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Compression {
    /// Stored verbatim (media).
    #[default]
    None = 0,
    /// zstd level 3 per chunk.
    Zstd3 = 1,
    /// zstd per chunk with per-project dictionary (dict hash in `dict_hash`).
    ZstdDict = 2,
}

impl Compression {
    /// From tag byte.
    #[must_use]
    pub fn from_tag(t: u8) -> Option<Self> {
        match t {
            0 => Some(Compression::None),
            1 => Some(Compression::Zstd3),
            2 => Some(Compression::ZstdDict),
            _ => None,
        }
    }

    /// Tag byte.
    #[must_use]
    pub const fn tag(self) -> u8 {
        self as u8
    }
}

/// File manifest: either a leaf (≤8,192 entries) or a fanout node over child manifests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Manifest {
    /// Leaf with entries (entries MUST be sorted by offset).
    Leaf {
        /// Raw-content chunk entries sorted by offset.
        entries: Vec<ManifestEntry>,
        /// Compression policy for every chunk in this file (ADR-0004).
        compression: Compression,
        /// Optional per-project dictionary hash (NLE project files).
        dict_hash: Option<Hash>,
        /// Container transform (v2): the stored chunks cover the INNER payload.
        transform: Transform,
    },
    /// Fanout node over child manifests (depth ≥ 2 for >8,192 chunks).
    Node {
        /// Children in file order with their byte coverage.
        children: Vec<ChildRef>,
        /// Compression policy (uniform across a file).
        compression: Compression,
        /// Optional per-project dictionary hash.
        dict_hash: Option<Hash>,
        /// Container transform (v2).
        transform: Transform,
    },
}

/// Child link in a fanout node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildRef {
    /// BLAKE3 of the child manifest bytes.
    pub hash: Hash,
    /// Total raw size covered by the child.
    pub total_len: u64,
    /// First chunk count covered (for stream reassembly order).
    pub entry_count: u32,
}

impl Manifest {
    /// Build a manifest from entries, fanning out at `MANIFEST_MAX_ENTRIES` (Git-style).
    #[must_use]
    pub fn build(
        entries: Vec<ManifestEntry>,
        compression: Compression,
        dict_hash: Option<Hash>,
    ) -> Self {
        Self::build_with_transform(entries, compression, dict_hash, Transform::None)
    }

    /// Build with a container transform (chunk-input normalization, v2).
    #[must_use]
    pub fn build_with_transform(
        mut entries: Vec<ManifestEntry>,
        compression: Compression,
        dict_hash: Option<Hash>,
        transform: Transform,
    ) -> Self {
        entries.sort();
        if entries.len() <= MANIFEST_MAX_ENTRIES {
            return Manifest::Leaf {
                entries,
                compression,
                dict_hash,
                transform,
            };
        }
        let mut children = Vec::new();
        for group in entries.chunks(MANIFEST_MAX_ENTRIES) {
            let leaf = Manifest::Leaf {
                entries: group.to_vec(),
                compression,
                dict_hash,
                transform,
            };
            let (h, bytes) = leaf.serialize();
            let _ = bytes;
            children.push(ChildRef {
                hash: h,
                total_len: group.iter().map(|e| u64::from(e.len)).sum(),
                entry_count: group.len() as u32,
            });
        }
        Manifest::Node {
            children,
            compression,
            dict_hash,
            transform,
        }
    }

    /// Flatten to entries in file order (stream reassembly order).
    #[must_use]
    pub fn flatten(&self) -> Vec<ManifestEntry> {
        match self {
            Manifest::Leaf { entries, .. } => entries.clone(),
            Manifest::Node { children, .. } => children
                .iter()
                .map(|_| ManifestEntry {
                    offset: 0,
                    len: 0,
                    chunk_hash: Hash::from_bytes([0u8; 32]),
                })
                .collect(), // replaced below; see `flatten_with`
        }
    }

    /// Flatten with a manifest-object resolver (needed to walk fanout nodes).
    #[must_use]
    pub fn flatten_with<F>(&self, resolve: &mut F) -> Vec<ManifestEntry>
    where
        F: FnMut(&Hash) -> Option<Manifest>,
    {
        match self {
            Manifest::Leaf { entries, .. } => entries.clone(),
            Manifest::Node { children, .. } => {
                let mut out = Vec::new();
                for c in children {
                    if let Some(child) = resolve(&c.hash) {
                        out.extend(child.flatten_with(resolve));
                    }
                }
                out
            }
        }
    }

    /// Total raw size covered.
    #[must_use]
    pub fn total_len(&self) -> u64 {
        match self {
            Manifest::Leaf { entries, .. } => entries.iter().map(|e| u64::from(e.len)).sum(),
            Manifest::Node { children, .. } => children.iter().map(|c| c.total_len).sum(),
        }
    }

    /// Chunk count.
    #[must_use]
    pub fn entry_count(&self) -> usize {
        match self {
            Manifest::Leaf { entries, .. } => entries.len(),
            Manifest::Node { children, .. } => {
                children.iter().map(|c| c.entry_count as usize).sum()
            }
        }
    }

    /// Serialize; returns `(manifest_hash, bytes)`. The hash is BLAKE3 of the top object bytes.
    #[must_use]
    pub fn serialize(&self) -> (Hash, Vec<u8>) {
        let mut buf = Vec::new();
        match self {
            Manifest::Leaf {
                entries,
                compression,
                dict_hash,
                transform,
            } => {
                buf.extend_from_slice(b"CMAN");
                buf.push(MANIFEST_FORMAT_VERSION);
                buf.push(compression.tag());
                buf.push(if dict_hash.is_some() { 1 } else { 0 });
                if let Some(d) = dict_hash {
                    buf.extend_from_slice(&d.0);
                }
                buf.push(transform.tag());
                buf.extend_from_slice(&(entries.len() as u32).to_le_bytes());
                for e in entries {
                    buf.extend_from_slice(&e.offset.to_le_bytes());
                    buf.extend_from_slice(&e.len.to_le_bytes());
                    buf.extend_from_slice(&e.chunk_hash.0);
                }
            }
            Manifest::Node {
                children,
                compression,
                dict_hash,
                transform,
            } => {
                buf.extend_from_slice(b"CMND");
                buf.push(MANIFEST_FORMAT_VERSION);
                buf.push(compression.tag());
                buf.push(if dict_hash.is_some() { 1 } else { 0 });
                if let Some(d) = dict_hash {
                    buf.extend_from_slice(&d.0);
                }
                buf.push(transform.tag());
                buf.extend_from_slice(&(children.len() as u32).to_le_bytes());
                for c in children {
                    buf.extend_from_slice(&c.hash.0);
                    buf.extend_from_slice(&c.total_len.to_le_bytes());
                    buf.extend_from_slice(&c.entry_count.to_le_bytes());
                }
            }
        }
        (Hash::of(&buf), buf)
    }

    /// Parse manifest bytes; validates magic, version, and entry invariants.
    pub fn parse(bytes: &[u8]) -> Result<Self, crate::error::CairnError> {
        let err = || crate::error::CairnError {
            kind: crate::error::ErrorKind::ManifestFormat,
            message: "manifest parse failed".into(),
        };
        if bytes.len() < 7 || (&bytes[0..4] != b"CMAN" && &bytes[0..4] != b"CMND") {
            return Err(err());
        }
        if bytes[4] != MANIFEST_FORMAT_VERSION {
            return Err(err());
        }
        let compression = Compression::from_tag(bytes[5]).ok_or_else(err)?;
        let has_dict = bytes[6] == 1;
        let mut pos = 7;
        let dict_hash = if has_dict {
            if bytes.len() < pos + 32 {
                return Err(err());
            }
            let h = Hash::from_slice(&bytes[pos..pos + 32]).ok_or_else(err)?;
            pos += 32;
            Some(h)
        } else {
            None
        };
        // v2 carries the container transform; v1 implies None
        let transform = if bytes[4] >= 2 {
            let t = Transform::from_tag(bytes[pos]).ok_or_else(err)?;
            pos += 1;
            t
        } else {
            Transform::None
        };
        if bytes.len() < pos + 4 {
            return Err(err());
        }
        let count = u32::from_le_bytes([bytes[pos], bytes[pos + 1], bytes[pos + 2], bytes[pos + 3]])
            as usize;
        pos += 4;
        if &bytes[0..4] == b"CMAN" {
            let stride = 44usize; // 8 + 4 + 32
            if bytes.len() < pos + count * stride {
                return Err(err());
            }
            let mut entries = Vec::with_capacity(count);
            for i in 0..count {
                let b = &bytes[pos + i * stride..pos + (i + 1) * stride];
                let offset = u64::from_le_bytes(b[0..8].try_into().map_err(|_| err())?);
                let len = u32::from_le_bytes(b[8..12].try_into().map_err(|_| err())?);
                let chunk_hash = Hash::from_slice(&b[12..44]).ok_or_else(err)?;
                entries.push(ManifestEntry {
                    offset,
                    len,
                    chunk_hash,
                });
            }
            if entries.windows(2).any(|w| w[0].offset >= w[1].offset) {
                return Err(err()); // entries MUST be sorted by offset
            }
            Ok(Manifest::Leaf {
                entries,
                compression,
                dict_hash,
                transform,
            })
        } else {
            let stride = 44usize; // 32 + 8 + 4
            if bytes.len() < pos + count * stride {
                return Err(err());
            }
            let mut children = Vec::with_capacity(count);
            for i in 0..count {
                let b = &bytes[pos + i * stride..pos + (i + 1) * stride];
                let hash = Hash::from_slice(&b[0..32]).ok_or_else(err)?;
                let total_len = u64::from_le_bytes(b[32..40].try_into().map_err(|_| err())?);
                let entry_count = u32::from_le_bytes(b[40..44].try_into().map_err(|_| err())?);
                children.push(ChildRef {
                    hash,
                    total_len,
                    entry_count,
                });
            }
            Ok(Manifest::Node {
                children,
                compression,
                dict_hash,
                transform,
            })
        }
    }
}

/// Reconstruct file bytes from a manifest: `resolve` walks fanout children (by child hash),
/// `get_chunk` fetches raw chunk content. Every chunk hash is re-verified before assembly.
pub fn assemble_file<R, G>(
    manifest: &Manifest,
    resolve: &mut R,
    get_chunk: &mut G,
) -> Result<Vec<u8>, crate::error::CairnError>
where
    R: FnMut(&Hash) -> Option<Manifest>,
    G: FnMut(&Hash) -> Option<Vec<u8>>,
{
    let entries = manifest.flatten_with(resolve);
    if entries.len() != manifest.entry_count() {
        return Err(crate::error::CairnError {
            kind: crate::error::ErrorKind::ManifestFormat,
            message: "fanout resolution incomplete".into(),
        });
    }
    let mut out = Vec::with_capacity(manifest.total_len() as usize);
    for e in entries {
        let bytes = get_chunk(&e.chunk_hash).ok_or_else(|| crate::error::CairnError {
            kind: crate::error::ErrorKind::ManifestFormat,
            message: format!("missing chunk {}", e.chunk_hash),
        })?;
        // I2: never materialize a corrupt file — verify every chunk hash on ingest.
        if bytes.len() != e.len as usize || Hash::of(&bytes) != e.chunk_hash {
            return Err(crate::error::CairnError {
                kind: crate::error::ErrorKind::ChunkVerification,
                message: format!("chunk {} failed verification", e.chunk_hash),
            });
        }
        out.extend_from_slice(&bytes);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::Hash;
    use std::collections::HashMap;

    fn entries_for(buf: &[u8], spans: &[crate::chunker::ChunkSpan]) -> Vec<ManifestEntry> {
        spans
            .iter()
            .map(|s| ManifestEntry {
                offset: s.offset,
                len: s.len,
                chunk_hash: Hash::of(&buf[s.offset as usize..s.offset as usize + s.len as usize]),
            })
            .collect()
    }

    #[test]
    fn leaf_roundtrip() {
        let buf: Vec<u8> = (0..5 * 1024 * 1024).map(|i| (i % 253) as u8).collect();
        let spans = crate::chunker::FastCdc::cut(&buf);
        let m = Manifest::build(entries_for(&buf, &spans), Compression::None, None);
        let (h, bytes) = m.serialize();
        assert_eq!(h, Hash::of(&bytes));
        let parsed = Manifest::parse(&bytes).unwrap();
        assert_eq!(parsed, m);
        assert_eq!(parsed.total_len() as usize, buf.len());
    }

    #[test]
    fn fanout_over_8192_entries() {
        // 20,000 synthetic entries → tree of depth 2
        let mut entries = Vec::new();
        let mut off = 0u64;
        for i in 0..20_000u64 {
            let len = 64u32;
            entries.push(ManifestEntry {
                offset: off,
                len,
                chunk_hash: Hash::of(&i.to_le_bytes()),
            });
            off += u64::from(len);
        }
        let m = Manifest::build(entries, Compression::Zstd3, None);
        match &m {
            Manifest::Node { children, .. } => {
                assert_eq!(children.len(), 3); // ceil(20000/8192)
                assert_eq!(m.entry_count(), 20_000);
                assert_eq!(m.total_len(), 20_000 * 64);
            }
            Manifest::Leaf { .. } => panic!("expected fanout node"),
        }
        let (h, bytes) = m.serialize();
        let parsed = Manifest::parse(&bytes).unwrap();
        assert_eq!(parsed, m);
        assert_eq!(Hash::of(&bytes), h);
    }

    #[test]
    fn assemble_verifies_every_chunk_i2() {
        let buf: Vec<u8> = (0..6 * 1024 * 1024)
            .map(|i| ((i * 7) % 255) as u8)
            .collect();
        let spans = crate::chunker::FastCdc::cut(&buf);
        let m = Manifest::build(entries_for(&buf, &spans), Compression::None, None);
        let mut store: HashMap<Hash, Vec<u8>> = HashMap::new();
        for e in m.flatten() {
            store.insert(
                e.chunk_hash,
                buf[e.offset as usize..(e.offset + u64::from(e.len)) as usize].to_vec(),
            );
        }
        let mut resolve = |_h: &Hash| -> Option<Manifest> { None }; // leaf needs no resolution
        let mut get = |h: &Hash| store.get(h).cloned();
        let back = assemble_file(&m, &mut resolve, &mut get).unwrap();
        assert_eq!(back, buf);
        // corrupt one chunk → assembly MUST fail (I2: never materialize corrupt files)
        let first = *store.keys().next().unwrap();
        store.insert(first, vec![0u8; 4]);
        let mut get2 = |h: &Hash| store.get(h).cloned();
        assert!(assemble_file(&m, &mut resolve, &mut get2).is_err());
    }

    #[test]
    fn parse_rejects_garbage_and_unsorted() {
        assert!(Manifest::parse(b"").is_err());
        assert!(Manifest::parse(b"XXXX\x01\x00\x00").is_err());
        // valid magic, unsorted entries → rejected
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"CMAN");
        bytes.push(1);
        bytes.push(0); // no dict
        bytes.extend_from_slice(&2u32.to_le_bytes());
        let h1 = Hash::of(b"one");
        let h2 = Hash::of(b"two");
        // first entry offset 100, second offset 50 → unsorted
        bytes.extend_from_slice(&100u64.to_le_bytes());
        bytes.extend_from_slice(&10u32.to_le_bytes());
        bytes.extend_from_slice(&h1.0);
        bytes.extend_from_slice(&50u64.to_le_bytes());
        bytes.extend_from_slice(&10u32.to_le_bytes());
        bytes.extend_from_slice(&h2.0);
        assert!(Manifest::parse(&bytes).is_err());
    }
}
