//! Cairn sync engine (SPEC §7): explicit state machine, AIMD uploader, conflict copies,
//! cursor replay. Written against the `Plane` trait so the deterministic sim drives real code.

#![forbid(unsafe_code)]

pub mod aimd;
pub mod apply;
pub mod engine;
pub mod hydrate;
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
