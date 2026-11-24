//! cairn-app — the native window (ADR-0022): a Tauri 2 shell around the
//! daemon's loopback console, `http://127.0.0.1:17778`, in the OS webview
//! (WebView2 on Windows, WKWebView on macOS, WebKitGTK on Linux).
//!
//! The window is still a viewport, not an app rewrite: the console is the
//! daemon's own no-build HTML surface, and there is no IPC surface for a
//! page to reach. What this shell adds is the desktop lifecycle the review
//! asked for (#58/#60):
//!
//! * **starts without a daemon**: the window opens on a bundled start
//!   screen (`dist/index.html`) and, if nothing answers on the console
//!   port, launches `cairn daemon` hidden (the same way the tray's
//!   supervisor does, ADR-0029), then hands the window to the console the
//!   moment it answers. No browser error page, ever.
//! * **one window**: a second launch (Start menu, tray "Open Console",
//!   double-click) focuses the existing window instead of opening another.
//! * **remembers itself**: size, position and maximized state persist.
//! * **stays on the console**: the webview may only navigate to the bundled
//!   start screen or the loopback console; any other link (help pages,
//!   review links the user clicks) opens in the system browser.
//! * **closing ≠ quitting**: closing the window never stops syncing. Quit
//!   lives in the tray ("Quit Cairn") and the console's Settings, both of
//!   which say plainly that syncing stops.

#![deny(unsafe_code)]
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::time::Duration;

use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};

const CONSOLE_ADDR: &str = "127.0.0.1:17778";

fn console_up() -> bool {
    let addr: SocketAddr = CONSOLE_ADDR.parse().expect("static loopback addr");
    TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok()
}

/// Where `cairn` (the engine) lives: beside this exe (installer layout),
/// the per-user install dir, then PATH.
fn engine_candidates() -> Vec<PathBuf> {
    let exe = if cfg!(windows) { "cairn.exe" } else { "cairn" };
    let mut v = Vec::new();
    if let Some(dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(PathBuf::from)) {
        v.push(dir.join(exe));
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        let local = PathBuf::from(local);
        v.push(local.join("Programs").join("Cairn").join(exe));
        v.push(local.join("Programs").join("cairn").join(exe));
    }
    if let Some(path) = std::env::var_os("PATH") {
        v.extend(std::env::split_paths(&path).map(|d| d.join(exe)));
    }
    v
}

/// Launch the daemon detached and hidden. The daemon dedups itself on the
/// console port, so racing the tray's supervisor is harmless.
fn spawn_daemon() -> bool {
    let Some(engine) = engine_candidates().into_iter().find(|p| p.is_file()) else {
        return false;
    };
    let mut cmd = std::process::Command::new(engine);
    cmd.arg("daemon")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null());
    // keep the daemon's log where the tray keeps it (<home>/daemon.log) so
    // "Send diagnostics" finds it whichever process started the daemon
    let home = std::env::var_os("CAIRN_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
                .map(|h| PathBuf::from(h).join(".cairn"))
        });
    match home.and_then(|h| {
        std::fs::create_dir_all(&h).ok()?;
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(h.join("daemon.log"))
            .ok()
    }) {
        Some(log) => {
            cmd.stderr(log);
        }
        None => {
            cmd.stderr(std::process::Stdio::null());
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        cmd.creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS);
    }
    cmd.spawn().is_ok()
}

/// The webview may show only the bundled start screen or the loopback
/// console. Everything else is a real web link: hand it to the OS browser.
fn allowed_in_window(url: &tauri::Url) -> bool {
    match url.scheme() {
        // the bundled start screen (custom protocol differs per platform)
        "tauri" | "asset" => true,
        "http" | "https" => {
            let host = url.host_str().unwrap_or_default();
            let bundled = host == "tauri.localhost";
            let console = (host == "127.0.0.1" || host == "localhost") && url.port() == Some(17778);
            bundled || console
        }
        "about" | "data" | "blob" => true,
        _ => false,
    }
}

fn main() {
    tauri::Builder::default()
        // must be registered first: a second launch focuses this window
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            if let Some(w) = app.get_webview_window("console") {
                let _ = w.unminimize();
                let _ = w.show();
                let _ = w.set_focus();
            }
        }))
        .plugin(tauri_plugin_window_state::Builder::default().build())
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            if !console_up() {
                // best effort; the start screen keeps waiting either way and
                // says so plainly if the daemon never answers
                std::thread::spawn(|| {
                    let _ = spawn_daemon();
                });
            }
            let handle = app.handle().clone();
            WebviewWindowBuilder::new(app, "console", WebviewUrl::App("index.html".into()))
                .title("Cairn")
                .inner_size(1440.0, 1000.0)
                .min_inner_size(980.0, 640.0)
                .center()
                .resizable(true)
                .disable_drag_drop_handler()
                .on_navigation(move |url| {
                    if allowed_in_window(url) {
                        return true;
                    }
                    use tauri_plugin_opener::OpenerExt;
                    let _ = handle.opener().open_url(url.as_str(), None::<&str>);
                    false
                })
                .build()?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .unwrap_or_else(|e| {
            // never a silent no-show: say what happened where a user looks
            eprintln!("cairn-app: the window could not start: {e}");
            std::process::exit(1);
        });
}

#[cfg(test)]
mod tests {
    use super::allowed_in_window;

    fn u(s: &str) -> tauri::Url {
        s.parse().unwrap()
    }

    #[test]
    fn the_window_only_ever_shows_the_console_or_its_start_screen() {
        for ok in [
            "http://127.0.0.1:17778/",
            "http://127.0.0.1:17778/#files",
            "http://localhost:17778/api/v1/file/download?t=x",
            "tauri://localhost/index.html",
            "http://tauri.localhost/index.html",
            "about:blank",
        ] {
            assert!(allowed_in_window(&u(ok)), "should stay in window: {ok}");
        }
        for out in [
            "https://github.com/ssmurfgg04-gif/cairn",
            "http://127.0.0.1:17779/r/token",
            "http://evil.com:17778/",
            "file:///etc/passwd",
            "mailto:support@example.com",
        ] {
            assert!(!allowed_in_window(&u(out)), "should leave the window: {out}");
        }
    }

    #[test]
    fn engine_search_prefers_the_install_layout() {
        let c = super::engine_candidates();
        assert!(!c.is_empty());
        let exe = std::env::current_exe().unwrap();
        assert_eq!(c[0].parent(), exe.parent(), "beside the app exe comes first");
    }
}
