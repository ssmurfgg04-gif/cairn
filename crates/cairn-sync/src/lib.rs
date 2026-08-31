//! Cairn sync engine (SPEC §7): explicit state machine, AIMD uploader, conflict copies,
//! cursor replay. Written against the `Plane` trait so the deterministic sim drives real code.

#![forbid(unsafe_code)]
#![warn(clippy::pedantic)]

pub mod aimd;
pub mod apply;
pub mod engine;
pub mod plane;
pub mod retry;

pub use aimd::Gate;
pub use engine::{Engine, PassStats};
pub use plane::{Entry, Plane, Session};
