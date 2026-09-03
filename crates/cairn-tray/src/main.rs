//! cairn-tray — the Windows system tray (ADR-0016 "Clicky-Clicky").
//!
//! Windows-only: the implementation lives in [`tray`] (gated); other targets
//! get a stub so `cargo check/test --workspace` stays green everywhere. The
//! real binary builds on the windows-latest CI leg and ships in the release.

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
