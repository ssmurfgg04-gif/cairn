//! cairn-tl — OTIO/FCPXML timeline three-way merge core (ADR-0015).
//!
//! Pure (`#![forbid(unsafe_code)]`), no I/O: exact-rational time, the OTIO
//! document model, a byte-deterministic canonical serializer (python-otio
//! 0.18.x interop), the identity ladder, typed op extraction, the total
//! C0–C10 classifier, and the three-way merge driver with its report.
//! FCPXML enters via the bridge (fcpxml.rs) and merges on OTIO only.

#![forbid(unsafe_code)]

pub mod canon;
pub mod classifier;
pub mod fcpxml;
pub mod identity;
pub mod merge;
pub mod model;
pub mod ops;
pub mod parse;
pub mod proofs;
pub mod rational;
pub mod sidecar;
