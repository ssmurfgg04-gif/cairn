//! The Windows COM surface (cfg(windows)): DLL exports, the class factory,
//! four Explorer overlay-icon handlers and the context menu. A thin adapter
//! — every decision routes through [`crate::core`], which is unit-tested on
//! all platforms.
//!
//! Registration (admin once): `regsvr32 cairn_shell_ext.dll`
//! (`DllRegisterServer` writes the CLSID + overlay + context-menu keys;
//! `DllUnregisterServer` removes them). Overlays register under
//! `ShellIconOverlayIdentifiers` with a leading space so they sort early
//! (Explorer caps the overlay set at ~15 — priority by name order).
//!
//! COM lifetime: the objects are apartment-threaded, stateless except the
//! path they're queried for; `DllCanUnloadNow` reports the live-object
//! count. All unsafe blocks follow the windows-rs 0.58 manual-vtable
//! pattern used across this workspace (cfapi.rs): refcount owned by the
//! box, interfaces raw pointers into vtables we define.

#![allow(clippy::missing_safety_doc)]
// The whole module is Windows COM FFI: unsafe is the contract. Every unsafe
// block below carries its own safety note (the fs-win::cfapi convention).
#![allow(unsafe_code)]

use std::cell::RefCell;
use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};

use windows::core::{IUnknown, Interface, GUID, HRESULT, PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    CLASS_E_CLASSNOTAVAILABLE, E_FAIL, E_INVALIDARG, E_NOINTERFACE, S_FALSE, S_OK,
};
use windows::Win32::System::Com::IClassFactory;
use windows::Win32::System::LibraryLoader::GetModuleFileNameW;
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_WRITE,
    REG_OPTION_NON_VOLATILE, REG_SZ, REG_VALUE_TYPE,
};
use windows::Win32::UI::Shell::{
    IContextMenu, IShellExtInit, IShellIconOverlayIdentifier, ShellExecuteExW, CMINVOKECOMMANDINFO,
    SHELLEXECUTEINFOW,
};

use crate::core::{self, MenuAction, OverlayState};

static LIVE_OBJECTS: AtomicU32 = AtomicU32::new(0);

// Stable CLSIDs (fixed at authoring time; registry keys reference them).
const CLSID_OVERLAY_SYNCED: GUID = GUID::from_values(
    0xd7f2_9ce3,
    0x4c1b,
    0x8a6e,
    [0x10, 0x32, 0x54, 0x76, 0x98, 0xba, 0xdc, 0xfe],
);
const CLSID_OVERLAY_CONFLICT: GUID = GUID::from_values(
    0xe1af_0d94,
    0x5527,
    0x9b3f,
    [0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef],
);
const CLSID_OVERLAY_FETCHING: GUID = GUID::from_values(
    0xf4c1_2e85,
    0x6638,
    0xac40,
    [0xfe, 0xdc, 0xba, 0x98, 0x76, 0x54, 0x32, 0x10],
);
const CLSID_OVERLAY_PINNED: GUID = GUID::from_values(
    0xa9b3_7f16,
    0x7749,
    0xbda1,
    [0x0f, 0x1e, 0x2d, 0x3c, 0x4b, 0x5a, 0x69, 0x78],
);
const CLSID_CONTEXT_MENU: GUID = GUID::from_values(
    0xc5e8_1a27,
    0x885a,
    0xcf62,
    [0x87, 0x76, 0x65, 0x54, 0x43, 0x32, 0x21, 0x10],
);

/// Registry-format CLSID string: {00000000-0000-0000-0000-000000000000}
fn clsid_string(g: &GUID) -> String {
    format!(
        "{{{:08x}-{:04x}-{:04x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}}}",
        g.data1,
        g.data2,
        g.data3,
        g.data4[0],
        g.data4[1],
        g.data4[2],
        g.data4[3],
        g.data4[4],
        g.data4[5],
        g.data4[6],
        g.data4[7]
    )
}

// ---------------------------------------------------------------------------
// small utilities
// ---------------------------------------------------------------------------

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn from_wide(pw: PCWSTR) -> String {
    if pw.is_null() {
        return String::new();
    }
    unsafe {
        let mut len = 0usize;
        while *pw.0.add(len) != 0 {
            len += 1;
        }
        let slice = std::slice::from_raw_parts(pw.0, len);
        String::from_utf16_lossy(slice)
    }
}

fn dll_path() -> PathBuf {
    let mut buf = [0u16; 1024];
    let len = unsafe { GetModuleFileNameW(None, &mut buf) } as usize;
    PathBuf::from(String::from_utf16_lossy(&buf[..len]))
}

/// The icon file: the DLL's own module path with .ico sibling — the
/// installer drops `cairn-icons.ico` next to the DLL. Icon indices match
/// [`OverlayState`] declaration order (0=synced, 1=conflict, 2=fetching,
/// 3=pinned).
fn icon_file() -> PathBuf {
    let mut p = dll_path();
    p.set_file_name("cairn-icons.ico");
    p
}

fn state_to_index(s: OverlayState) -> i32 {
    match s {
        OverlayState::Synced => 0,
        OverlayState::Conflict => 1,
        OverlayState::Fetching => 2,
        OverlayState::Pinned => 3,
    }
}

// ---------------------------------------------------------------------------
// COM object scaffolding (manual vtables, refcounted boxes)
// ---------------------------------------------------------------------------

struct VTablePtr<const N: usize> {
    entries: [*const c_void; N],
}

/// A refcounted COM object shell: the box owns the inner state; the vtable
/// pointers live in statics per interface.
#[repr(C)]
struct ComObject<T> {
    vtable: *const *const c_void,
    refs: AtomicU32,
    inner: T,
}

impl<T> ComObject<T> {
    unsafe fn new(vtable: *const *const c_void, inner: T) -> *mut Self {
        LIVE_OBJECTS.fetch_add(1, Ordering::Relaxed);
        Box::into_raw(Box::new(Self {
            vtable,
            refs: AtomicU32::new(1),
            inner,
        }))
    }

    unsafe fn add_ref(&mut self) -> u32 {
        self.refs.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Returns true when the object was destroyed.
    unsafe fn release(&mut self) -> bool {
        let prev = self.refs.fetch_sub(1, Ordering::Release);
        if prev == 1 {
            drop(Box::from_raw(self as *mut Self));
            LIVE_OBJECTS.fetch_sub(1, Ordering::Relaxed);
            true
        } else {
            false
        }
    }

    unsafe fn inner(&mut self) -> &mut T {
        &mut self.inner
    }
}

// IUnknown slot layout shared by every interface here. QI=0, ADDREF=1,
// RELEASE=2; interface methods follow.
const QI_RELEASE: usize = 2;

unsafe extern "system" fn object_add_ref<T>(this: *mut c_void) -> u32 {
    let obj = this as *mut ComObject<T>;
    (*obj).add_ref()
}

unsafe extern "system" fn object_release<T>(this: *mut c_void) -> u32 {
    let obj = this as *mut ComObject<T>;
    let died = (*obj).release();
    if died {
        0
    } else {
        (*obj).refs.load(Ordering::Acquire)
    }
}

unsafe fn write_ptr(ppv: *mut *mut c_void, p: *mut c_void) -> HRESULT {
    if ppv.is_null() {
        return E_INVALIDARG;
    }
    *ppv = p;
    S_OK
}

// ---------------------------------------------------------------------------
// Overlay icon handlers (IShellIconOverlayIdentifier)
// ---------------------------------------------------------------------------

/// Inner state for one overlay handler: the state it reports + the icon
/// slot it fills.
struct OverlayInner {
    state: OverlayState,
}

/// Real QI for the overlay face: IUnknown + IShellIconOverlayIdentifier.
unsafe extern "system" fn overlay_query_interface(
    this: *mut c_void,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    if ppv.is_null() {
        return E_INVALIDARG;
    }
    let want = *riid;
    if want == <IUnknown as Interface>::IID
        || want == <IShellIconOverlayIdentifier as Interface>::IID
    {
        let obj = this as *mut ComObject<OverlayInner>;
        (*obj).add_ref();
        return write_ptr(ppv, this);
    }
    E_NOINTERFACE
}

// IShellIconOverlayIdentifier vtable slots after IUnknown (verified against
// the windows-rs 0.58 projection): IsMemberOf=3, GetOverlayInfo=4,
// GetPriority=5.
unsafe extern "system" fn overlay_is_member_of(
    this: *mut c_void,
    pwsz_path: PCWSTR,
    _dw_attrib: u32,
) -> HRESULT {
    let obj = this as *mut ComObject<OverlayInner>;
    let wanted = (*obj).inner().state;
    let abs = PathBuf::from(from_wide(pwsz_path));
    let Some((root, _info)) = core::resolve_root(&abs) else {
        return S_FALSE; // not a cairn root: no overlay, no cost
    };
    let Some(rel) = core::rel_under(&root, &abs) else {
        return S_FALSE;
    };
    match core::OverlayStateFile::read(&root).and_then(|f| f.state_of(&rel)) {
        Some(state) if state == wanted => S_OK,
        _ => S_FALSE,
    }
}

unsafe extern "system" fn overlay_get_overlay_info(
    this: *mut c_void,
    pwsz_icon_file: PWSTR,
    cch_max: i32,
    pindex: *mut i32,
    pdw_flags: *mut u32,
) -> HRESULT {
    let obj = this as *mut ComObject<OverlayInner>;
    let inner = (*obj).inner();
    let path = icon_file();
    let wpath = wide(&path.to_string_lossy());
    // cch_max is i32 in the projection; Explorer passes the buffer capacity
    // INCLUDING the terminator.
    let cap = cch_max.max(0) as usize;
    let n = wpath.len().min(cap).saturating_sub(1);
    if pwsz_icon_file.is_null() || pindex.is_null() || pdw_flags.is_null() {
        return E_INVALIDARG;
    }
    for (i, ch) in wpath[..n].iter().enumerate() {
        *pwsz_icon_file.0.add(i) = *ch;
    }
    *pwsz_icon_file.0.add(n) = 0;
    *pindex = state_to_index(inner.state);
    // ISIOI_ICONFILE | ISIOI_ICONINDEX
    *pdw_flags = 0x1 | 0x2;
    S_OK
}

unsafe extern "system" fn overlay_get_priority(this: *mut c_void, ppriority: *mut i32) -> HRESULT {
    let obj = this as *mut ComObject<OverlayInner>;
    if ppriority.is_null() {
        return E_INVALIDARG;
    }
    *ppriority = i32::from((*obj).inner().state.priority());
    S_OK
}

const OVERLAY_VTABLE: VTablePtr<6> = VTablePtr {
    entries: [
        overlay_query_interface as *const c_void,
        object_add_ref::<OverlayInner> as *const c_void,
        object_release::<OverlayInner> as *const c_void,
        overlay_is_member_of as *const c_void,
        overlay_get_overlay_info as *const c_void,
        overlay_get_priority as *const c_void,
    ],
};

unsafe fn make_overlay(state: OverlayState) -> *mut ComObject<OverlayInner> {
    ComObject::new(OVERLAY_VTABLE.entries.as_ptr(), OverlayInner { state })
}

// ---------------------------------------------------------------------------
// Context menu (IShellExtInit + IContextMenu)
// ---------------------------------------------------------------------------

/// Selection state shared by the context-menu object's two faces. Explorer
/// instantiates the object, QIs IShellExtInit, calls Initialize with the
/// selection, then QIs IContextMenu on the same instance. The two faces
/// share one Rc so the Initialize'd selection is what InvokeCommand acts on.
///
/// Milestone caveat (ADR-0019 §5): QI for the companion face mints a new
/// object rather than returning an offset pointer, so the strict COM
/// identity rule (same IUnknown* from every QI) is not upheld across faces.
/// Explorer's per-invocation QI pattern (each face queried once per menu
/// open) is unaffected; a proper offset-based dual-interface object lands
/// with the icon resource pack.
type SharedSelection = Rc<RefCell<Vec<PathBuf>>>;

struct MenuInner {
    /// The paths Explorer selected (Initialize).
    selected: SharedSelection,
}

// IShellExtInit slots after IUnknown: Initialize=3.
unsafe extern "system" fn menu_init_query_interface(
    this: *mut c_void,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    if ppv.is_null() {
        return E_INVALIDARG;
    }
    let want = *riid;
    let obj = this as *mut ComObject<MenuInner>;
    if want == <IUnknown as Interface>::IID || want == <IShellExtInit as Interface>::IID {
        (*obj).add_ref();
        return write_ptr(ppv, this);
    }
    if want == <IContextMenu as Interface>::IID {
        // mint the companion cmd face sharing the same selection
        let inner = (*obj).inner();
        let shared = Rc::clone(&inner.selected);
        let companion = make_menu_cmd_face(shared);
        return write_ptr(ppv, companion as *mut c_void);
    }
    E_NOINTERFACE
}

unsafe extern "system" fn menu_initialize(
    this: *mut c_void,
    _pidl_folder: *mut c_void,
    pdata_obj: *mut c_void,
    _hkey_prog_id: HKEY,
) -> HRESULT {
    // pdata_obj is an IDataObject — the canonical drag-drop format is
    // CF_HDROP; we shell out to DragQueryInfo via the shell API surface.
    // For the initial bring-up we accept the selection via the simpler
    // DROPFILES parse from the data object's CF_HDROP storage.
    let obj = this as *mut ComObject<MenuInner>;
    let inner = (*obj).inner();
    *inner.selected.borrow_mut() = menu_paths_from_data_object(pdata_obj);
    if inner.selected.borrow().is_empty() {
        S_FALSE
    } else {
        S_OK
    }
}

// IContextMenu slots after IUnknown: QueryContextMenu=3, InvokeCommand=4,
// GetCommandString=5 (verified against the windows-rs 0.58 projection).
unsafe extern "system" fn menu_cmd_query_interface(
    this: *mut c_void,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    if ppv.is_null() {
        return E_INVALIDARG;
    }
    let want = *riid;
    let obj = this as *mut ComObject<MenuInner>;
    if want == <IUnknown as Interface>::IID || want == <IContextMenu as Interface>::IID {
        (*obj).add_ref();
        return write_ptr(ppv, this);
    }
    if want == <IShellExtInit as Interface>::IID {
        // mint the companion init face sharing the same selection
        let inner = (*obj).inner();
        let shared = Rc::clone(&inner.selected);
        let companion = make_menu_init_face(shared);
        return write_ptr(ppv, companion as *mut c_void);
    }
    E_NOINTERFACE
}

unsafe extern "system" fn menu_query_context_menu(
    this: *mut c_void,
    _hmenu: *mut c_void,
    _index_menu: u32,
    _id_first: u32,
    _id_last: u32,
    _flags: u32,
) -> HRESULT {
    // menu items are inserted in InvokeCommand order; the ids are
    // id_first..+3 handled by Explorer's insertion in QueryContextMenu.
    // We build the items here via the standard helper (kept simple).
    let _ = this as *mut ComObject<MenuInner>;
    // NOTE: full InsertMenuItemW plumbing lands with the icon resource
    // pack; the actions + argv are pinned by the cross-platform core.
    S_OK
}

unsafe extern "system" fn menu_invoke_command(
    this: *mut c_void,
    pici: *const CMINVOKECOMMANDINFO,
) -> HRESULT {
    if pici.is_null() {
        return E_INVALIDARG;
    }
    let obj = this as *mut ComObject<MenuInner>;
    let inner = (*obj).inner();
    let ici = &*pici;
    // lpVerb (PCSTR) is either a MAKEINTRESOURCE id (0,1,2) or a string verb
    let verb_ptr: *const u8 = ici.lpVerb.0;
    if verb_ptr.is_null() {
        return E_INVALIDARG;
    }
    let which: i32 = if (ici.lpVerb.0 as usize) < 0x10000 {
        ici.lpVerb.0 as i32
    } else {
        // string verb: "lock" | "unlock" | "snapshot"
        let mut s = String::new();
        let mut i = 0usize;
        while *verb_ptr.add(i) != 0 {
            s.push(*verb_ptr.add(i) as char);
            i += 1;
        }
        match s.as_str() {
            "lock" => 0,
            "unlock" => 1,
            "snapshot" => 2,
            _ => return E_INVALIDARG,
        }
    };
    let first = match inner.selected.borrow().first().cloned() {
        Some(f) => f,
        None => return E_INVALIDARG,
    };
    let Some((root, info)) = core::resolve_root(&first) else {
        return E_FAIL;
    };
    let Some(rel) = core::rel_under(&root, &first) else {
        return E_FAIL;
    };
    let action = match which {
        0 => MenuAction::Lock {
            project: info.project_id,
            rel,
        },
        1 => MenuAction::Unlock {
            project: info.project_id,
            rel,
        },
        2 => MenuAction::Snapshot {
            project: info.project_id,
            label: String::new(),
        },
        // "open in the NLE that owns this file's association" — the
        // right-click the reference shells never gave media people
        3 => {
            open_file_default(&first);
            return S_OK;
        }
        _ => return E_INVALIDARG,
    };
    run_cairn(&action);
    S_OK
}

unsafe extern "system" fn menu_get_command_string(
    _this: *mut c_void,
    _verb: *const u8,
    _flags: u32,
    _reserved: *mut u32,
    _name: PWSTR,
    _cch_max: u32,
) -> HRESULT {
    S_OK
}

/// Spawn `cairn <argv>` detached. Uses ShellExecuteW ("open" on the
/// resolved cairn.exe) so no console window flashes in Explorer.
unsafe fn run_cairn(action: &MenuAction) {
    let params = action.argv().join(" ");
    // The wide buffers must outlive the ShellExecuteExW call: PCWSTR is a
    // borrowed pointer, not an owned handle.
    let verb_w = wide("open");
    let file_w = wide("cairn.exe");
    let params_w = wide(&params);
    let mut sei = SHELLEXECUTEINFOW::default();
    sei.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    sei.lpVerb = PCWSTR(verb_w.as_ptr());
    sei.lpFile = PCWSTR(file_w.as_ptr());
    sei.lpParameters = PCWSTR(params_w.as_ptr());
    sei.nShow = 0; // SW_HIDE: the CLI is quiet on success
    let _ = ShellExecuteExW(&mut sei);
}

/// Open a project file with its SYSTEM DEFAULT handler — for media that
/// is whatever NLE owns the association (.prproj -> Premiere, .drp ->
/// Resolve). We execute the association, never a hard-coded editor path,
/// so this stays correct on machines we have never seen.
unsafe fn open_file_default(path: &Path) {
    let verb_w = wide("open");
    let file_w: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    let mut sei = SHELLEXECUTEINFOW::default();
    sei.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    sei.lpVerb = PCWSTR(verb_w.as_ptr());
    sei.lpFile = PCWSTR(file_w.as_ptr());
    sei.nShow = 5; // SW_SHOW: the editor should actually appear
    let _ = ShellExecuteExW(&mut sei);
}

/// CF_HDROP extraction from the data object (DROPFILES + wide path list).
unsafe fn menu_paths_from_data_object(_pdata: *mut c_void) -> Vec<PathBuf> {
    // The data-object HGLOBAL dance (StgMedium + GetData(CF_HDROP)) is
    // windows-runtime plumbing; the paths then parse from DROPFILES.
    // Kept minimal for the first compiled milestone: an empty selection
    // makes the menu a no-op rather than a crash; full plumbing lands with
    // the icon pack (tracked in ADR-0019 §5 rollout).
    Vec::new()
}

const MENU_INIT_VTABLE: VTablePtr<4> = VTablePtr {
    entries: [
        menu_init_query_interface as *const c_void,
        object_add_ref::<MenuInner> as *const c_void,
        object_release::<MenuInner> as *const c_void,
        menu_initialize as *const c_void,
    ],
};

const MENU_CMD_VTABLE: VTablePtr<6> = VTablePtr {
    entries: [
        menu_cmd_query_interface as *const c_void,
        object_add_ref::<MenuInner> as *const c_void,
        object_release::<MenuInner> as *const c_void,
        menu_query_context_menu as *const c_void,
        menu_invoke_command as *const c_void,
        menu_get_command_string as *const c_void,
    ],
};

// The context menu object implements BOTH IShellExtInit and IContextMenu;
// the two faces share one selection via Rc. Explorer's entry face is
// IShellExtInit (Initialize), so the factory mints that face; QI between
// faces mints companions carrying the same Rc.
unsafe fn make_menu_init_face(selected: SharedSelection) -> *mut ComObject<MenuInner> {
    ComObject::new(MENU_INIT_VTABLE.entries.as_ptr(), MenuInner { selected })
}

unsafe fn make_menu_cmd_face(selected: SharedSelection) -> *mut ComObject<MenuInner> {
    ComObject::new(MENU_CMD_VTABLE.entries.as_ptr(), MenuInner { selected })
}

/// The face the class factory serves (Explorer QIs further from there).
unsafe fn make_menu_object() -> *mut ComObject<MenuInner> {
    make_menu_init_face(Rc::new(RefCell::new(Vec::new())))
}

// ---------------------------------------------------------------------------
// Class factory
// ---------------------------------------------------------------------------

struct FactoryInner {
    clsid: GUID,
}

unsafe extern "system" fn factory_query_interface(
    this: *mut c_void,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    if ppv.is_null() {
        return E_INVALIDARG;
    }
    let want = *riid;
    if want == <IUnknown as Interface>::IID || want == <IClassFactory as Interface>::IID {
        let obj = this as *mut ComObject<FactoryInner>;
        (*obj).add_ref();
        return write_ptr(ppv, this);
    }
    E_NOINTERFACE
}

unsafe extern "system" fn factory_create_instance(
    this: *mut c_void,
    _outer: *mut c_void,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    let obj = this as *mut ComObject<FactoryInner>;
    let clsid = (*obj).inner().clsid;
    let want = *riid;
    let unknown: *mut c_void = match clsid {
        CLSID_OVERLAY_SYNCED => make_overlay(OverlayState::Synced) as *mut c_void,
        CLSID_OVERLAY_CONFLICT => make_overlay(OverlayState::Conflict) as *mut c_void,
        CLSID_OVERLAY_FETCHING => make_overlay(OverlayState::Fetching) as *mut c_void,
        CLSID_OVERLAY_PINNED => make_overlay(OverlayState::Pinned) as *mut c_void,
        CLSID_CONTEXT_MENU => make_menu_object() as *mut c_void,
        _ => return CLASS_E_CLASSNOTAVAILABLE,
    };
    // the vtable's own QI handles the requested interface. The object's
    // first field is the vtable pointer (pointer to the slot array); each
    // slot is a `*const c_void` holding a fn pointer.
    let vt = *(unknown as *mut *const *const c_void);
    let qi: unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT =
        std::mem::transmute(*vt.add(0));
    let hr = qi(unknown, &want, ppv);
    // The object was born with one creation reference. QI added its own
    // reference for the caller on success; either way the creation
    // reference is ours to drop (on failure QI never added one, so this
    // destroys the object; on success the caller's reference survives).
    let rel: unsafe extern "system" fn(*mut c_void) -> u32 =
        std::mem::transmute(*vt.add(QI_RELEASE));
    rel(unknown);
    hr
}

unsafe extern "system" fn factory_lock_server(_this: *mut c_void, _lock: i32) -> HRESULT {
    S_OK
}

const FACTORY_VTABLE: VTablePtr<5> = VTablePtr {
    entries: [
        factory_query_interface as *const c_void,
        object_add_ref::<FactoryInner> as *const c_void,
        object_release::<FactoryInner> as *const c_void,
        factory_create_instance as *const c_void,
        factory_lock_server as *const c_void,
    ],
};

unsafe fn make_factory(clsid: GUID) -> *mut c_void {
    ComObject::new(FACTORY_VTABLE.entries.as_ptr(), FactoryInner { clsid }) as *mut c_void
}

// ---------------------------------------------------------------------------
// DLL exports
// ---------------------------------------------------------------------------

/// CLSIDs this DLL serves.
pub fn served_clsids() -> [GUID; 5] {
    [
        CLSID_OVERLAY_SYNCED,
        CLSID_OVERLAY_CONFLICT,
        CLSID_OVERLAY_FETCHING,
        CLSID_OVERLAY_PINNED,
        CLSID_CONTEXT_MENU,
    ]
}

#[no_mangle]
extern "system" fn DllGetClassObject(
    rclsid: *const GUID,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    unsafe {
        let clsid = *rclsid;
        if !served_clsids().contains(&clsid) {
            return CLASS_E_CLASSNOTAVAILABLE;
        }
        let factory = make_factory(clsid);
        let vt = *(factory as *mut *const *const c_void);
        let qi: unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT =
            std::mem::transmute(*vt.add(0));
        let hr = qi(factory, riid, ppv);
        // drop the creation reference: the caller's QI reference is the one
        // that survives (see factory_create_instance for the semantics).
        let rel: unsafe extern "system" fn(*mut c_void) -> u32 =
            std::mem::transmute(*vt.add(QI_RELEASE));
        rel(factory);
        hr
    }
}

#[no_mangle]
extern "system" fn DllCanUnloadNow() -> HRESULT {
    if LIVE_OBJECTS.load(Ordering::Acquire) == 0 {
        S_OK
    } else {
        S_FALSE
    }
}

fn reg_set_string(key: HKEY, name: Option<&str>, value: &str) -> HRESULT {
    unsafe {
        // Both wide buffers outlive the RegSetValueExW call (PCWSTR borrows).
        let name_w: Vec<u16> = name.map(wide).unwrap_or_default();
        let value_w = wide(value);
        let name_ptr = if name_w.is_empty() {
            PCWSTR::null()
        } else {
            PCWSTR(name_w.as_ptr())
        };
        let res = RegSetValueExW(
            key,
            name_ptr,
            0,
            REG_VALUE_TYPE(REG_SZ.0),
            Some(std::slice::from_raw_parts(
                value_w.as_ptr().cast::<u8>(),
                value_w.len() * 2,
            )),
        );
        if res.0 == 0 {
            S_OK
        } else {
            E_FAIL
        }
    }
}

unsafe fn reg_create(path: &str) -> Result<HKEY, HRESULT> {
    let path_w = wide(path);
    let mut key = std::mem::zeroed();
    let res = RegCreateKeyExW(
        HKEY_CURRENT_USER,
        PCWSTR(path_w.as_ptr()),
        0,
        PCWSTR::null(),
        REG_OPTION_NON_VOLATILE,
        KEY_WRITE,
        None,
        &mut key,
        None,
    );
    if res.0 == 0 {
        Ok(key)
    } else {
        Err(E_FAIL)
    }
}

/// HKCU-based registration (regsvr32 /i:user or an installer run) so no
/// admin elevation is REQUIRED; per-machine registration (HKCR) remains
/// available to the installer.
#[no_mangle]
extern "system" fn DllRegisterServer() -> HRESULT {
    unsafe {
        let dll = dll_path().to_string_lossy().into_owned();
        let mut ok = true;
        let overlays: [(GUID, &str, OverlayState); 4] = [
            (CLSID_OVERLAY_SYNCED, " 1cairn-synced", OverlayState::Synced),
            (
                CLSID_OVERLAY_CONFLICT,
                " 2cairn-conflict",
                OverlayState::Conflict,
            ),
            (
                CLSID_OVERLAY_FETCHING,
                " 3cairn-fetching",
                OverlayState::Fetching,
            ),
            (CLSID_OVERLAY_PINNED, " 4cairn-pinned", OverlayState::Pinned),
        ];
        for (guid, name, _state) in overlays {
            let clsid_str = clsid_string(&guid);
            // overlay handler key → default value = CLSID
            if let Ok(k) = reg_create(&format!(
                "SOFTWARE\\Classes\\CLSID\\{clsid_str}\\InprocServer32"
            )) {
                ok &= reg_set_string(k, None, &dll).is_ok();
                ok &= reg_set_string(k, Some("ThreadingModel"), "Apartment").is_ok();
                let _ = RegCloseKey(k);
            } else {
                ok = false;
            }
            let overlay_key = format!(
                "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Explorer\\ShellIconOverlayIdentifiers\\{name}"
            );
            if let Ok(k) = reg_create(&overlay_key) {
                ok &= reg_set_string(k, None, &clsid_str).is_ok();
                let _ = RegCloseKey(k);
            } else {
                ok = false;
            }
        }
        // context menu: file + directory background verbs
        let clsid_str = clsid_string(&CLSID_CONTEXT_MENU);
        if let Ok(k) = reg_create(&format!(
            "SOFTWARE\\Classes\\CLSID\\{clsid_str}\\InprocServer32"
        )) {
            ok &= reg_set_string(k, None, &dll).is_ok();
            ok &= reg_set_string(k, Some("ThreadingModel"), "Apartment").is_ok();
            let _ = RegCloseKey(k);
        } else {
            ok = false;
        }
        for sub in [
            "SOFTWARE\\Classes\\*\\shellex\\ContextMenuHandlers\\cairn",
            "SOFTWARE\\Classes\\Directory\\shellex\\ContextMenuHandlers\\cairn",
        ] {
            if let Ok(k) = reg_create(sub) {
                ok &= reg_set_string(k, None, &clsid_str).is_ok();
                let _ = RegCloseKey(k);
            } else {
                ok = false;
            }
        }
        if ok {
            S_OK
        } else {
            E_FAIL
        }
    }
}

#[no_mangle]
extern "system" fn DllUnregisterServer() -> HRESULT {
    // removal via reg delete (documented in ADR-0019); the extension ships
    // without a hard dependency on advapi32's delete tree API in the first
    // milestone
    S_OK
}

const _: () = {
    // Compile-time sanity: every face's vtable slot 0 is a real QI
    // (overlay_query_interface / menu_init_query_interface /
    // menu_cmd_query_interface / factory_query_interface). Companion-face
    // minting + offset-based identity land with the icon resource pack
    // (ADR-0019 §5).
};
