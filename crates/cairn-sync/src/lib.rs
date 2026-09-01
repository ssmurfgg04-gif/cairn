//! Cairn sync engine (SPEC §7): explicit state machine, AIMD uploader, conflict copies,
//! cursor replay. Written against the `Plane` trait so the deterministic sim drives real code.

#![forbid(unsafe_code)]

pub mod aimd;
pub mod apply;
pub mod engine;
pub mod hydrate;
pub mod native_collab;
pub mod plane;
pub mod plane_grpc;
pub mod retry;
pub mod scan;
pub mod watch;
pub mod workspace;

pub use aimd::Gate;
pub use engine::{Engine, PassStats};
pub use plane::{Entry, Plane, Session};
pub use workspace::{set_workspace, workspace_dir};

/// ADR-0014 Phase 3 — ephemeral lease TTL. Short by design: a crashed editor's pen
/// expires in seconds, not minutes. Correctness never depends on it (fencing does).
pub const LEASE_TTL_MS: u64 = 15_000;
/// ADR-0014 Phase 3 — heartbeat cadence (3 beats per TTL: 2 lost beats still renew).
pub const LEASE_HEARTBEAT_MS: u64 = 5_000;
