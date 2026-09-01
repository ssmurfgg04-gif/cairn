//! Cairn local store (SPEC §5.3): client SQLite in WAL mode with `PRAGMA user_version`
//! migrations, single-writer discipline, content-addressed local chunk store, durable outbox
//! for pending journal appends, and the header cache behind the I1 <50ms hydration gate.
//!
//! Crash discipline (I2): every acknowledged write is committed through SQLite WAL before
//! being reported durable; a crash at ANY point replays to a consistent state — verified by
//! the `kill -9` fault harness (`cairn-x`).

// deny-by-default; exceptions (each reviewed inline with SAFETY proofs):
//  - eviction.rs: free-space probes (statvfs / GetDiskFreeSpaceExW have no safe std equivalent)
//  - db.rs: process-alive probe for ADR-0014 Phase 3 lease reaping (kill(pid,0) / OpenProcess)
#![deny(unsafe_code)]

pub mod cas;
pub mod db;
#[allow(unsafe_code)] // free-space probes only (statvfs / GetDiskFreeSpaceExW)
pub mod eviction;
pub mod headers;
pub mod outbox;
pub mod state;

pub use cas::Cas;
pub use db::FileRow;
pub use db::Store;
pub use headers::HeaderCache;
pub use outbox::{Outbox, OutboxEntry};
