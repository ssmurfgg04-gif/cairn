# THIRD_PARTY.md — provenance of studied/ported implementations and notable dependencies

Cairn's working agreement: wherever a battle-tested open-source implementation exists, USE it,
PORT it, or STUDY its source before writing our own. This file records which. Copied code (if
any) carries provenance headers; preference order is: crates > port-the-approach > invent.

## Studied / ported references

| Project | License | What we studied / ported | Where |
|---|---|---|---|
| restic | BSD-2-Clause | Chunker discipline (Gear hash table generation, rolling boundary), packing/verifiable-pack layout, crypto patterns | `cairn-core/src/chunker.rs` (approach), `cairn-server/src/jobs/pack.rs` |
| FastCDC (Joran Dirk Greef, 2016) | paper | Content-defined chunking with min/avg/max and boundary masks (2^22 average target) | `cairn-core/src/chunker.rs` |
| kopia | Apache-2.0 | Dedup pipeline shape, parallel upload orchestration, pack format rationale | `cairn-sync/src/aimd.rs`, `cairn-server/src/jobs/pack.rs` |
| syncthing | MPL-2.0 | Sync engine structure, conflict-copy semantics ("name (conflict — device — date).ext"), cursor/reconnect behavior | `cairn-sync/src/engine.rs`, `cairn-sync/src/apply.rs` |
| rclone | MIT | Transfer/backoff/presign patterns, full-jitter retry, AIMD-style concurrency discipline | `cairn-server/src/storage/sigv4.rs`, `cairn-sync/src/retry.rs` |
| git | GPLv2 (format spec ONLY) | Packfile + .idx two-file format concepts (versioned byte, sorted idx, verify-before-switch). No git code copied. | `cairn-server/src/jobs/pack.rs` |
| SQLite | Public domain | WAL discipline, busy_timeout, single-writer patterns, PRAGMA user_version migrations | `cairn-store/src/db.rs`, `cairn-server/src/db.rs` |
| Blender Foundation | GPL/CC (demo assets) | REAL production .blend (`BMW27.blend`, gzip-compressed by Blender itself) used as the real-container normalization evidence — bytes committed under `crates/cairn-core/tests/data/` | `cairn-core/tests/real_blend_roundtrip.rs` |
| nextcloud/desktop | AGPL-3.0 | CfAPI walking-skeleton patterns studied (CfRegisterSyncRoot policies, FETCH_DATA→TRANSFER_DATA with 4096-block alignment, self-hydration deadlock guard, SyncRootManager registry keys) | `cairn-fs-win/src/cfapi.rs` (approach) |
| AWS SigV4 | documentation | Presigned URL canonical request format (we implement the signer ourselves — see ADR-0005) | `cairn-server/src/storage/sigv4.rs` |

## Skill / design references (installed)

| Project | License | Use |
|---|---|---|
| Leonxlnx/taste-skill | MIT | UI design guidance for the local diagnostics dashboard (security-reviewed before install; see ADR-0009) |

## Notable crate dependencies (runtime)

tokio, tonic, prost, axum (async/gRPC/HTTP) · rusqlite (client SQLite, bundled) · sqlx (server
SQLite) · blake3 (hashing) · zstd (compression) · notify (fs watching) · fuser (FUSE, Linux) ·
keyring (OS keychain) · pasetors (PASETO v4 tokens) · uuid v7 (request ids) · thiserror /
anyhow (error taxonomy per ADR-0010) · tracing / tracing-subscriber (observability per
ADR-0007) · proptest (property tests) · memmap2 (large-file mmap) · unicode-normalization
(NFC paths) · flate2 (gzip container normalization) · zip (zip container normalization, deflate-only feature set).

## Deleted-code guarantee
No GPL code is copied into this repository. Git's pack format knowledge is used at the format/
spec level only. restic/syncthing/rclone/kopia are studied as references; direct ports of
non-trivial code blocks (if ever introduced) must add a provenance header and a row above.

## WO2 additions (2026-08-31)
windows / windows-core (Microsoft-maintained Rust bindings for the Win32 CloudFilters API,
MIT) · rcgen (dev-dependency only: self-signed TLS test certificates, MIT/Apache-2.0).
