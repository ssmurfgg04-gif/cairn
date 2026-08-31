# ADR-0012: Rename conflict-check semantics + TREE/COMMIT object formats v1

Date: 2026-08-31 · Status: Accepted

## Context
1. SPEC §7.1 defines the conflict rule for paths; renames touch TWO paths. The exact check
   endpoints for `Rename{old_path, new_path}` were ambiguous.
2. §5.1 defines TREE/COMMIT logically; the v4 wire/storage formats need frozen byte layouts
   (protocol-versioned per §18).

## Decision
1. **Rename conflict-check endpoints:** a `Rename` op is accepted iff NO entry from a
   DIFFERENT device has seq > base_seq for EITHER `old_path` OR `new_path` (conservative:
   either endpoint diverging blocks the rename). The journal stores `old_path` in the
   `path` column for index parity; the check queries both endpoints explicitly. Renames
   remain metadata-only — never re-chunked.
2. **TREE v1:** `"CTRE" | ver=1 | u32 count | (u16 name_len, name bytes, u8 kind, hash 32)*`
   — kind 0 = manifest_hash, 1 = tree_hash (fanout reserved). NO mtime in the hash input
   (SPEC §5.1).
3. **COMMIT v1:** `"CCMT" | ver=1 | tree 32 | parent 32 (zeros = root) | (u16 len, author) |
   (u16 len, label) | u64 snapshot_seq` — big-endian-free (all LE), fixed positions
   documented in `cairn-server/src/fold.rs`.
4. **PACK v1:** `"CPCK" | ver=1 | u32 count | (u32 len, hash 32, data)*` — ported format
   concepts from git packfile+idx (no git code copied); verified read-back before the
   pack_index transaction switch.

## Consequences
- All object formats carry the version byte; changes are protocol changes (ADR required).
- Fuzz targets cover manifest, pack, and journal-op parsing (§15.5).
