//! CfAPI walking skeleton (WO2) — real CloudFilters bindings, design in docs/cfapi-design.md.
//!
//! Scope is deliberately ONE placeholder round-tripping through the filter driver:
//! register a sync root, create a placeholder carrying the manifest hash as file identity,
//! connect with a FETCH_DATA callback that serves hash-verified bytes from a
//! [`PlaceholderSource`] (the daemon's CAS-backed implementation). No pin policies, no
//! bulk enumeration, no writeback — those are exactly where overclaiming would live.
//!
//! Safety: CfAPI is a raw C API — this module necessarily contains `unsafe`. Every
//! unsafe block touches the documented CF ABI and is annotated with its invariant.
#![allow(unsafe_code)]

use std::ffi::c_void;
use windows_core::PCWSTR;

/// Bytes the filter driver asks us to hydrate. Implementors MUST return exactly
/// `len` bytes from `offset` (hash-verified — see Cas::get) or an error.
pub trait PlaceholderSource: Send + Sync {
    fn fetch(&self, manifest_hash_hex: &str, offset: u64, len: u32) -> Result<Vec<u8>, i32>;
}

/// Convert a Rust str to a null-terminated wide buffer for CF APIs.
fn wide(s: &str) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt as _;
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// Register `root` as a CloudFiles sync root for this provider (per-user registration;
/// the service/installer decision is deliberately out of the skeleton's scope).
pub fn register_sync_root(root: &str, provider_name: &str) -> Result<(), i32> {
    use windows::Win32::Storage::CloudFilters::{
        CfRegisterSyncRoot, CF_HYDRATION_POLICY, CF_HYDRATION_POLICY_FULL,
        CF_HYDRATION_POLICY_MODIFIER, CF_INSYNC_POLICY, CF_POPULATION_POLICY,
        CF_POPULATION_POLICY_MODIFIER, CF_POPULATION_POLICY_PARTIAL, CF_REGISTER_FLAGS,
        CF_SYNC_POLICIES, CF_SYNC_REGISTRATION,
    };
    let root_w = wide(&cairn_core::pathutil::win_long_path(root));
    let name_w = wide(provider_name);
    let version_w = wide(env!("CARGO_PKG_VERSION"));

    // SAFETY: CF_SYNC_REGISTRATION is a flat struct; the PCWSTR/pointer fields point at
    // buffers that outlive the call (CfRegisterSyncRoot copies them).
    let registration = CF_SYNC_REGISTRATION {
        StructSize: std::mem::size_of::<CF_SYNC_REGISTRATION>() as u32,
        ProviderName: PCWSTR(name_w.as_ptr()),
        ProviderVersion: PCWSTR(version_w.as_ptr()),
        SyncRootIdentity: name_w.as_ptr().cast::<c_void>(),
        SyncRootIdentityLength: (name_w.len() as u32) * 2,
        FileIdentity: name_w.as_ptr().cast::<c_void>(),
        FileIdentityLength: (name_w.len() as u32) * 2,
        // provider identity: fixed GUID per install (dev skeleton: one static GUID;
        // production provisions per-tenant)
        ProviderId: windows_core::GUID::from_u128(0xc1a1_0001_0000_0000_0000_0000_0000_0001),
    };
    // hydration FULL on open; population NONE (scan-driven, skeleton scope)
    let policies = CF_SYNC_POLICIES {
        StructSize: std::mem::size_of::<CF_SYNC_POLICIES>() as u32,
        Hydration: CF_HYDRATION_POLICY {
            Primary: CF_HYDRATION_POLICY_FULL,
            Modifier: CF_HYDRATION_POLICY_MODIFIER(0),
        },
        Population: CF_POPULATION_POLICY {
            Primary: CF_POPULATION_POLICY_PARTIAL,
            Modifier: CF_POPULATION_POLICY_MODIFIER(0),
        },
        InSync: CF_INSYNC_POLICY(0),
        HardLink: Default::default(),
        PlaceholderManagement: Default::default(),
    };
    // SAFETY: pointers valid for the duration of the call; root is a real directory.
    unsafe {
        CfRegisterSyncRoot(
            PCWSTR(root_w.as_ptr()),
            &registration,
            &policies,
            CF_REGISTER_FLAGS(0),
        )
        .map_err(|e| e.code().0)
    }
}

/// Create ONE placeholder at `path` (under the registered root) whose file identity is
/// the manifest hash; `size` is the full on-server size.
pub fn create_placeholder(
    root: &str,
    path: &str,
    manifest_hash_hex: &str,
    size: u64,
) -> Result<(), i32> {
    use windows::Win32::Storage::CloudFilters::{
        CfCreatePlaceholders, CF_CREATE_FLAGS, CF_PLACEHOLDER_CREATE_FLAGS,
        CF_PLACEHOLDER_CREATE_INFO,
    };
    let parent = std::path::Path::new(root).join(
        std::path::Path::new(path)
            .parent()
            .unwrap_or(std::path::Path::new("")),
    );
    let base_w = wide(&cairn_core::pathutil::win_long_path(
        &parent.to_string_lossy(),
    ));
    let name = std::path::Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string());
    let name_w = wide(&name);
    let identity_w = wide(manifest_hash_hex);
    // SAFETY: CF_PLACEHOLDER_CREATE_INFO holds pointers into buffers that live across
    // the single CfCreatePlaceholders call (it copies identity + metadata).
    let mut info = CF_PLACEHOLDER_CREATE_INFO {
        RelativeFileName: PCWSTR(name_w.as_ptr()),
        FsMetadata: Default::default(),
        FileIdentity: identity_w.as_ptr().cast::<c_void>(),
        FileIdentityLength: (identity_w.len() as u32) * 2,
        Flags: CF_PLACEHOLDER_CREATE_FLAGS(0),
        Result: Default::default(),
        CreateUsn: 0,
    };
    // file size + attributes live in FsMetadata; zero timestamps are valid
    info.FsMetadata.FileSize = size as i64;
    info.FsMetadata.BasicInfo.FileAttributes = 0x80; // FILE_ATTRIBUTE_NORMAL
                                                     // SAFETY: one info entry; returned-count pointer unused for a single create.
    unsafe {
        CfCreatePlaceholders(
            PCWSTR(base_w.as_ptr()),
            std::slice::from_mut(&mut info),
            CF_CREATE_FLAGS(0),
            None,
        )
        .map_err(|e| e.code().0)
    }
}

/// Connect the registered root; FETCH_DATA serves from `source` for the life of the
/// returned guard (dropping it disconnects the root).
pub fn connect(
    root: &str,
    source: std::sync::Arc<dyn PlaceholderSource>,
) -> Result<Connection, i32> {
    use windows::Win32::Storage::CloudFilters::{
        CfConnectSyncRoot, CF_CALLBACK_REGISTRATION, CF_CALLBACK_TYPE, CF_CALLBACK_TYPE_FETCH_DATA,
    };
    let root_w = wide(&cairn_core::pathutil::win_long_path(root));
    // SAFETY: the context pointer must stay alive for the connection's lifetime — the
    // Arc lives in Connection (below); the callback table is 'static.
    let ctx = Box::into_raw(Box::new(source));
    extern "system" fn on_fetch(
        info: *const windows::Win32::Storage::CloudFilters::CF_CALLBACK_INFO,
        params: *const windows::Win32::Storage::CloudFilters::CF_CALLBACK_PARAMETERS,
    ) {
        // SAFETY: the filter driver guarantees both pointers for the callback duration.
        unsafe { fetch_data(info, params) }
    }
    let table = Box::new([
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_FETCH_DATA,
            Callback: Some(on_fetch),
        },
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE(0), // CF_CALLBACK_END
            Callback: None,
        },
    ]);
    // SAFETY: table + context outlive the connection.
    let key = unsafe {
        CfConnectSyncRoot(
            PCWSTR(root_w.as_ptr()),
            table.as_ptr(),
            Some(ctx.cast::<c_void>()),
            Default::default(),
        )
        .map_err(|e| e.code().0)?
    };
    Ok(Connection {
        _root_wide: root_w,
        _table: table,
        _ctx: ctx,
        key: Some(key),
    })
}

/// Connected root: holds the connection key + the boxed source alive.
pub struct Connection {
    _root_wide: Vec<u16>,
    _table: Box<[windows::Win32::Storage::CloudFilters::CF_CALLBACK_REGISTRATION]>,
    _ctx: *mut std::sync::Arc<dyn PlaceholderSource>,
    key: Option<windows::Win32::Storage::CloudFilters::CF_CONNECTION_KEY>,
}

// SAFETY: CF_CONNECTION_KEY is plain data; the Arc target is Send+Sync.
unsafe impl Send for Connection {}
// SAFETY: CfAPI supports concurrent operations on one connection key.
unsafe impl Sync for Connection {}

impl Drop for Connection {
    fn drop(&mut self) {
        use windows::Win32::Storage::CloudFilters::CfDisconnectSyncRoot;
        // SAFETY: key belongs to the live connection established in connect().
        unsafe {
            if let Some(k) = self.key.take() {
                let _ = CfDisconnectSyncRoot(k);
            }
        }
        // SAFETY: we created this Box::into_raw in connect().
        unsafe {
            drop(Box::from_raw(self._ctx));
        }
    }
}

/// FETCH_DATA: read the file identity (manifest hash) + requested range, fetch verified
/// bytes, complete with RETRIEVE_DATA. On source failure we complete with zero bytes —
/// the filter surfaces the hydration failure to Explorer (never serve unverified bytes).
unsafe fn fetch_data(
    info: *const windows::Win32::Storage::CloudFilters::CF_CALLBACK_INFO,
    params: *const windows::Win32::Storage::CloudFilters::CF_CALLBACK_PARAMETERS,
) {
    use windows::Win32::Storage::CloudFilters::{
        CfExecute, CF_OPERATION_INFO, CF_OPERATION_PARAMETERS, CF_OPERATION_TYPE_RETRIEVE_DATA,
    };
    // SAFETY: filter driver guarantees validity + lifetime of both pointers.
    let (info, params) = unsafe { (&*info, &*params) };
    let source: &std::sync::Arc<dyn PlaceholderSource> = unsafe {
        &*info
            .CallbackContext
            .cast::<std::sync::Arc<dyn PlaceholderSource>>()
    };
    // file identity is the wide string we wrote at placeholder creation
    let hash = unsafe {
        let len = (info.FileIdentityLength / 2) as usize;
        let slice = std::slice::from_raw_parts(info.FileIdentity.cast::<u16>(), len);
        String::from_utf16_lossy(slice)
            .trim_end_matches('\0')
            .to_string()
    };
    let offset =
        u64::try_from(unsafe { params.Anonymous.FetchData.RequiredFileOffset }).unwrap_or(0);
    let length = u32::try_from(unsafe { params.Anonymous.FetchData.RequiredLength }).unwrap_or(0);
    let bytes = source.fetch(&hash, offset, length).unwrap_or_default();
    let mut op_params = CF_OPERATION_PARAMETERS::default();
    op_params.Anonymous.RetrieveData =
        windows::Win32::Storage::CloudFilters::CF_OPERATION_PARAMETERS_0_5 {
            Flags: Default::default(),
            Buffer: bytes.as_ptr().cast::<c_void>().cast_mut(),
            Offset: offset as i64,
            Length: bytes.len() as i64,
            ReturnedLength: bytes.len() as i64,
        };
    op_params.ParamSize = std::mem::size_of::<
        windows::Win32::Storage::CloudFilters::CF_OPERATION_PARAMETERS_0_5,
    >() as u32
        + 4; // ParamSize field itself (CF contract: offset-to-member + member size)
    let op = CF_OPERATION_INFO {
        StructSize: std::mem::size_of::<CF_OPERATION_INFO>() as u32,
        Type: CF_OPERATION_TYPE_RETRIEVE_DATA,
        ConnectionKey: info.ConnectionKey,
        TransferKey: info.TransferKey,
        CorrelationVector: std::ptr::null(),
        SyncStatus: std::ptr::null(),
        RequestKey: info.RequestKey,
    };
    // SAFETY: buffers valid for the call; keys are the filter's own.
    unsafe {
        let _ = CfExecute(&op, &mut op_params);
    }
}
