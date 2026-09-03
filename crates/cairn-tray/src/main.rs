//! cairn-tray — the Windows system tray (ADR-0016 "Clicky-Clicky").
//!
//! Windows-only: the implementation lives in [`tray`] (gated); other targets
//! get a stub so `cargo check/test --workspace` stays green everywhere. The
//! real binary builds on the windows-latest CI leg and ships in the release.
//!
//! Unsafe policy (WO6-9, the security sweep's rule): this crate root denies
//! unsafe; the windows-only [`tray`] module re-allows it for the documented
//! Win32 FFI (same pattern as cairn-fs-win: deny at root, audited module
//! exception below). Round 13: the sweep caught the root declaration missing.

#![deny(unsafe_code)]

#[cfg(windows)]
mod tray;

#[cfg(windows)]
fn main() {
    tray::run();
}

#[cfg(not(windows))]
fn main() {
    println!("cairn-tray is a Windows-only tray app; build with --target x86_64-pc-windows-msvc");
}
