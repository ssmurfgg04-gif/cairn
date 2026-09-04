//! Windows Explorer shell extension for cairn (ADR-0019 §5): overlay icons
//! and context-menu actions — the visual feedback layer non-technical users
//! need (synced/conflict state is otherwise invisible outside `cairn status`).
//!
//! Architecture (reference-grade polish, none of its WebView weight):
//! - **State transport**: the daemon writes a tiny JSON file per root —
//!   `<root>/.cairn/overlay.json` (one write per sync pass, best-effort).
//!   The COM layer reads it with a short cache window. No sqlite access
//!   from Explorer's process (multi-process safety), no background services.
//! - **Root identity**: `<root>/.cairn/root.json` (written at attach) names
//!   the project id; its presence marks a cairn-managed root.
//! - **This crate's split**: `core` (state model, icon priority, command
//!   argv construction) is cross-platform and 100% unit-tested; the `com`
//!   module (DLL exports, `IExplorerIconOverlayIdentifier` ×4,
//!   `IContextMenu`) is cfg(windows) and compiled by the windows CI matrix.
//!
//! Icon priority (highest first): Conflict > Fetching > Pinned > Synced —
//! a conflict must never be masked by a stale "synced" badge.

pub mod core;

#[cfg(windows)]
pub mod com;
