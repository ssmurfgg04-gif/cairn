//! Native folder picker (round 27, "click, don't type"): the OS dialog
//! behind the dashboard's Add-to-Workspace Browse button and the
//! onboarding attach scene.
//!
//! SAFETY rationale (the module opts into `unsafe`, lib.rs's deny stays
//! the crate policy): `SHBrowseForFolderW` is raw C FFI. The call
//! sequence is the SAME one cairn-tray has shipped since round 19
//! (tray.rs `pick_folder`) — the legacy folder dialog:
//! * no COM apartment juggling, no IFileDialog generic bounds;
//! * works with a NULL owner HWND (the daemon owns no window);
//! * the returned PIDL is freed with `CoTaskMemFree` exactly once, on
//!   every path (the tray's contract);
//! * all buffers are stack arrays with fixed capacity (MAX_PATH);
//! * every FFI result is checked; failure paths return `None`, never a
//!   half-constructed string.
//!
//! The rest of the crate stays `deny(unsafe_code)`: this module is the
//! boundary, reviewed like `cfapi`/`badge` (the WO6-9 unsafe policy).

#![allow(unsafe_code)]

/// What the dialog decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Picked {
    /// The user chose this filesystem folder.
    Folder(String),
    /// The user closed the dialog without choosing (not an error).
    Cancelled,
    /// No dialog could be shown on this host (no interactive session,
    /// non-Windows target). The UI keeps the typed path centered.
    Unsupported,
}

/// Open the native folder dialog and return the choice. Blocks until
/// the user decides — run it on a blocking-pool thread
/// (`tokio::task::spawn_blocking`), never the async runtime.
#[cfg(windows)]
pub fn pick_folder() -> Picked {
    // SAFETY: see the module doc — the tray's shipped sequence, all
    // results checked, PIDL freed on every path.
    unsafe {
        use windows::core::PCWSTR;
        use windows::Win32::System::Com::CoTaskMemFree;
        use windows::Win32::UI::Shell::{SHBrowseForFolderW, SHGetPathFromIDListW, BROWSEINFOW};

        let title: Vec<u16> = "Choose a project folder"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let mut display = [0u16; 260];
        let bi = BROWSEINFOW {
            // HWND is a newtype in windows 0.58 — null wrapped, not raw
            hwndOwner: windows::Win32::Foundation::HWND(std::ptr::null_mut()), // the daemon owns no window
            pidlRoot: std::ptr::null_mut(),
            pszDisplayName: windows::core::PWSTR(display.as_mut_ptr()),
            lpszTitle: PCWSTR(title.as_ptr()),
            ulFlags: 0x0040, // BIF_RETURNONLYFSDIRS
            lpfn: None,
            lParam: windows::Win32::Foundation::LPARAM(0),
            iImage: 0,
        };
        let pidl = SHBrowseForFolderW(&bi);
        if pidl.is_null() {
            return Picked::Cancelled; // user closed the dialog
        }
        let mut path = [0u16; 260];
        let ok = SHGetPathFromIDListW(pidl, &mut path).as_bool();
        CoTaskMemFree(Some(pidl.cast()));
        if !ok {
            return Picked::Cancelled;
        }
        let len = path.iter().position(|&c| c == 0).unwrap_or(0);
        if len == 0 {
            return Picked::Cancelled;
        }
        Picked::Folder(String::from_utf16_lossy(&path[..len]))
    }
}

/// Non-Windows host: no dialog from the daemon — the attach flows type
/// a path or use the CLI. The UI offers the text input.
#[cfg(not(windows))]
pub fn pick_folder() -> Picked {
    Picked::Unsupported
}
