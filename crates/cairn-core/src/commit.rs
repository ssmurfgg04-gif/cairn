//! Snapshot object formats (SPEC §7.2, frozen wire shapes — formerly server-only in
//! cairn-server::fold; moved here so the DAEMON can parse commits/trees for the ctl
//! restore path without depending on the server crate).
//!
//! TREE object: "CTRE" | v1 | u32 n | (u16 name_len, name, u8 kind, hash 32)*
//!   kind: 0 = manifest_hash, 1 = tree_hash (fanout reserved)
//! COMMIT object: "CCMT" | v1 | tree 32 | parent 32 | (u16 len, author) | (u16 len, label) | u64 snapshot_seq

use crate::hash::Hash;
use crate::{CairnError, ErrorKind};

pub const TREE_MAGIC: &[u8; 4] = b"CTRE";
pub const COMMIT_MAGIC: &[u8; 4] = b"CCMT";
pub const OBJECT_FORMAT_VERSION: u8 = 1;

/// Build TREE bytes (no mtime in hash input — SPEC §5.1).
#[must_use]
pub fn build_tree(entries: &[(String, String)]) -> (Hash, Vec<u8>) {
    let mut buf = Vec::new();
    buf.extend_from_slice(TREE_MAGIC);
    buf.push(OBJECT_FORMAT_VERSION);
    buf.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for (name, hash_hex) in entries {
        let name_bytes = name.as_bytes();
        buf.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        buf.extend_from_slice(name_bytes);
        buf.push(0u8); // kind = manifest
        let h = Hash::from_hex(hash_hex).unwrap_or_else(|| Hash::of(name.as_bytes()));
        buf.extend_from_slice(&h.0);
    }
    (Hash::of(&buf), buf)
}

/// Build COMMIT bytes.
#[must_use]
pub fn build_commit(
    tree: &Hash,
    parent: Option<&Hash>,
    author: &str,
    label: &str,
    snapshot_seq: u64,
) -> (Hash, Vec<u8>) {
    let mut buf = Vec::new();
    buf.extend_from_slice(COMMIT_MAGIC);
    buf.push(OBJECT_FORMAT_VERSION);
    buf.extend_from_slice(&tree.0);
    buf.extend_from_slice(&parent.unwrap_or(&Hash::from_bytes([0u8; 32])).0);
    let author_b = author.as_bytes();
    buf.extend_from_slice(&(author_b.len() as u16).to_le_bytes());
    buf.extend_from_slice(author_b);
    let label_b = label.as_bytes();
    buf.extend_from_slice(&(label_b.len() as u16).to_le_bytes());
    buf.extend_from_slice(label_b);
    buf.extend_from_slice(&snapshot_seq.to_le_bytes());
    (Hash::of(&buf), buf)
}

/// Parse COMMIT bytes (restore path, ctl listing).
pub fn parse_commit(bytes: &[u8]) -> Result<(Hash, Option<Hash>, String, String, u64), CairnError> {
    let err = || CairnError::new(ErrorKind::ManifestFormat, "commit parse failed");
    if bytes.len() < 8 || &bytes[0..4] != COMMIT_MAGIC || bytes[4] != OBJECT_FORMAT_VERSION {
        return Err(err());
    }
    let tree = Hash::from_slice(&bytes[5..37]).ok_or_else(err)?;
    let parent_bytes = &bytes[37..69];
    let parent = Hash::from_slice(parent_bytes).ok_or_else(err)?;
    let parent = if parent.0 == [0u8; 32] {
        None
    } else {
        Some(parent)
    };
    let mut pos = 69;
    let a_len = u16::from_le_bytes([bytes[pos], bytes[pos + 1]]) as usize;
    pos += 2;
    let author = String::from_utf8_lossy(&bytes[pos..pos + a_len]).into_owned();
    pos += a_len;
    let l_len = u16::from_le_bytes([bytes[pos], bytes[pos + 1]]) as usize;
    pos += 2;
    let label = String::from_utf8_lossy(&bytes[pos..pos + l_len]).into_owned();
    pos += l_len;
    let mut seq_b = [0u8; 8];
    seq_b.copy_from_slice(&bytes[pos..pos + 8]);
    Ok((tree, parent, author, label, u64::from_le_bytes(seq_b)))
}

/// Parse TREE bytes into (path, manifest-hash) entries (ctl restore path).
pub fn parse_tree(bytes: &[u8]) -> Result<Vec<(String, String)>, CairnError> {
    let err = || CairnError::new(ErrorKind::ManifestFormat, "tree parse failed");
    if bytes.len() < 9 || &bytes[0..4] != TREE_MAGIC || bytes[4] != OBJECT_FORMAT_VERSION {
        return Err(err());
    }
    let n = u32::from_le_bytes([bytes[5], bytes[6], bytes[7], bytes[8]]) as usize;
    let mut pos = 9;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        if pos + 2 > bytes.len() {
            return Err(err());
        }
        let name_len = u16::from_le_bytes([bytes[pos], bytes[pos + 1]]) as usize;
        pos += 2;
        if pos + name_len + 1 + 32 > bytes.len() {
            return Err(err());
        }
        let name = String::from_utf8_lossy(&bytes[pos..pos + name_len]).into_owned();
        pos += name_len;
        let kind = bytes[pos];
        pos += 1;
        if kind != 0 {
            return Err(err()); // fanout trees (kind 1) are reserved, never produced
        }
        let Some(h) = Hash::from_slice(&bytes[pos..pos + 32]) else {
            return Err(err());
        };
        pos += 32;
        out.push((name, h.hex()));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tree_commit_roundtrip() {
        let entries = vec![
            ("a.prproj".to_string(), Hash::of(b"a").hex()),
            ("media/b.mov".to_string(), Hash::of(b"b").hex()),
        ];
        let (tree_hash, tree_bytes) = build_tree(&entries);
        assert_eq!(parse_tree(&tree_bytes).unwrap(), entries);
        let (commit_hash, commit_bytes) =
            build_commit(&tree_hash, None, "dev-1", "before-conform", 7);
        let (tree2, parent, author, label, seq) = parse_commit(&commit_bytes).unwrap();
        assert_eq!(tree2, tree_hash);
        assert!(parent.is_none());
        assert_eq!(author, "dev-1");
        assert_eq!(label, "before-conform");
        assert_eq!(seq, 7);
        assert_eq!(commit_hash, Hash::of(&commit_bytes));
        // parent chain round-trips
        let (_h2, c2) = build_commit(&tree_hash, Some(&tree_hash), "d", "l", 8);
        let (_, parent2, _, _, seq2) = parse_commit(&c2).unwrap();
        assert_eq!(parent2, Some(tree_hash));
        assert_eq!(seq2, 8);
    }
}
