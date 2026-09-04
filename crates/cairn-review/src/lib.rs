//! The client review portal (ADR-0020): the "send a link to the client"
//! workflow, built on the sync engine instead of a cloud bucket.
//!
//! Frame.io's core product is not file syncing — it is the review and
//! approval loop. Cairn's answer keeps the zero-cloud P2P foundation and
//! adds the loop locally:
//!
//! * **Version stack** — [`model::ReviewVersion`] entries, appended-only.
//!   Clients always land on the newest version; older versions stay
//!   reachable for comparison (the Frame.io stack model).
//! * **Guest links, no account** — [`model::GuestLink`] tokens (122 bits of
//!   OS CSPRNG entropy, uuid-v4) carrying a role (`Commenter` or `Viewer`)
//!   and an expiry. Nothing to sign up for; the token IS the identity.
//! * **Frame-accurate comments** — comments are ordinary
//!   [`cairn_tl::notes::NoteSet`] files (one per version,
//!   `.cairn/review-notes/v{N}.json`), so they sync peer-to-peer like any
//!   other file and merge with the round-14 three-way note merge. Anchors
//!   are exact frames at the version's rate; timecodes render NDF.
//! * **Reviewer presence** — ephemeral heartbeats held by the serving
//!   daemon (in-memory by design: presence is a live signal, not state).
//! * **The player** — [`http`] serves a self-contained web page (no build
//!   toolchain, no CDN) with a scrub timeline, frame stepping, comment
//!   pins, and HTTP-range media serving so browsers can scrub.
//!
//! The session file `.cairn/review.json` and the per-version note files
//! are plain deterministic JSON in the project root — the engine treats
//! them like any other file, which is the whole trick: review state rides
//! the same encrypted P2P transport, with zero cloud bytes.

// Pure Rust: axum + tokio, no FFI anywhere.
#![forbid(unsafe_code)]

pub mod http;
pub mod model;
pub mod store;

pub use model::{GuestLink, GuestRole, ReviewFile, ReviewVersion};
pub use store::{comment_path, session_path, Store};
