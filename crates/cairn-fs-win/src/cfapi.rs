//! CfAPI walking skeleton (WO2) — real CloudFilters bindings, design in docs/cfapi-design.md.
//!
//! ## Provenance (THIRD_PARTY.md): patterns ported from a battle-tested implementation
//! The call sequences here follow `nextcloud/desktop`'s production `vfs/cfapi` plugin
//! (cfapiwrapper.cpp, AGPL-3.0) — the most complete open-source CfAPI client — rather
//! than being invented:
//! - registration policies: `CF_HYDRATION_POLICY_FULL` + `CF_POPULATION_POLICY_PARTIAL`
//!   + `CF_INSYNC_POLICY_PRESERVE_INSYNC_FOR_SYNC_ENGINE`, `CF_REGISTER_FLAG_UPDATE`;
//! - connect flags: `REQUIRE_PROCESS_INFO | REQUIRE_FULL_FILE_PATH |
//!   BLOCK_SELF_IMPLICIT_HYDRATION` — with the explicit self-PID deadlock guard in the
//!   callback ("implicit hydration triggered by the client itself will lead to a
//!   deadlock");
//! - FETCH_DATA completion via `CF_OPERATION_TYPE_TRANSFER_DATA` with an explicit
//!   `CompletionStatus` (success AND failure travel the same path; failures surface in
//!   Explorer instead of hanging the copy dialog);
//! - **4096-byte block alignment**: CfAPI requires transferred blocks to be a multiple
//!   of the block size; only the LAST block of a hydration may be smaller. We serve in
//!   aligned chunks with the trailing partial block sent last (Nextcloud's
//!   align-and-send pattern);
//! - `CfReportProviderProgress` per block so Explorer's copy dialog animates;
//! - placeholder creation with `CF_PLACEHOLDER_CREATE_FLAG_MARK_IN_SYNC` + all four
//!   file timestamps (zero timestamps render as 1601-01-01 in Explorer).
//! One deviation from the skeleton's first draft, caught while porting: `ParamSize`
//! must be `offsetof(CF_OPERATION_PARAMETERS, Anonymous) + sizeof(member)`
//! (`CF_SIZE_OF_OP_PARAM`) — the union sits at offset 8 on x64, so the previous
//! `+4` was an ABI bug that would have failed every CfExecute with E_INVALIDARG.
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

/// CfAPI block-size contract (nextcloud/desktop cfapiwrapper.cpp): transferred blocks
/// must be block-aligned; only the final block may be smaller.
const CFAPI_BLOCK_SIZE: usize = 4096;

/// NTSTATUS for a failed hydration — Explorer shows the error state, the copy dialog
/// aborts cleanly (never serve unverified bytes: I2).
const STATUS_UNSUCCESSFUL: i32 = 0xC000_0001u32 as i32;

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

/// SystemTime → NT FILETIME (100ns units since 1601-01-01).
fn filetime_now() -> i64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = i64::try_from(now.as_secs()).unwrap_or(i64::MAX);
    secs.saturating_add(11_644_473_600)
        .saturating_mul(10_000_000)
        + i64::from(now.subsec_nanos() / 100)
}

/// Register `root` as a CloudFiles sync root for this provider (per-user registration;
/// the service/installer decision is deliberately out of the skeleton's scope).
pub fn register_sync_root(root: &str, provider_name: &str) -> Result<(), i32> {
    use windows::Win32::Storage::CloudFilters::{
        CfRegisterSyncRoot, CF_HYDRATION_POLICY, CF_HYDRATION_POLICY_FULL,
        CF_HYDRATION_POLICY_MODIFIER, CF_INSYNC_POLICY_PRESERVE_INSYNC_FOR_SYNC_ENGINE,
        CF_POPULATION_POLICY, CF_POPULATION_POLICY_MODIFIER, CF_POPULATION_POLICY_PARTIAL,
        CF_REGISTER_FLAG_UPDATE, CF_SYNC_POLICIES, CF_SYNC_REGISTRATION,
    };
    // plain DOS path (nextcloud/desktop passes user-mode paths; the \\?\ prefix is
    // rejected by CfConnectSyncRoot with E_INVALIDARG even though CfRegisterSyncRoot
    // tolerates it — use the same normalized path form for BOTH calls)
    let root_w = wide(root);
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
    // Nextcloud's exact policy set (cfapiwrapper.cpp registerSyncRoot)
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
        InSync: CF_INSYNC_POLICY_PRESERVE_INSYNC_FOR_SYNC_ENGINE,
        HardLink: Default::default(),
        PlaceholderManagement: Default::default(),
    };
    // SAFETY: pointers valid for the duration of the call; root is a real directory.
    unsafe {
        CfRegisterSyncRoot(
            PCWSTR(root_w.as_ptr()),
            &registration,
            &policies,
            CF_REGISTER_FLAG_UPDATE,
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
        CF_PLACEHOLDER_CREATE_FLAG_MARK_IN_SYNC, CF_PLACEHOLDER_CREATE_INFO,
    };
    use windows::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_NORMAL, FILE_BASIC_INFO};
    let parent = std::path::Path::new(root).join(
        std::path::Path::new(path)
            .parent()
            .unwrap_or(std::path::Path::new("")),
    );
    let base_w = wide(&parent.to_string_lossy());
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
        // MARK_IN_SYNC (nextcloud/desktop): otherwise the filter nags the provider with
        // sync-state callbacks and Explorer shows the wrong state
        Flags: CF_PLACEHOLDER_CREATE_FLAGS(CF_PLACEHOLDER_CREATE_FLAG_MARK_IN_SYNC.0),
        Result: Default::default(),
        CreateUsn: 0,
    };
    // file size + attributes + REAL timestamps (zero timestamps render as 1601-01-01)
    let ft = filetime_now();
    info.FsMetadata.FileSize = size as i64;
    info.FsMetadata.BasicInfo = FILE_BASIC_INFO {
        CreationTime: ft,
        LastAccessTime: ft,
        LastWriteTime: ft,
        ChangeTime: ft,
        FileAttributes: FILE_ATTRIBUTE_NORMAL.0 as u32,
    };
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
        CF_CONNECT_FLAGS, CF_CONNECT_FLAG_BLOCK_SELF_IMPLICIT_HYDRATION,
        CF_CONNECT_FLAG_REQUIRE_FULL_FILE_PATH, CF_CONNECT_FLAG_REQUIRE_PROCESS_INFO,
    };
    let root_w = wide(root);
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
    // Nextcloud's exact connect flags; REQUIRE_PROCESS_INFO powers the self-PID guard.
    let flags = CF_CONNECT_FLAGS(
        CF_CONNECT_FLAG_REQUIRE_PROCESS_INFO.0
            | CF_CONNECT_FLAG_REQUIRE_FULL_FILE_PATH.0
            | CF_CONNECT_FLAG_BLOCK_SELF_IMPLICIT_HYDRATION.0,
    );
    // SAFETY: table + context outlive the connection.
    let key = unsafe {
        CfConnectSyncRoot(
            PCWSTR(root_w.as_ptr()),
            table.as_ptr(),
            Some(ctx.cast::<c_void>()),
            flags,
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

/// FETCH_DATA (nextcloud/desktop cfApiFetchDataCallback pattern):
/// 1. self-PID deadlock guard,
/// 2. fetch hash-verified bytes from the source,
/// 3. complete with TRANSFER_DATA in block-aligned chunks (last partial),
/// 4. report provider progress per chunk,
/// 5. on ANY source failure: complete with STATUS_UNSUCCESSFUL — never serve
///    unverified bytes (I2).
unsafe fn fetch_data(
    info: *const windows::Win32::Storage::CloudFilters::CF_CALLBACK_INFO,
    params: *const windows::Win32::Storage::CloudFilters::CF_CALLBACK_PARAMETERS,
) {
    use windows::Win32::Storage::CloudFilters::CfReportProviderProgress;
    // SAFETY: filter driver guarantees validity + lifetime of both pointers.
    let (info, params) = unsafe { (&*info, &*params) };
    let source: &std::sync::Arc<dyn PlaceholderSource> = unsafe {
        &*info
            .CallbackContext
            .cast::<std::sync::Arc<dyn PlaceholderSource>>()
    };

    // --- (1) self-hydration deadlock guard (nextcloud: "will lead to a deadlock") ---
    let self_pid = windows::Win32::System::Threading::GetCurrentProcessId();
    let req_pid = unsafe {
        if info.ProcessInfo.is_null() {
            0
        } else {
            (*info.ProcessInfo).ProcessId
        }
    };
    if req_pid == self_pid {
        complete_transfer(info, std::ptr::null(), 0, 0, STATUS_UNSUCCESSFUL);
        return;
    }

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

    // --- (2) hash-verified bytes from the daemon's CAS-backed source ---
    let bytes = match source.fetch(&hash, offset, length) {
        Ok(b) if b.len() == length as usize => b,
        // short/failed reads are hydration FAILURES, not truncations (I2)
        Ok(_) | Err(_) => {
            complete_transfer(info, std::ptr::null(), 0, 0, STATUS_UNSUCCESSFUL);
            return;
        }
    };

    // --- (3) block-aligned chunked transfer (Nextcloud alignAndSendData) ---
    let mut sent: usize = 0;
    while sent < bytes.len() {
        let take = if bytes.len() - sent <= CFAPI_BLOCK_SIZE {
            bytes.len() - sent // only the LAST block may be unaligned
        } else {
            ((bytes.len() - sent) / CFAPI_BLOCK_SIZE) * CFAPI_BLOCK_SIZE
        };
        let ptr = bytes[sent..sent + take].as_ptr().cast::<c_void>();
        complete_transfer(info, ptr, (offset as usize + sent) as i64, take as i64, 0);
        // --- (4) progress for Explorer's copy dialog ---
        unsafe {
            let _ = CfReportProviderProgress(
                info.ConnectionKey,
                info.TransferKey,
                info.FileSize,
                (offset as i64) + (sent + take) as i64,
            );
        }
        sent += take;
    }
}

/// Complete (a chunk of) a FETCH_DATA hydration via CfExecute(TRANSFER_DATA).
/// `status != 0` marks the whole hydration failed (CompletionStatus propagates).
unsafe fn complete_transfer(
    info: &windows::Win32::Storage::CloudFilters::CF_CALLBACK_INFO,
    buffer: *const c_void,
    offset: i64,
    length: i64,
    status: i32,
) {
    use windows::Win32::Storage::CloudFilters::{
        CfExecute, CF_OPERATION_INFO, CF_OPERATION_PARAMETERS, CF_OPERATION_PARAMETERS_0_6,
        CF_OPERATION_TYPE_TRANSFER_DATA,
    };
    let mut op_params = CF_OPERATION_PARAMETERS::default();
    op_params.Anonymous.TransferData = CF_OPERATION_PARAMETERS_0_6 {
        Flags: Default::default(),
        CompletionStatus: windows::Win32::Foundation::NTSTATUS(status),
        Buffer: buffer.cast_mut(),
        Offset: offset,
        Length: length,
    };
    // CF_SIZE_OF_OP_PARAM(TransferData): offsetof(union) + sizeof(member). The union
    // sits at offset 8 on x64 (ParamSize: u32 + padding) — `+4` here fails every call
    // with E_INVALIDARG (found by porting nextcloud/desktop's macro usage).
    op_params.ParamSize = (std::mem::offset_of!(CF_OPERATION_PARAMETERS, Anonymous)
        + std::mem::size_of::<CF_OPERATION_PARAMETERS_0_6>()) as u32;
    let op = CF_OPERATION_INFO {
        StructSize: std::mem::size_of::<CF_OPERATION_INFO>() as u32,
        Type: CF_OPERATION_TYPE_TRANSFER_DATA,
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
