//! cairn-app — the native window (ADR-0022): a Tauri 2 shell that points
//! the OS webview (WebView2 on Windows, WKWebView on macOS) at the
//! daemon's loopback console, `http://127.0.0.1:17778`.
//!
//! Why this shape (the 1-week "pretty" leg):
//! * the console is ALREADY a no-build HTML/CSS/JS surface served by the
//!   daemon — the window is a viewport, not an app rewrite;
//! * WebView2 ships with Windows 10/11 (the `embed` mode pins a fixed
//!   runtime for the stragglers), WKWebView ships with macOS — no
//!   Chromium bundled, no 85 MB installer, ~6 MB of our own code;
//! * the loopback URL keeps ADR-0009's security posture: the dashboard
//!   never listens beyond the machine, so the window cannot leak it.
//!
//! What this deliberately is NOT:
//! * no tray — `cairn-tray` (ADR-0016) owns the tray; its Open action
//!   gains "launch cairn-app" alongside the browser fallback;
//! * no updater, no custom protocol, no IPC surface — nothing for an
//!   attacker to reach that the browser view cannot.
//!
//! Build (Windows): `cargo tauri build` in this directory, or the plain
//! `cargo build --release` for a check-only gate; the NSIS bundle lands
//! in `target/release/bundle/`. HKCU Run registration stays with
//! install.ps1 (cairn-tray autostart), per ADR-0016.

#![deny(unsafe_code)]

// The window's job is to exist; everything else lives in the served page.
fn main() {
    tauri::Builder::default()
        .run(tauri::generate_context!())
        .expect("cairn-app: window failed to start (is another instance holding the console?)");
}
