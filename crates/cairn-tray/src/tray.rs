//! cairn-tray — the Windows system tray (ADR-0016 "Clicky-Clicky", part B).
//!
//! Design contract (the ADR's hard boundary): the tray is a THIN ONBOARDING
//! LAYER. It never links the engine, never opens the store, never talks
//! gRPC. Every capability is a wrapped `cairn.exe` invocation
//! (CREATE_NO_WINDOW — no console ever flashes), and every read is
//! `cairn status --json` / `cairn doctor` / `cairn init --json` output.
//! The engine stays headless; a tray crash can never take sync down.
//!
//! What it gives the video editor without a terminal:
//! - tray icon + tooltip with live sync state (poll `status --json`)
//! - Connect to Project… → folder picker → `cairn attach <path>`
//!   (enrollment + attachment + mount happen in the daemon, in the
//!   background — one-click attach; the login code flow is out of tray v1:
//!   the installer's next-step text covers it)
//! - Status… → `cairn doctor` output in a message box
//! - Open Project Folder → Explorer at the project root
//! - Disconnect → `cairn detach --project <id>`
//!
//! Build: `cargo build -p cairn-tray` on a Windows host (or the CI
//! windows-latest leg). The icon is embedded (include_bytes!) and loaded via
//! CreateIconFromResourceEx — no external files, no temp writes.

//! The tray implementation (gated `cfg(windows)` at the mod declaration).
//!
//! The window proc and message loop are `unsafe` Win32 FFI — this module
//! opts back into unsafe explicitly (workspace `deny(unsafe_code)` policy,
//! same as cairn-fs-win: every unsafe block touches the documented Win32
//! ABI, invariants annotated per block).
#![allow(unsafe_code)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    SHBrowseForFolderW, SHGetPathFromIDListW, ShellExecuteW, Shell_NotifyIconW, BROWSEINFOW,
    NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_ERROR, NIIF_INFO, NIM_ADD, NIM_DELETE,
    NIM_MODIFY, NOTIFYICONDATAW, NOTIFY_ICON_INFOTIP_FLAGS,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconFromResourceEx, CreatePopupMenu, CreateWindowExW, DefWindowProcW,
    DestroyWindow, DispatchMessageW, GetCursorPos, GetMessageW, LoadImageW, MessageBoxW,
    PostMessageW, PostQuitMessage, RegisterClassW, SetForegroundWindow, SetTimer, TrackPopupMenu,
    TranslateMessage, HICON, IMAGE_FLAGS, LR_DEFAULTCOLOR, MB_ICONERROR, MB_ICONINFORMATION,
    MB_ICONWARNING, MB_OK, MF_DISABLED, MF_GRAYED, MF_SEPARATOR, MF_STRING, MSG, TPM_BOTTOMALIGN,
    TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_APP, WM_COMMAND, WM_DESTROY, WM_LBUTTONUP, WM_RBUTTONUP,
    WM_TIMER, WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_OVERLAPPED,
};

/// Tray callback message (WM_APP-range is reserved for exactly this).
const WM_TRAY: u32 = WM_APP + 1;
/// Background status poll result posted from the worker thread.
const WM_STATUS: u32 = WM_APP + 2;
/// Poll cadence for `cairn status --json`.
const POLL_MS: u32 = 3000;
/// CREATE_NO_WINDOW — children never flash a console.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Menu command ids (returned by TrackPopupMenu with TPM_RETURNCMD).
mod menu {
    pub const CONNECT: i32 = 2001;
    pub const STATUS: i32 = 2002;
    pub const OPEN: i32 = 2003;
    pub const SETTINGS: i32 = 2004;
    pub const DISCONNECT: i32 = 2005;
    pub const EXIT: i32 = 2099;
}

/// The parsed live status (all fields best-effort: missing daemon, missing
/// enrollment — every state has a rendering).
#[derive(Clone, Debug, Default)]
struct LiveStatus {
    daemon_up: bool,
    enrolled: Option<bool>,
    server_reachable: Option<bool>,
    project_id: Option<String>,
    project_root: Option<String>,
    project_state: Option<String>,
    files_synced: Option<u64>,
    pending_outbox: Option<u64>,
    last_error: Option<String>,
}

impl LiveStatus {
    /// The tooltip is the product's "I'm alive" light (ADR-0021 §4): it must
    /// read like plain English, not a log line. Errors are truncated so the
    /// 128-char NOTIFYICONDATAW tip budget is never the thing that mangles it.
    fn summary(&self) -> String {
        if !self.daemon_up {
            return "Cairn — daemon not running".into();
        }
        let clip = |s: &str| -> String {
            if s.chars().count() > 72 {
                s.chars().take(72).collect::<String>() + "…"
            } else {
                s.to_string()
            }
        };
        match (self.project_state.as_deref(), self.last_error.as_deref()) {
            (Some("error"), Some(e)) => format!("Cairn — attention: {}", clip(e)),
            (Some("error"), None) => "Cairn — attention: project in error".into(),
            (Some("syncing"), _) => format!(
                "Cairn — syncing{}",
                self.pending_outbox
                    .map(|p| format!(" — {p} chunk{} in flight", if p == 1 { "" } else { "s" }))
                    .unwrap_or_default()
            ),
            (Some("synced"), _) => format!(
                "Cairn — all files synced ({} files){}",
                self.files_synced.unwrap_or(0),
                self.server_reachable
                    .filter(|r| !*r)
                    .map(|_| " · offline from server")
                    .unwrap_or_default()
            ),
            (Some(s), _) => format!("Cairn — {s}"),
            (None, _) => {
                if self.server_reachable == Some(false) {
                    "Cairn — connected (offline from server)".into()
                } else {
                    "Cairn — connected (no project)".into()
                }
            }
        }
    }
}

/// Shared tray state (status worker thread → message loop).
struct Shared {
    status: Mutex<LiveStatus>,
    /// last status the balloon pass SAW — transitions, not polls, drive
    /// notifications (a 3 s cadence of "syncing… syncing…" is noise)
    prev_status: Mutex<Option<LiveStatus>>,
    /// set after `connect…` runs so the next poll re-reads eagerly
    poll_now: AtomicBool,
}

/// Balloon (tray toast) emission rules — the "push" the tray never had:
/// * daemon LOST or a NEW error: red, always
/// * sync COMPLETED (in-flight chunks drained to zero with files known):
///   one quiet info balloon — the "I'm alive and done" moment
/// * anything else (still syncing, still up, error unchanged): silence
/// Returns (title, body, NIIF level).
fn notify_transition(
    prev: Option<&LiveStatus>,
    now: &LiveStatus,
) -> Option<(&'static str, String, NOTIFY_ICON_INFOTIP_FLAGS)> {
    let prev = prev?;
    if prev.daemon_up && !now.daemon_up {
        return Some((
            "Cairn",
            "daemon unreachable — restart it from your terminal".into(),
            NIIF_ERROR,
        ));
    }
    if !prev.daemon_up && !now.daemon_up {
        return None; // still down: the tooltip carries it
    }
    let new_error = now
        .last_error
        .as_deref()
        .or_else(|| {
            if now.project_state.as_deref() == Some("error") {
                Some("project in error")
            } else {
                None
            }
        })
        .filter(|e| Some(*e) != prev.last_error.as_deref());
    if let Some(e) = new_error {
        let body: String = e.chars().take(200).collect();
        return Some(("Cairn — attention", body, NIIF_ERROR));
    }
    let drained = now.daemon_up
        && now.pending_outbox == Some(0)
        && now.project_state.as_deref() == Some("synced")
        && now.files_synced.unwrap_or(0) > 0
        && (prev.pending_outbox.unwrap_or(0) > 0
            || prev.project_state.as_deref() != Some("synced"));
    if drained {
        return Some((
            "Cairn",
            format!("all files synced ({} files)", now.files_synced.unwrap_or(0)),
            NIIF_INFO,
        ));
    }
    None
}

pub fn run() {
    let shared = Arc::new(Shared {
        status: Mutex::new(LiveStatus::default()),
        prev_status: Mutex::new(None),
        poll_now: AtomicBool::new(true),
    });

    unsafe {
        let hinstance = GetModuleHandleW(None).expect("GetModuleHandleW");
        let class_name = w!("CairnTrayWnd");

        let wc = WNDCLASSW {
            lpfnWndProc: Some(wnd_proc),
            hInstance: HINSTANCE(hinstance.0),
            lpszClassName: class_name,
            ..Default::default()
        };
        let atom = RegisterClassW(&wc);
        if atom == 0 {
            eprintln!("cairn-tray: RegisterClassW failed");
            std::process::exit(1);
        }

        // A message-only-ish tool window: never shown, owns the tray icon.
        let hwnd = CreateWindowExW(
            WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            class_name,
            w!("Cairn Tray"),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None,
            None,
            HINSTANCE(hinstance.0),
            None,
        )
        .expect("CreateWindowExW");

        // the Shared pointer rides the window's user data
        windows::Win32::UI::WindowsAndMessaging::SetWindowLongPtrW(
            hwnd,
            windows::Win32::UI::WindowsAndMessaging::GWLP_USERDATA,
            Box::into_raw(Box::new(Arc::clone(&shared))) as isize,
        );

        let hicon = load_cairn_icon();
        let mut nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: 1,
            uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
            uCallbackMessage: WM_TRAY,
            hIcon: hicon,
            ..Default::default()
        };
        set_tip(&mut nid, "Cairn — starting…");
        if !Shell_NotifyIconW(NIM_ADD, &nid).as_bool() {
            eprintln!(
                "cairn-tray: Shell_NotifyIcon(NIM_ADD) failed — explorer not ready? retrying once"
            );
            std::thread::sleep(std::time::Duration::from_secs(1));
            if !Shell_NotifyIconW(NIM_ADD, &nid).as_bool() {
                eprintln!("cairn-tray: tray unavailable; exiting");
                std::process::exit(1);
            }
        }

        // 3s status poll (worker thread does the subprocess; the loop stays responsive)
        SetTimer(hwnd, 1, POLL_MS, None);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
    }
}

/// Find cairn.exe: same directory as this exe (installer layout), PATH as
/// fallback (dev runs).
fn cairn_binary() -> String {
    let here = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(std::path::Path::to_path_buf));
    if let Some(dir) = here {
        let candidate = dir.join("cairn.exe");
        if candidate.is_file() {
            return candidate.to_string_lossy().into_owned();
        }
    }
    "cairn".into()
}

/// Run `cairn … args` hidden; return (exit ok, combined output trimmed).
fn run_cairn(args: &[&str]) -> (bool, String) {
    use std::process::Command;
    let mut cmd = Command::new(cairn_binary());
    cmd.args(args);
    let out = cmd.creation_flags(CREATE_NO_WINDOW).output();
    match out {
        Ok(o) => {
            let mut text = String::from_utf8_lossy(&o.stdout).into_owned();
            if o.stdout.is_empty() && !o.stderr.is_empty() {
                text = String::from_utf8_lossy(&o.stderr).into_owned();
            }
            (o.status.success(), text.trim().to_string())
        }
        Err(e) => (false, format!("cannot run cairn: {e}")),
    }
}

use std::os::windows::process::CommandExt;

/// The status worker: parse `cairn status --json` (daemon shape) and fall
/// back to `cairn init --json` (enrolled?) when the daemon is down.
fn poll_status() -> LiveStatus {
    let mut st = LiveStatus::default();
    let (ok, text) = run_cairn(&["status", "--json"]);
    if ok {
        st.daemon_up = true;
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
            st.server_reachable = v.get("server_reachable").and_then(|b| b.as_bool());
            if let Some(projects) = v.get("projects").and_then(|p| p.as_array()) {
                if let Some(p) = projects.first() {
                    st.project_id = p
                        .get("project_id")
                        .and_then(|x| x.as_str())
                        .map(str::to_string);
                    st.project_root = p
                        .get("root_path")
                        .and_then(|x| x.as_str())
                        .map(str::to_string);
                    st.project_state = p.get("state").and_then(|x| x.as_str()).map(str::to_string);
                    st.files_synced = p.get("files_synced").and_then(|x| x.as_u64());
                    st.pending_outbox = p.get("pending_outbox").and_then(|x| x.as_u64());
                    st.last_error = p
                        .get("last_error")
                        .and_then(|x| x.as_str())
                        .map(str::to_string);
                }
            }
        }
    } else {
        // daemon down: enrollment still worth reporting
        let (init_ok, init_text) = run_cairn(&["init", "--json"]);
        if init_ok {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&init_text) {
                st.enrolled = v.get("enrolled").and_then(|b| b.as_bool());
            }
        }
    }
    st
}

fn set_tip(nid: &mut NOTIFYICONDATAW, tip: &str) {
    let chars: Vec<u16> = tip.encode_utf16().take(127).collect();
    let mut buf = [0u16; 128];
    buf[..chars.len()].copy_from_slice(&chars);
    nid.szTip = buf;
}

/// Wide-copy with a budget (balloon text 255, title 63 — never truncate
/// mid-surrogate).
fn set_wide(dst: &mut [u16], s: &str) {
    let budget = dst.len().saturating_sub(1);
    let mut n = 0usize;
    for (i, unit) in s.encode_utf16().enumerate() {
        if i >= budget {
            break;
        }
        dst[i] = unit;
        n = i + 1;
    }
    dst[n] = 0;
}

/// Fire the balloon toast (Windows renders it as a toast for tray apps
/// without a toast identity — the zero-dependency path).
unsafe fn show_balloon(hwnd: HWND, title: &str, body: &str, level: NOTIFY_ICON_INFOTIP_FLAGS) {
    let mut nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uFlags: NIF_INFO,
        dwInfoFlags: level,
        ..Default::default()
    };
    set_wide(&mut nid.szInfo, body);
    set_wide(&mut nid.szInfoTitle, title);
    let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
}

unsafe fn load_cairn_icon() -> HICON {
    // Embedded .ico bytes → icon. CreateIconFromResourceEx expects the icon
    // RESOURCE bits: the bytes AFTER the ICONDIR+ICONDIRENTRY header (the
    // BITMAPINFOHEADER-led image), version 0x0003_0000 per the docs.
    const ICO: &[u8] = include_bytes!("cairn.ico");
    let image = &ICO[22..]; // skip ICONDIR (6) + ICONDIRENTRY (16)
    CreateIconFromResourceEx(image, true, 0x0003_0000, 0, 0, LR_DEFAULTCOLOR).unwrap_or_else(|_| {
        // Stock fallback: never run without an icon at all
        LoadImageW(
            None,
            PCWSTR(32512usize as *const u16), // IDI_APPLICATION by ordinal
            windows::Win32::UI::WindowsAndMessaging::IMAGE_ICON,
            0,
            0,
            IMAGE_FLAGS(0),
        )
        .map(|h| HICON(h.0))
        .expect("stock icon")
    })
}

unsafe fn shared_of(hwnd: HWND) -> Option<Arc<Shared>> {
    let ptr = windows::Win32::UI::WindowsAndMessaging::GetWindowLongPtrW(
        hwnd,
        windows::Win32::UI::WindowsAndMessaging::GWLP_USERDATA,
    ) as *const Arc<Shared>;
    if ptr.is_null() {
        None
    } else {
        Some(Arc::clone(&*ptr))
    }
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_TRAY => {
            let button = (lparam.0 as u32) & 0xFFFF;
            if button == WM_RBUTTONUP || button == WM_LBUTTONUP {
                show_menu(hwnd);
            }
            LRESULT(0)
        }
        WM_TIMER => {
            let _ = wparam;
            if let Some(shared) = shared_of(hwnd) {
                shared.poll_now.swap(false, Ordering::Relaxed);
                std::thread::spawn(move || {
                    let st = poll_status();
                    if let Ok(mut guard) = shared.status.lock() {
                        *guard = st;
                    }
                    // wake the loop to re-render the tooltip (best-effort post)
                    unsafe {
                        let _ = PostMessageW(None, WM_STATUS, WPARAM(0), LPARAM(0));
                    }
                });
            }
            LRESULT(0)
        }
        WM_STATUS => {
            if let Some(shared) = shared_of(hwnd) {
                if let Ok(st) = shared.status.lock() {
                    // transition detection FIRST (it needs the previous state
                    // before we overwrite it), then the tooltip update
                    let toast = shared.prev_status.lock().ok().and_then(|mut prev| {
                        let now = st.clone();
                        let t = notify_transition(prev.as_ref(), &now);
                        *prev = Some(now);
                        t
                    });
                    let mut nid = NOTIFYICONDATAW {
                        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
                        hWnd: hwnd,
                        uID: 1,
                        uFlags: NIF_TIP,
                        ..Default::default()
                    };
                    set_tip(&mut nid, &st.summary());
                    let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
                    if let Some((title, body, level)) = toast {
                        show_balloon(hwnd, title, &body, level);
                    }
                }
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        WM_COMMAND => {
            // menu commands from TrackPopupMenu arrive as the return value
            // (TPM_RETURNCMD), not WM_COMMAND — kept for completeness
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

unsafe fn show_menu(hwnd: HWND) {
    let Some(shared) = shared_of(hwnd) else {
        return;
    };
    let status = shared.status.lock().map(|s| s.clone()).unwrap_or_default();

    let menu = CreatePopupMenu().expect("CreatePopupMenu");
    let summary = status.summary();
    // Status line (disabled) — the tooltip, visible
    let summary_w = to_wide(&summary);
    let _ = AppendMenuW(
        menu,
        MF_STRING | MF_DISABLED | MF_GRAYED,
        0,
        PCWSTR(summary_w.as_ptr()),
    );
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
    let connect_label = if status.project_id.is_some() {
        "Connect to Another Project…"
    } else {
        "Connect to Project…"
    };
    let connect_w = to_wide(connect_label);
    let _ = AppendMenuW(
        menu,
        MF_STRING,
        menu::CONNECT as usize,
        PCWSTR(connect_w.as_ptr()),
    );
    let status_w = to_wide("Status Details");
    let _ = AppendMenuW(
        menu,
        MF_STRING,
        menu::STATUS as usize,
        PCWSTR(status_w.as_ptr()),
    );
    let disabled_open = status.project_root.is_none();
    let mut flags = MF_STRING;
    if disabled_open {
        flags |= MF_DISABLED | MF_GRAYED;
    }
    let open_w = to_wide("Open Project Folder");
    let _ = AppendMenuW(menu, flags, menu::OPEN as usize, PCWSTR(open_w.as_ptr()));
    let settings_w = to_wide("Settings");
    let _ = AppendMenuW(
        menu,
        MF_STRING,
        menu::SETTINGS as usize,
        PCWSTR(settings_w.as_ptr()),
    );
    let mut flags = MF_STRING;
    if status.project_id.is_none() {
        flags |= MF_DISABLED | MF_GRAYED;
    }
    let disconnect_w = to_wide("Disconnect");
    let _ = AppendMenuW(
        menu,
        flags,
        menu::DISCONNECT as usize,
        PCWSTR(disconnect_w.as_ptr()),
    );
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
    let exit_w = to_wide("Exit");
    let _ = AppendMenuW(
        menu,
        MF_STRING,
        menu::EXIT as usize,
        PCWSTR(exit_w.as_ptr()),
    );

    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    let _ = SetForegroundWindow(hwnd);

    let cmd = TrackPopupMenu(
        menu,
        TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN,
        pt.x,
        pt.y,
        0,
        hwnd,
        None,
    );
    let _ = windows::Win32::UI::WindowsAndMessaging::DestroyMenu(menu);

    match cmd.0 as i32 {
        menu::CONNECT => do_connect(hwnd, &shared),
        menu::STATUS => do_status(hwnd),
        menu::OPEN => do_open(&status),
        menu::SETTINGS => do_settings(hwnd, &status),
        menu::DISCONNECT => do_disconnect(hwnd, &shared, &status),
        menu::EXIT => {
            DestroyWindow(hwnd).ok();
        }
        _ => {}
    }
}

fn to_wide(s: &str) -> Vec<u16> {
    let mut v: Vec<u16> = s.encode_utf16().collect();
    v.push(0);
    v
}

unsafe fn msg_box(hwnd: HWND, title: &str, body: &str, warn: bool, err: bool) {
    let icon = if err {
        MB_ICONERROR
    } else if warn {
        MB_ICONWARNING
    } else {
        MB_ICONINFORMATION
    };
    let body_w = to_wide(body);
    let title_w = to_wide(title);
    MessageBoxW(
        hwnd,
        PCWSTR(body_w.as_ptr()),
        PCWSTR(title_w.as_ptr()),
        icon | MB_OK,
    );
}

/// Folder picker → `cairn attach <path>`. The daemon does enrollment state,
/// scanning, chunking, mount — no terminal ever appears.
unsafe fn do_connect(hwnd: HWND, shared: &Arc<Shared>) {
    let Some(path) = pick_folder(hwnd) else {
        return;
    };
    shared.poll_now.store(true, Ordering::Relaxed);
    let (ok, text) = run_cairn(&["attach", &path]);
    if ok {
        msg_box(
            hwnd,
            "Cairn — Connected",
            &format!("Project attached:\n\n{}\n\nThe folder is now syncing — see the tray icon for progress.", text),
            false,
            false,
        );
    } else {
        msg_box(
            hwnd,
            "Cairn — Could not connect",
            &format!("attach failed:\n\n{text}\n\nIs the daemon running? Status Details runs the doctor."),
            false,
            true,
        );
    }
}

unsafe fn pick_folder(hwnd: HWND) -> Option<String> {
    let title = to_wide("Choose the project folder to sync");
    let mut display = [0u16; 260];
    let bi = BROWSEINFOW {
        hwndOwner: hwnd,
        pidlRoot: std::ptr::null_mut(),
        pszDisplayName: windows::core::PWSTR(display.as_mut_ptr()),
        lpszTitle: PCWSTR(title.as_ptr()),
        ulFlags: 0x0040, // BIF_RETURNONLYFSDIRS
        lpfn: None,
        lParam: LPARAM(0),
        iImage: 0,
    };
    let pidl = SHBrowseForFolderW(&bi);
    if pidl.is_null() {
        return None; // cancelled
    }
    let mut path = [0u16; 260];
    let ok = SHGetPathFromIDListW(pidl, &mut path).as_bool();
    windows::Win32::System::Com::CoTaskMemFree(Some(pidl.cast()));
    if !ok {
        return None;
    }
    let len = path.iter().position(|&c| c == 0).unwrap_or(0);
    Some(String::from_utf16_lossy(&path[..len]))
}

unsafe fn do_status(hwnd: HWND) {
    let (ok, text) = run_cairn(&["doctor"]);
    let body = if text.is_empty() {
        "(doctor returned no output)".into()
    } else {
        text
    };
    msg_box(
        hwnd,
        if ok {
            "Cairn — Everything is OK"
        } else {
            "Cairn — Attention needed"
        },
        &body,
        !ok,
        false,
    );
}

unsafe fn do_open(status: &LiveStatus) {
    let Some(root) = status.project_root.as_deref() else {
        return;
    };
    let wide = to_wide(root);
    ShellExecuteW(
        None,
        w!("open"),
        PCWSTR(wide.as_ptr()),
        None,
        None,
        windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL,
    );
}

unsafe fn do_settings(hwnd: HWND, status: &LiveStatus) {
    let (enrolled, home) = {
        let (ok, text) = run_cairn(&["init", "--json"]);
        let mut enrolled = String::from("unknown");
        let mut home = String::new();
        if ok {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                enrolled = v
                    .get("enrolled")
                    .and_then(|b| b.as_bool())
                    .map(|b| if b { "yes".into() } else { "no".into() })
                    .unwrap_or_else(|| "unknown".into());
                home = v
                    .get("home")
                    .and_then(|h| h.as_str())
                    .unwrap_or_default()
                    .to_string();
            }
        }
        (enrolled, home)
    };
    let body = format!(
        "Version: {}\nEnrolled: {enrolled}\nHome: {home}\n\nProject: {}\nRoot: {}\nState: {}\nFiles synced: {}\nPending: {}",
        env!("CARGO_PKG_VERSION"),
        status.project_id.as_deref().unwrap_or("(none)"),
        status.project_root.as_deref().unwrap_or("(none)"),
        status.project_state.as_deref().unwrap_or("(none)"),
        status.files_synced.map(|f| f.to_string()).unwrap_or_else(|| "-".into()),
        status.pending_outbox.map(|p| p.to_string()).unwrap_or_else(|| "-".into()),
    );
    msg_box(hwnd, "Cairn — Settings", &body, false, false);
}

unsafe fn do_disconnect(hwnd: HWND, shared: &Arc<Shared>, status: &LiveStatus) {
    let Some(pid) = status.project_id.as_deref() else {
        return;
    };
    shared.poll_now.store(true, Ordering::Relaxed);
    let (ok, text) = run_cairn(&["detach", "--project", pid]);
    msg_box(
        hwnd,
        if ok {
            "Cairn — Disconnected"
        } else {
            "Cairn — Disconnect failed"
        },
        &format!("project {pid}:\n\n{text}\n\n(local files are never touched by detach)"),
        !ok,
        !ok,
    );
}
