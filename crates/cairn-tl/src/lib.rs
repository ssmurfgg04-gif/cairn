//! cairn-tl — OTIO/FCPXML timeline three-way merge core (ADR-0015).
//!
//! Pure (`#![forbid(unsafe_code)]`), no I/O: exact-rational time, the OTIO
//! document model, a byte-deterministic canonical serializer (python-otio
//! 0.18.x interop), the identity ladder, typed op extraction, the total
//! C0–C11 classifier (C11 = opt-in semantic policy, ADR-0023), and the
//! three-way merge driver with its report. FCPXML enters via the bridge
//! (fcpxml.rs) and merges on OTIO only. Round 20 adds the no-AI client-note
//! → mechanical-op recipe (note_ops.rs) and timeline branches (branch.rs).

#![forbid(unsafe_code)]

pub mod branch;
pub mod canon;
pub mod classifier;
pub mod fcpxml;
pub mod handoff;
pub mod identity;
pub mod markers;
pub mod merge;
pub mod model;
pub mod note_ops;
pub mod notes;
pub mod ops;
pub mod parse;
pub mod proofs;
pub mod rational;
pub mod sidecar;
pub mod verify;
