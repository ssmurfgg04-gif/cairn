#![allow(unsafe_code)]
// Win32 folder picker APIs take raw pointers; suppressed module-wide.
#![allow(clippy::borrow_as_ptr)]

/// What the dialog decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Picked {
    Folder(String),
    Cancelled,
    Unsupported,
}

#[cfg(windows)]
pub fn pick_folder() -> Picked {
    if let Some(p) = try_modern() {
        return p;
    }
    try_legacy()
}

#[cfg(windows)]
fn try_modern() -> Option<Picked> {
    unsafe {
        use windows::Win32::System::Com::{
            CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
            COINIT_APARTMENTTHREADED,
        };
        use windows::Win32::UI::Shell::{
            FileOpenDialog, IFileOpenDialog, IShellItem, FOS_PICKFOLDERS, SIGDN_FILESYSPATH,
        };
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let dialog: Result<IFileOpenDialog, _> =
            CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER);
        let dialog = match dialog {
            Ok(d) => d,
            Err(_) => {
                CoUninitialize();
                return None;
            }
        };
        let opts = dialog
            .GetOptions()
            .unwrap_or(windows::Win32::UI::Shell::FILEOPENDIALOGOPTIONS(0));
        let _ = dialog.SetOptions(windows::Win32::UI::Shell::FILEOPENDIALOGOPTIONS(
            opts.0 | FOS_PICKFOLDERS.0,
        ));
        let _ = dialog.SetTitle(windows::core::w!("Choose a project folder"));
        let hr = dialog.Show(None);
        if hr.is_err() {
            CoUninitialize();
            return Some(Picked::Cancelled);
        }
        let item: Result<IShellItem, _> = dialog.GetResult();
        let item = match item {
            Ok(i) => i,
            Err(_) => {
                CoUninitialize();
                return Some(Picked::Cancelled);
            }
        };
        let pwstr = match item.GetDisplayName(SIGDN_FILESYSPATH) {
            Ok(s) => s,
            Err(_) => {
                CoUninitialize();
                return Some(Picked::Cancelled);
            }
        };
        let s = pwstr.to_string().unwrap_or_default();
        windows::Win32::System::Com::CoTaskMemFree(Some(pwstr.0 as *const _));
        CoUninitialize();
        if s.is_empty() {
            Some(Picked::Cancelled)
        } else {
            Some(Picked::Folder(s))
        }
    }
}

#[cfg(windows)]
fn try_legacy() -> Picked {
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
            hwndOwner: windows::Win32::Foundation::HWND(std::ptr::null_mut()),
            pidlRoot: std::ptr::null_mut(),
            pszDisplayName: windows::core::PWSTR(display.as_mut_ptr()),
            lpszTitle: PCWSTR(title.as_ptr()),
            ulFlags: 0x0040,
            lpfn: None,
            lParam: windows::Win32::Foundation::LPARAM(0),
            iImage: 0,
        };
        let pidl = SHBrowseForFolderW(&bi);
        if pidl.is_null() {
            return Picked::Cancelled;
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

#[cfg(not(windows))]
pub fn pick_folder() -> Picked {
    Picked::Unsupported
}
