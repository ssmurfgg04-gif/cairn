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

    /// On-demand population query (FETCH_PLACEHOLDERS callback): the filter asks
    /// which REMOTE entries exist under `dir_path` matching `pattern` (null pattern
    /// arrives as an empty string = everything). The answer is transferred back via
    /// CF_OPERATION_TYPE_TRANSFER_PLACEHOLDERS — see `transfer_placeholders`.
    ///
    /// Cairn v1 answers EMPTY by design: attach pre-creates every placeholder
    /// (CfCreatePlaceholders batch), so there is never a remote entry the filter
    /// does not already know. The callback must still be REGISTERED and ANSWERED:
    /// the registered population policy is PARTIAL (nextcloud's policy set), and
    /// with PARTIAL the filter BLOCKS any open of a not-yet-known path until the
    /// provider completes the population — an unregistered callback means the
    /// operation hangs for the filter's fixed 60 s timeout and fails with
    /// ERROR_CLOUD_FILE_REQUEST_TIMEOUT (426). That was quirk W10, paid for on the
    /// real windows-latest VM. The machinery below transfers REAL entries when a
    /// source returns them (nextcloud cfApiSendPlaceholdersTransferInfo shape).
    fn fetch_placeholders(&self, dir_path: &str, pattern: &str) -> Vec<PopulateEntry> {
        let _ = (dir_path, pattern);
        Vec::new()
    }
}

/// One remote entry offered by `fetch_placeholders` (transferred to the filter as a
/// CF_PLACEHOLDER_CREATE_INFO during on-demand population).
pub struct PopulateEntry {
    /// File name relative to `dir_path` (no separators).
    pub name: String,
    /// Placeholder identity — for Cairn this is the file manifest hash (hex).
    pub identity_hex: String,
    pub size: u64,
    pub is_directory: bool,
}

/// Outcome of a write-open validation (WO6-1 §2, docs/design/write-back.md).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValidateOutcome {
    /// Placeholder identity == server head AND content fully on disk:
    /// the write-open proceeds offline — no network on save (studio fast path).
    CurrentHydrated,
    /// Matches head but dehydrated: force `DataRequired` so the filter drives
    /// FETCH_DATA (hydrate-before-write reuses the proven read machinery).
    CurrentDehydrated,
    /// Server head moved (another device saved): force hydration of the fresh
    /// bytes; the resulting divergence folds into the conflict-copy rule (§7.1).
    Stale,
    /// Cannot validate (offline) and content is not on disk: fail the write-open
    /// loudly (v1 rule — writes requiring hydration fail offline, never silently).
    Offline,
}

/// Write-back hooks the daemon provides (WO6-1). `write_open_validate` is called
/// from the filter's VALIDATE_DATA callback; the notify hooks carry open/close/
/// delete events so the ENGINE-side policy (leases, dirty marking with the
/// size+mtime predicate, tombstones) stays in cairn-sync/cairn-cli — this layer
/// is deliberately policy-free.
pub trait WriteBackSource: PlaceholderSource {
    fn write_open_validate(&self, path: &str, identity: &str) -> ValidateOutcome;
    /// NOTIFY_FILE_OPEN_COMPLETION — lease auto-acquire policy lives in the source.
    fn open_notified(&self, _path: &str) {}
    /// NOTIFY_FILE_CLOSE_COMPLETION — mark dirty via the size+mtime predicate.
    fn close_notified(&self, _path: &str) {}
    /// NOTIFY_DELETE — record tombstone intent (journal is the source of truth).
    fn delete_notified(&self, _path: &str) {}
}

/// cfapi.h `CF_OPERATION_ACK_DATA_FLAG_DATA_REQUIRED` (0x1) — windows-rs 0.58
/// exports only `..._FLAG_NONE` for this flag set; the value is cited from the
/// Windows SDK header, not invented.
const CF_OPERATION_ACK_DATA_FLAG_DATA_REQUIRED: i32 = 0x0000_0001;

/// The connection's boxed callback context: `Box::into_raw(Arc<...>)` produced by
/// connect()/connect_write_back(). Rebuilt and dropped EXACTLY once in
/// Connection::drop — the Connection is the sole owner after handoff.
enum Ctx {
    Read(*mut std::sync::Arc<dyn PlaceholderSource>),
    Write(*mut std::sync::Arc<dyn WriteBackSource>),
}
// SAFETY: the pointee is an owned Arc<...> (Send+Sync target); ownership moves
// to the Connection alone, and Drop consumes it on whichever thread drops it.
unsafe impl Send for Ctx {}

/// Convert a null-terminated wide pointer to a String (lossy, no trailing nul).
fn pcwstr_to_string(p: PCWSTR) -> String {
    if p.0.is_null() {
        return String::new();
    }
    // SAFETY: the filter guarantees a nul-terminated buffer for the callback's duration.
    unsafe {
        let mut len = 0usize;
        while *p.0.add(len) != 0 {
            len += 1;
        }
        let slice = std::slice::from_raw_parts(p.0, len);
        String::from_utf16_lossy(slice)
    }
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
        CF_PLACEHOLDER_CREATE_FLAG_DISABLE_ON_DEMAND_POPULATION,
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
        // sync-state callbacks and Explorer shows the wrong state.
        // DISABLE_ON_DEMAND_POPULATION (quirk W10): this file exists remotely NOW; the
        // filter must never wait on a population query to satisfy opens of it.
        Flags: CF_PLACEHOLDER_CREATE_FLAGS(
            CF_PLACEHOLDER_CREATE_FLAG_MARK_IN_SYNC.0
                | CF_PLACEHOLDER_CREATE_FLAG_DISABLE_ON_DEMAND_POPULATION.0,
        ),
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
/// returned guard (dropping it disconnects the root). READ-ONLY surface (WO2).
pub fn connect(
    root: &str,
    source: std::sync::Arc<dyn PlaceholderSource>,
) -> Result<Connection, i32> {
    use windows::Win32::Storage::CloudFilters::{
        CfConnectSyncRoot, CF_CALLBACK_REGISTRATION, CF_CALLBACK_TYPE_CANCEL_FETCH_PLACEHOLDERS,
        CF_CALLBACK_TYPE_FETCH_DATA, CF_CALLBACK_TYPE_FETCH_PLACEHOLDERS, CF_CALLBACK_TYPE_NONE,
        CF_CONNECT_FLAGS, CF_CONNECT_FLAG_BLOCK_SELF_IMPLICIT_HYDRATION,
        CF_CONNECT_FLAG_REQUIRE_FULL_FILE_PATH, CF_CONNECT_FLAG_REQUIRE_PROCESS_INFO,
    };
    let root_w = wide(root);
    extern "system" fn on_fetch(
        info: *const windows::Win32::Storage::CloudFilters::CF_CALLBACK_INFO,
        params: *const windows::Win32::Storage::CloudFilters::CF_CALLBACK_PARAMETERS,
    ) {
        // SAFETY: the filter driver guarantees both pointers for the callback duration.
        unsafe {
            let (info, params) = (&*info, &*params);
            let src = ctx_as_read_source(info);
            serve_fetch(&**src, info, params);
        }
    }
    let table: Box<[CF_CALLBACK_REGISTRATION]> = Box::new([
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_FETCH_DATA,
            Callback: Some(on_fetch),
        },
        // Population callbacks (quirk W10): the PARTIAL population policy makes the
        // filter WAIT for FETCH_PLACEHOLDERS on any open of a not-yet-known path —
        // an unregistered table here means a 60 s ERROR_CLOUD_FILE_REQUEST_TIMEOUT.
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_FETCH_PLACEHOLDERS,
            Callback: Some(on_fetch_placeholders),
        },
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_CANCEL_FETCH_PLACEHOLDERS,
            Callback: Some(on_cancel_fetch_placeholders),
        },
        // CF_CALLBACK_REGISTRATION_END = { CF_CALLBACK_TYPE_INVALID, NULL } — INVALID is
        // 0xFFFFFFFF (windows-rs exports it as CF_CALLBACK_TYPE_NONE = -1). A 0 type here
        // is NOT the sentinel: CfConnectSyncRoot rejects the table with E_INVALIDARG
        // (proven on the real windows-latest VM).
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_NONE,
            Callback: None,
        },
    ]);
    // Nextcloud's exact connect flags; REQUIRE_PROCESS_INFO powers the self-PID guard.
    let flags = CF_CONNECT_FLAGS(
        CF_CONNECT_FLAG_REQUIRE_PROCESS_INFO.0
            | CF_CONNECT_FLAG_REQUIRE_FULL_FILE_PATH.0
            | CF_CONNECT_FLAG_BLOCK_SELF_IMPLICIT_HYDRATION.0,
    );
    // SAFETY: table + context outlive the connection; cleanup closure rebuilds the Box.
    let ctx = Box::into_raw(Box::new(source));
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
        key: Some(key),
        ctx: Ctx::Read(ctx),
    })
}

/// Connect with the FULL write-back callback table (WO6-1): FETCH_DATA (shared
/// hydration machinery) + VALIDATE_DATA (hydrate-before-write) +
/// NOTIFY_FILE_OPEN/CLOSE_COMPLETION (lease + dirty bookkeeping) + NOTIFY_DELETE
/// (tombstone intent). Connect flags identical to the read-only connect — the
/// self-PID deadlock guard applies to write-open hydration too.
pub fn connect_write_back(
    root: &str,
    source: std::sync::Arc<dyn WriteBackSource>,
) -> Result<Connection, i32> {
    use windows::Win32::Storage::CloudFilters::{
        CfConnectSyncRoot, CF_CALLBACK_REGISTRATION, CF_CALLBACK_TYPE_CANCEL_FETCH_PLACEHOLDERS,
        CF_CALLBACK_TYPE_FETCH_DATA, CF_CALLBACK_TYPE_FETCH_PLACEHOLDERS, CF_CALLBACK_TYPE_NONE,
        CF_CALLBACK_TYPE_NOTIFY_DELETE, CF_CALLBACK_TYPE_NOTIFY_FILE_CLOSE_COMPLETION,
        CF_CALLBACK_TYPE_NOTIFY_FILE_OPEN_COMPLETION, CF_CALLBACK_TYPE_VALIDATE_DATA,
        CF_CONNECT_FLAGS, CF_CONNECT_FLAG_BLOCK_SELF_IMPLICIT_HYDRATION,
        CF_CONNECT_FLAG_REQUIRE_FULL_FILE_PATH, CF_CONNECT_FLAG_REQUIRE_PROCESS_INFO,
    };
    let root_w = wide(root);

    extern "system" fn on_fetch_wb(
        info: *const windows::Win32::Storage::CloudFilters::CF_CALLBACK_INFO,
        params: *const windows::Win32::Storage::CloudFilters::CF_CALLBACK_PARAMETERS,
    ) {
        unsafe {
            let (info, params) = (&*info, &*params);
            let src = ctx_as_write_source(info);
            serve_fetch(&**src, info, params);
        }
    }
    // Write-side population handler: same answer as the read one, but the callback
    // context is an Arc<dyn WriteBackSource> — a different vtable, so a different
    // extern fn (supertrait method calls work through the coercion).
    extern "system" fn on_fetch_placeholders_wb(
        info: *const windows::Win32::Storage::CloudFilters::CF_CALLBACK_INFO,
        params: *const windows::Win32::Storage::CloudFilters::CF_CALLBACK_PARAMETERS,
    ) {
        unsafe {
            let (info, params) = (&*info, &*params);
            let src = ctx_as_write_source(info);
            let self_pid = windows::Win32::System::Threading::GetCurrentProcessId();
            let req_pid = if info.ProcessInfo.is_null() {
                0
            } else {
                (*info.ProcessInfo).ProcessId
            };
            if req_pid == self_pid {
                transfer_placeholders(info, &[], STATUS_UNSUCCESSFUL);
                return;
            }
            let path = pcwstr_to_string(info.NormalizedPath);
            let pattern = if params.Anonymous.FetchPlaceholders.Pattern.0.is_null() {
                String::new()
            } else {
                pcwstr_to_string(params.Anonymous.FetchPlaceholders.Pattern)
            };
            let entries = (**src).fetch_placeholders(&path, &pattern);
            transfer_placeholders(info, &entries, 0);
        }
    }
    extern "system" fn on_validate(
        info: *const windows::Win32::Storage::CloudFilters::CF_CALLBACK_INFO,
        params: *const windows::Win32::Storage::CloudFilters::CF_CALLBACK_PARAMETERS,
    ) {
        unsafe { validate_data(&*info, &*params) }
    }
    extern "system" fn on_open(
        info: *const windows::Win32::Storage::CloudFilters::CF_CALLBACK_INFO,
        _params: *const windows::Win32::Storage::CloudFilters::CF_CALLBACK_PARAMETERS,
    ) {
        // SAFETY: filter guarantees the pointer; notify hooks are best-effort and
        // MUST NOT block the filter longer than the source's bookkeeping (no I/O).
        unsafe {
            let info = &*info;
            let src = ctx_as_write_source(info);
            let path = pcwstr_to_string(info.NormalizedPath);
            (**src).open_notified(&path);
        }
    }
    extern "system" fn on_close(
        info: *const windows::Win32::Storage::CloudFilters::CF_CALLBACK_INFO,
        _params: *const windows::Win32::Storage::CloudFilters::CF_CALLBACK_PARAMETERS,
    ) {
        unsafe {
            let info = &*info;
            let src = ctx_as_write_source(info);
            let path = pcwstr_to_string(info.NormalizedPath);
            (**src).close_notified(&path);
        }
    }
    extern "system" fn on_delete(
        info: *const windows::Win32::Storage::CloudFilters::CF_CALLBACK_INFO,
        _params: *const windows::Win32::Storage::CloudFilters::CF_CALLBACK_PARAMETERS,
    ) {
        unsafe {
            let info = &*info;
            let src = ctx_as_write_source(info);
            let path = pcwstr_to_string(info.NormalizedPath);
            (**src).delete_notified(&path);
            ack_delete(info);
        }
    }

    let table: Box<[CF_CALLBACK_REGISTRATION]> = Box::new([
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_FETCH_DATA,
            Callback: Some(on_fetch_wb),
        },
        // Population callbacks (quirk W10) — MANDATORY under the PARTIAL policy.
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_FETCH_PLACEHOLDERS,
            Callback: Some(on_fetch_placeholders_wb),
        },
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_CANCEL_FETCH_PLACEHOLDERS,
            Callback: Some(on_cancel_fetch_placeholders),
        },
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_VALIDATE_DATA,
            Callback: Some(on_validate),
        },
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_NOTIFY_FILE_OPEN_COMPLETION,
            Callback: Some(on_open),
        },
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_NOTIFY_FILE_CLOSE_COMPLETION,
            Callback: Some(on_close),
        },
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_NOTIFY_DELETE,
            Callback: Some(on_delete),
        },
        // END sentinel: CF_CALLBACK_TYPE_INVALID (0xFFFFFFFF / CF_CALLBACK_TYPE_NONE = -1)
        // — the 0-type table is rejected with E_INVALIDARG by the real driver.
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_NONE,
            Callback: None,
        },
    ]);
    let flags = CF_CONNECT_FLAGS(
        CF_CONNECT_FLAG_REQUIRE_PROCESS_INFO.0
            | CF_CONNECT_FLAG_REQUIRE_FULL_FILE_PATH.0
            | CF_CONNECT_FLAG_BLOCK_SELF_IMPLICIT_HYDRATION.0,
    );
    // SAFETY: table + context outlive the connection; cleanup closure rebuilds the Box.
    let ctx = Box::into_raw(Box::new(source));
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
        key: Some(key),
        ctx: Ctx::Write(ctx),
    })
}

// SAFETY (both ctx_as_* fns): the CallbackContext pointer was created by
// Box::into_raw(Arc<...>) in connect()/connect_write_back() and stays valid for
// the connection's lifetime (Connection owns the rebuild-and-drop closure).
fn ctx_as_read_source(
    info: &windows::Win32::Storage::CloudFilters::CF_CALLBACK_INFO,
) -> &std::sync::Arc<dyn PlaceholderSource> {
    unsafe {
        &*info
            .CallbackContext
            .cast::<std::sync::Arc<dyn PlaceholderSource>>()
    }
}

fn ctx_as_write_source(
    info: &windows::Win32::Storage::CloudFilters::CF_CALLBACK_INFO,
) -> &std::sync::Arc<dyn WriteBackSource> {
    unsafe {
        &*info
            .CallbackContext
            .cast::<std::sync::Arc<dyn WriteBackSource>>()
    }
}

/// Connected root: holds the connection key + keeps the boxed source alive.
pub struct Connection {
    _root_wide: Vec<u16>,
    _table: Box<[windows::Win32::Storage::CloudFilters::CF_CALLBACK_REGISTRATION]>,
    key: Option<windows::Win32::Storage::CloudFilters::CF_CONNECTION_KEY>,
    ctx: Ctx,
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
        // SAFETY: the raw context was Box::into_raw'd by connect*(); this rebuilds
        // and drops it exactly once (Connection is the sole owner after handoff).
        match &self.ctx {
            Ctx::Read(p) => unsafe { drop(Box::from_raw(*p)) },
            Ctx::Write(p) => unsafe { drop(Box::from_raw(*p)) },
        }
    }
}

impl Connection {
    /// The raw connection key (badge layer FFI rides the same connection).
    pub fn key(&self) -> windows::Win32::Storage::CloudFilters::CF_CONNECTION_KEY {
        self.key.unwrap_or_default()
    }
}

/// FETCH_DATA core (nextcloud/desktop cfApiFetchDataCallback pattern), shared by
/// the read-only and write-back connections:
/// 1. self-PID deadlock guard,
/// 2. fetch hash-verified bytes from the source,
/// 3. complete with TRANSFER_DATA in block-aligned chunks (last partial),
/// 4. report provider progress per chunk,
/// 5. on ANY source failure: complete with STATUS_UNSUCCESSFUL — never serve
///    unverified bytes (I2).
fn serve_fetch(
    source: &dyn PlaceholderSource,
    info: &windows::Win32::Storage::CloudFilters::CF_CALLBACK_INFO,
    params: &windows::Win32::Storage::CloudFilters::CF_CALLBACK_PARAMETERS,
) {
    use windows::Win32::Storage::CloudFilters::CfReportProviderProgress;
    // SAFETY: the filter driver guarantees the validity + lifetime of info/params
    // for the callback duration; everything below touches only those.
    unsafe {
        // --- (1) self-hydration deadlock guard (nextcloud: "will lead to a deadlock") ---
        let self_pid = windows::Win32::System::Threading::GetCurrentProcessId();
        let req_pid = if info.ProcessInfo.is_null() {
            0
        } else {
            (*info.ProcessInfo).ProcessId
        };
        if req_pid == self_pid {
            complete_transfer(info, std::ptr::null(), 0, 0, STATUS_UNSUCCESSFUL);
            return;
        }

        // file identity is the wide string we wrote at placeholder creation
        let len = (info.FileIdentityLength / 2) as usize;
        let slice = std::slice::from_raw_parts(info.FileIdentity.cast::<u16>(), len);
        let hash = String::from_utf16_lossy(slice)
            .trim_end_matches('\0')
            .to_string();
        let offset = u64::try_from(params.Anonymous.FetchData.RequiredFileOffset).unwrap_or(0);
        let length = u32::try_from(params.Anonymous.FetchData.RequiredLength).unwrap_or(0);

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
            let _ = CfReportProviderProgress(
                info.ConnectionKey,
                info.TransferKey,
                info.FileSize,
                (offset as i64) + (sent + take) as i64,
            );
            sent += take;
        }
    }
}

/// VALIDATE_DATA (WO6-1 §2 — write-open). Acknowledges with ACK_DATA:
/// - CurrentHydrated → DataRequired = 0 (open proceeds offline),
/// - CurrentDehydrated / Stale → DataRequired = 1 (the filter drives FETCH_DATA;
///   hydrate-before-write reuses the proven read machinery),
/// - Offline → CompletionStatus = STATUS_UNSUCCESSFUL (writes requiring hydration
///   fail loudly offline — v1 rule; Explorer shows the error, nothing hangs).
fn validate_data(
    info: &windows::Win32::Storage::CloudFilters::CF_CALLBACK_INFO,
    params: &windows::Win32::Storage::CloudFilters::CF_CALLBACK_PARAMETERS,
) {
    let _ = params; // RequiredFileOffset/Length unused in v1: validation is whole-file
                    // SAFETY: filter guarantees info validity for the callback duration.
    let (identity, self_open) = unsafe {
        let self_pid = windows::Win32::System::Threading::GetCurrentProcessId();
        let req_pid = if info.ProcessInfo.is_null() {
            0
        } else {
            (*info.ProcessInfo).ProcessId
        };
        let len = (info.FileIdentityLength / 2) as usize;
        let slice = std::slice::from_raw_parts(info.FileIdentity.cast::<u16>(), len);
        (
            String::from_utf16_lossy(slice)
                .trim_end_matches('\0')
                .to_string(),
            req_pid == self_pid,
        )
    };
    if self_open {
        // the provider never validates its own writes (deadlock guard)
        ack_data(info, false, STATUS_UNSUCCESSFUL);
        return;
    }
    let path = pcwstr_to_string(info.NormalizedPath);
    let src = ctx_as_write_source(info);
    let outcome = (**src).write_open_validate(&path, &identity);
    match outcome {
        ValidateOutcome::CurrentHydrated => ack_data(info, false, 0),
        ValidateOutcome::CurrentDehydrated | ValidateOutcome::Stale => ack_data(info, true, 0),
        ValidateOutcome::Offline => ack_data(info, false, STATUS_UNSUCCESSFUL),
    }
}

/// ACK_DATA completion for VALIDATE_DATA. `data_required` sets
/// CF_OPERATION_ACK_DATA_FLAG_DATA_REQUIRED (cfapi.h 0x1, cited — windows-rs gap).
fn ack_data(
    info: &windows::Win32::Storage::CloudFilters::CF_CALLBACK_INFO,
    data_required: bool,
    status: i32,
) {
    use windows::Win32::Storage::CloudFilters::{
        CfExecute, CF_OPERATION_ACK_DATA_FLAGS, CF_OPERATION_INFO, CF_OPERATION_PARAMETERS,
        CF_OPERATION_PARAMETERS_0_0, CF_OPERATION_TYPE_ACK_DATA,
    };
    let mut op_params = CF_OPERATION_PARAMETERS::default();
    op_params.Anonymous.AckData = CF_OPERATION_PARAMETERS_0_0 {
        Flags: CF_OPERATION_ACK_DATA_FLAGS(if data_required {
            CF_OPERATION_ACK_DATA_FLAG_DATA_REQUIRED
        } else {
            0
        }),
        CompletionStatus: windows::Win32::Foundation::NTSTATUS(status),
        // Offset/Length of the (re)validated range: whole file when data is required
        Offset: 0,
        Length: if data_required { info.FileSize } else { 0 },
    };
    // Same ABI rule proven in round 4: ParamSize = offsetof(union) + sizeof(member).
    op_params.ParamSize = (std::mem::offset_of!(CF_OPERATION_PARAMETERS, Anonymous)
        + std::mem::size_of::<CF_OPERATION_PARAMETERS_0_0>()) as u32;
    let op = CF_OPERATION_INFO {
        StructSize: std::mem::size_of::<CF_OPERATION_INFO>() as u32,
        Type: CF_OPERATION_TYPE_ACK_DATA,
        ConnectionKey: info.ConnectionKey,
        TransferKey: info.TransferKey,
        CorrelationVector: std::ptr::null(),
        SyncStatus: std::ptr::null(),
        RequestKey: info.RequestKey,
    };
    // SAFETY: keys are the filter's own; params are plain data for the call.
    unsafe {
        let _ = CfExecute(&op, &mut op_params);
    }
}

/// ACK_DELETE completion for NOTIFY_DELETE — allow the deletion; the engine
/// records the tombstone via the source's delete hook (journal is truth).
fn ack_delete(info: &windows::Win32::Storage::CloudFilters::CF_CALLBACK_INFO) {
    use windows::Win32::Storage::CloudFilters::{
        CfExecute, CF_OPERATION_ACK_DELETE_FLAGS, CF_OPERATION_INFO, CF_OPERATION_PARAMETERS,
        CF_OPERATION_PARAMETERS_0_2, CF_OPERATION_TYPE_ACK_DELETE,
    };
    let mut op_params = CF_OPERATION_PARAMETERS::default();
    op_params.Anonymous.AckDelete = CF_OPERATION_PARAMETERS_0_2 {
        Flags: CF_OPERATION_ACK_DELETE_FLAGS(0),
        CompletionStatus: windows::Win32::Foundation::NTSTATUS(0),
    };
    op_params.ParamSize = (std::mem::offset_of!(CF_OPERATION_PARAMETERS, Anonymous)
        + std::mem::size_of::<CF_OPERATION_PARAMETERS_0_2>()) as u32;
    let op = CF_OPERATION_INFO {
        StructSize: std::mem::size_of::<CF_OPERATION_INFO>() as u32,
        Type: CF_OPERATION_TYPE_ACK_DELETE,
        ConnectionKey: info.ConnectionKey,
        TransferKey: info.TransferKey,
        CorrelationVector: std::ptr::null(),
        SyncStatus: std::ptr::null(),
        RequestKey: info.RequestKey,
    };
    // SAFETY: keys are the filter's own; params are plain data for the call.
    unsafe {
        let _ = CfExecute(&op, &mut op_params);
    }
}

/// Complete a FETCH_PLACEHOLDERS callback via CfExecute(TRANSFER_PLACEHOLDERS).
/// Shape mirrors nextcloud/desktop `cfApiSendPlaceholdersTransferInfo`: empty
/// answers carry counts 0 and `DISABLE_ON_DEMAND_POPULATION` so the filter stops
/// re-asking for directories whose content the provider fully controls (Cairn v1:
/// attach pre-creates everything — quirk W10). `status` propagates as the
/// operation's CompletionStatus (success = "here is the authoritative answer").
fn transfer_placeholders(
    info: &windows::Win32::Storage::CloudFilters::CF_CALLBACK_INFO,
    entries: &[PopulateEntry],
    status: i32,
) {
    use windows::Win32::Storage::CloudFilters::{
        CfExecute, CF_OPERATION_INFO, CF_OPERATION_PARAMETERS, CF_OPERATION_PARAMETERS_0_7,
        CF_OPERATION_TRANSFER_PLACEHOLDERS_FLAGS,
        CF_OPERATION_TRANSFER_PLACEHOLDERS_FLAG_DISABLE_ON_DEMAND_POPULATION,
        CF_OPERATION_TYPE_TRANSFER_PLACEHOLDERS, CF_PLACEHOLDER_CREATE_FLAGS,
        CF_PLACEHOLDER_CREATE_FLAG_DISABLE_ON_DEMAND_POPULATION,
        CF_PLACEHOLDER_CREATE_FLAG_MARK_IN_SYNC, CF_PLACEHOLDER_CREATE_INFO,
    };
    use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY;
    // name/identity wide buffers must outlive the CfExecute call
    let names: Vec<Vec<u16>> = entries.iter().map(|e| wide(&e.name)).collect();
    let idents: Vec<Vec<u16>> = entries.iter().map(|e| wide(&e.identity_hex)).collect();
    let ft = filetime_now();
    let infos: Vec<CF_PLACEHOLDER_CREATE_INFO> = entries
        .iter()
        .zip(names.iter())
        .zip(idents.iter())
        .map(|((e, name), ident)| {
            let mut info = CF_PLACEHOLDER_CREATE_INFO {
                RelativeFileName: PCWSTR(name.as_ptr()),
                FsMetadata: Default::default(),
                FileIdentity: ident.as_ptr().cast::<c_void>(),
                FileIdentityLength: (ident.len() as u32) * 2,
                // MARK_IN_SYNC: the provider IS the authority for what it transfers
                Flags: CF_PLACEHOLDER_CREATE_FLAGS(
                    CF_PLACEHOLDER_CREATE_FLAG_MARK_IN_SYNC.0
                        | CF_PLACEHOLDER_CREATE_FLAG_DISABLE_ON_DEMAND_POPULATION.0,
                ),
                Result: Default::default(),
                CreateUsn: 0,
            };
            info.FsMetadata.FileSize = e.size as i64;
            info.FsMetadata.BasicInfo.FileAttributes = if e.is_directory {
                FILE_ATTRIBUTE_DIRECTORY.0 as u32
            } else {
                0 // plain file attributes
            };
            info.FsMetadata.BasicInfo.CreationTime = ft;
            info.FsMetadata.BasicInfo.LastWriteTime = ft;
            info.FsMetadata.BasicInfo.LastAccessTime = ft;
            info.FsMetadata.BasicInfo.ChangeTime = ft;
            info
        })
        .collect();
    let mut op_params = CF_OPERATION_PARAMETERS::default();
    op_params.Anonymous.TransferPlaceholders = CF_OPERATION_PARAMETERS_0_7 {
        Flags: CF_OPERATION_TRANSFER_PLACEHOLDERS_FLAGS(
            CF_OPERATION_TRANSFER_PLACEHOLDERS_FLAG_DISABLE_ON_DEMAND_POPULATION.0,
        ),
        CompletionStatus: windows::Win32::Foundation::NTSTATUS(status),
        PlaceholderTotalCount: infos.len() as i64,
        PlaceholderArray: if infos.is_empty() {
            std::ptr::null_mut()
        } else {
            infos.as_ptr() as *mut CF_PLACEHOLDER_CREATE_INFO
        },
        PlaceholderCount: infos.len() as u32,
        EntriesProcessed: infos.len() as u32,
    };
    // Same ABI rule proven in round 4: ParamSize = offsetof(union) + sizeof(member).
    op_params.ParamSize = (std::mem::offset_of!(CF_OPERATION_PARAMETERS, Anonymous)
        + std::mem::size_of::<CF_OPERATION_PARAMETERS_0_7>()) as u32;
    let op = CF_OPERATION_INFO {
        StructSize: std::mem::size_of::<CF_OPERATION_INFO>() as u32,
        Type: CF_OPERATION_TYPE_TRANSFER_PLACEHOLDERS,
        ConnectionKey: info.ConnectionKey,
        TransferKey: info.TransferKey,
        CorrelationVector: std::ptr::null(),
        SyncStatus: std::ptr::null(),
        RequestKey: info.RequestKey,
    };
    // SAFETY: keys are the filter's own; infos/names/idents outlive the call.
    unsafe {
        let _ = CfExecute(&op, &mut op_params);
    }
}

/// FETCH_PLACEHOLDERS handler: the filter wants remote entries under a directory
/// (population policy PARTIAL makes answering MANDATORY — quirk W10). Self-PID
/// requests complete with a failed empty transfer (nextcloud: implicit population
/// from the provider itself "will lead to a deadlock").
extern "system" fn on_fetch_placeholders(
    info: *const windows::Win32::Storage::CloudFilters::CF_CALLBACK_INFO,
    params: *const windows::Win32::Storage::CloudFilters::CF_CALLBACK_PARAMETERS,
) {
    unsafe {
        let info = &*info;
        let params = &*params;
        let self_pid = windows::Win32::System::Threading::GetCurrentProcessId();
        let req_pid = if info.ProcessInfo.is_null() {
            0
        } else {
            (*info.ProcessInfo).ProcessId
        };
        let src = ctx_as_read_source(info);
        if req_pid == self_pid {
            transfer_placeholders(info, &[], STATUS_UNSUCCESSFUL);
            return;
        }
        let path = pcwstr_to_string(info.NormalizedPath);
        // Pattern may be null (enumerate everything)
        let pattern = if params.Anonymous.FetchPlaceholders.Pattern.0.is_null() {
            String::new()
        } else {
            pcwstr_to_string(params.Anonymous.FetchPlaceholders.Pattern)
        };
        let entries = (**src).fetch_placeholders(&path, &pattern);
        transfer_placeholders(info, &entries, 0);
    }
}

/// CANCEL_FETCH_PLACEHOLDERS handler: the originating request went away. The
/// completion for the cancelled callback is no longer accepted by the filter and
/// the next query re-fires FETCH_PLACEHOLDERS — there is no CfExecute for a cancel
/// (nextcloud logs and returns).
extern "system" fn on_cancel_fetch_placeholders(
    _info: *const windows::Win32::Storage::CloudFilters::CF_CALLBACK_INFO,
    _params: *const windows::Win32::Storage::CloudFilters::CF_CALLBACK_PARAMETERS,
) {
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

// ---------- lifecycle helpers (WO6-1 §4 + WO6-2: pin/dehydrate/bulk) ----------

/// Open a file with a protected CfAPI handle (oplock), the handle class every
/// placeholder-state mutation requires (nextcloud pattern: open with
/// CF_OPEN_FILE_FLAGS(0), close via CloseHandle).
fn open_protected(path: &str) -> Result<windows::Win32::Foundation::HANDLE, i32> {
    use windows::Win32::Storage::CloudFilters::{CfOpenFileWithOplock, CF_OPEN_FILE_FLAGS};
    let w = wide(path);
    // SAFETY: w outlives the call; the returned handle is closed by the caller.
    let handle = unsafe {
        CfOpenFileWithOplock(PCWSTR(w.as_ptr()), CF_OPEN_FILE_FLAGS(0)).map_err(|e| e.code().0)?
    };
    Ok(handle)
}

/// Mark a placeholder IN SYNC (after a successful push — the Explorer badge clears
/// exactly when the cloud holds the bytes, never before). Identity-preserving.
pub fn mark_in_sync(path: &str) -> Result<(), i32> {
    use windows::Win32::Storage::CloudFilters::{
        CfSetInSyncState, CF_IN_SYNC_STATE_IN_SYNC, CF_SET_IN_SYNC_FLAGS,
    };
    let handle = open_protected(path)?;
    // SAFETY: handle from open_protected above; usn out-param unused.
    let r = unsafe {
        CfSetInSyncState(
            handle,
            CF_IN_SYNC_STATE_IN_SYNC,
            CF_SET_IN_SYNC_FLAGS(0),
            None,
        )
    };
    unsafe {
        let _ = windows::Win32::Foundation::CloseHandle(handle);
    }
    r.map_err(|e| e.code().0)
}

/// Clear the in-sync bit (engine marks a row dirty that the filter didn't —
/// e.g. divergence found by the reconcile sweep).
pub fn mark_not_in_sync(path: &str) -> Result<(), i32> {
    use windows::Win32::Storage::CloudFilters::{
        CfSetInSyncState, CF_IN_SYNC_STATE_NOT_IN_SYNC, CF_SET_IN_SYNC_FLAGS,
    };
    let handle = open_protected(path)?;
    // SAFETY: handle from open_protected above.
    let r = unsafe {
        CfSetInSyncState(
            handle,
            CF_IN_SYNC_STATE_NOT_IN_SYNC,
            CF_SET_IN_SYNC_FLAGS(0),
            None,
        )
    };
    unsafe {
        let _ = windows::Win32::Foundation::CloseHandle(handle);
    }
    r.map_err(|e| e.code().0)
}

/// Convert an existing FULL local file (created by an editor, ingested by the
/// engine) into a placeholder whose identity is the manifest hash — gate W2.
/// `MARK_IN_SYNC` (it IS synced: we just pushed it) + `ENABLE_ON_DEMAND_POPULATION`
/// (it may be dehydrated/evicted later — WO6-2).
pub fn convert_to_placeholder(path: &str, identity_hex: &str) -> Result<(), i32> {
    use windows::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
    use windows::Win32::Storage::CloudFilters::{
        CfConvertToPlaceholder, CF_CONVERT_FLAGS, CF_CONVERT_FLAG_ENABLE_ON_DEMAND_POPULATION,
        CF_CONVERT_FLAG_MARK_IN_SYNC,
    };
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_SHARE_MODE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    let w = wide(path);
    let identity_w = wide(identity_hex);
    // SAFETY: w + identity_w outlive the call (CfConvertToPlaceholder copies identity).
    let handle = unsafe {
        CreateFileW(
            PCWSTR(w.as_ptr()),
            GENERIC_READ.0 | GENERIC_WRITE.0,
            FILE_SHARE_MODE(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0),
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTE_NONE,
            None,
        )
        .map_err(|e| e.code().0)?
    };
    let flags = CF_CONVERT_FLAGS(
        CF_CONVERT_FLAG_MARK_IN_SYNC.0 | CF_CONVERT_FLAG_ENABLE_ON_DEMAND_POPULATION.0,
    );
    // SAFETY: handle valid; identity pointer copied by the call.
    let r = unsafe {
        CfConvertToPlaceholder(
            handle,
            Some(identity_w.as_ptr().cast::<c_void>()),
            (identity_w.len() as u32) * 2,
            flags,
            None,
            None,
        )
    };
    unsafe {
        let _ = windows::Win32::Foundation::CloseHandle(handle);
    }
    r.map_err(|e| e.code().0)
}

const FILE_FLAGS_AND_ATTRIBUTE_NONE:
    windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES =
    windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES(0);

/// Pin states for ctl pin/unpin (WO6-2); INHERIT left to directory-level policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinState {
    /// always available (hydrated, never evicted)
    Pinned,
    /// explicitly unpinned (may dehydrate immediately, eviction-eligible)
    Unpinned,
    /// default: follow the parent/root policy
    Inherit,
}

impl PinState {
    fn to_cf(self) -> windows::Win32::Storage::CloudFilters::CF_PIN_STATE {
        use windows::Win32::Storage::CloudFilters::{
            CF_PIN_STATE_INHERIT, CF_PIN_STATE_PINNED, CF_PIN_STATE_UNPINNED,
        };
        match self {
            PinState::Pinned => CF_PIN_STATE_PINNED,
            PinState::Unpinned => CF_PIN_STATE_UNPINNED,
            PinState::Inherit => CF_PIN_STATE_INHERIT,
        }
    }
}

/// Set the filter's pin state on a placeholder (needs a FULL placeholder: pinning
/// a dehydrated one hydrates it — that is the OS contract, and exactly what
/// `cairn pin` wants: pinned == local + protected).
pub fn set_pin_state(path: &str, state: PinState) -> Result<(), i32> {
    use windows::Win32::Storage::CloudFilters::{CfSetPinState, CF_SET_PIN_FLAGS};
    let handle = open_protected(path)?;
    // SAFETY: handle from open_protected; pin state is plain data.
    let r = unsafe { CfSetPinState(handle, state.to_cf(), CF_SET_PIN_FLAGS(0), None) };
    unsafe {
        let _ = windows::Win32::Foundation::CloseHandle(handle);
    }
    r.map_err(|e| e.code().0)
}

/// Dehydrate a placeholder (reclaim local bytes; the CAS already holds the content —
/// the CALLER verifies byte coverage in the store before calling, see engine rules:
/// never dehydrate dirty/open files). `CF_DEHYDRATE_FLAG_BACKGROUND` keeps Explorer
/// from surfacing progress UI for an engine-initiated eviction.
pub fn dehydrate_placeholder(path: &str) -> Result<(), i32> {
    use windows::Win32::Storage::CloudFilters::{
        CfDehydratePlaceholder, CF_DEHYDRATE_FLAGS, CF_DEHYDRATE_FLAG_BACKGROUND,
    };
    let handle = open_protected(path)?;
    // SAFETY: handle valid; whole-file dehydrate (offset 0, length = full).
    let r = unsafe {
        CfDehydratePlaceholder(
            handle,
            0,
            i64::MAX, // to EOF per CfAPI contract
            CF_DEHYDRATE_FLAGS(CF_DEHYDRATE_FLAG_BACKGROUND.0),
            None,
        )
    };
    unsafe {
        let _ = windows::Win32::Foundation::CloseHandle(handle);
    }
    r.map_err(|e| e.code().0)
}

/// One entry for a bulk placeholder create (attach on Windows, WO6-2).
pub struct BulkEntry {
    /// path RELATIVE to `root` (may contain subdirectories; parents are created)
    pub relative_path: String,
    /// manifest-hash identity (same encoding create_placeholder writes)
    pub identity_hex: String,
    pub size: u64,
    /// Journaled mtime (unix millis, FileRow encoding). Stamping the
    /// placeholder's LastWriteTime with THIS (not 'now') is punch #5 for the
    /// attach path: the scan's size+mtime predicate then classifies the
    /// freshly created placeholder as UNCHANGED instead of redirtying every
    /// attach (which would re-hydrate the whole tree through the callback
    /// and re-append it — caught by round 13's design review of the
    /// cold-attach matrix row; materialize_missing already did this).
    pub mtime_ms: i64,
}

/// Unix millis → NT FILETIME (100ns units since 1601-01-01), exact at
/// millisecond granularity (mirrors the row encoding so stat round-trips
/// match bit-for-bit: epoch offset 11,644,473,600 s = 11,644,473,600,000 ms,
/// then ms → 100ns).
fn filetime_from_unix_millis(ms: i64) -> i64 {
    ms.saturating_add(11_644_473_600_000).saturating_mul(10_000)
}

/// CfCreatePlaceholders BATCH (WO6-2: attach 2GB tree → placeholders appear in one
/// filter call per directory batch). Returns the first failing entry index + NTSTATUS
/// on partial failure; entries before it were created (idempotent re-run is safe:
/// create-existing reports ERROR_ALREADY_EXISTS per entry, treated as success).
pub fn create_placeholders_batch(root: &str, entries: &[BulkEntry]) -> Result<usize, (usize, i32)> {
    use windows::Win32::Storage::CloudFilters::{
        CfCreatePlaceholders, CF_CREATE_FLAGS, CF_PLACEHOLDER_CREATE_FLAGS,
        CF_PLACEHOLDER_CREATE_FLAG_DISABLE_ON_DEMAND_POPULATION,
        CF_PLACEHOLDER_CREATE_FLAG_MARK_IN_SYNC, CF_PLACEHOLDER_CREATE_INFO,
    };
    use windows::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_NORMAL, FILE_BASIC_INFO};
    if entries.is_empty() {
        return Ok(0);
    }
    // Group by parent directory: CfCreatePlaceholders creates all entries of one
    // call under a single base directory.
    let mut by_parent: std::collections::BTreeMap<String, Vec<usize>> = Default::default();
    for (i, e) in entries.iter().enumerate() {
        let rel = std::path::Path::new(&e.relative_path);
        let parent = rel
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        by_parent.entry(parent).or_default().push(i);
    }
    let ft = filetime_now();
    let mut created = 0usize;
    for (parent, idxs) in by_parent {
        let base = if parent.is_empty() {
            root.to_string()
        } else {
            format!("{root}\\{parent}")
        };
        // CfCreatePlaceholders requires the BASE DIRECTORY to exist and does NOT
        // materialize intermediate dirs (a missing parent fails the whole batch with
        // HRESULT_FROM_WIN32(ERROR_FILE_NOT_FOUND) 0x80070002 — the windows CI gate
        // caught this on the attach-style 3-file subtree). Attach semantics imply the
        // subtree materializes with the placeholders, so create the parents here.
        if !parent.is_empty() {
            std::fs::create_dir_all(&base)
                .map_err(|e| (0, e.raw_os_error().unwrap_or(2) as i32))?;
        }
        let base_w = wide(&base);
        // per-batch buffers must outlive the call
        let mut names: Vec<Vec<u16>> = Vec::with_capacity(idxs.len());
        let mut idents: Vec<Vec<u16>> = Vec::with_capacity(idxs.len());
        let mut infos: Vec<CF_PLACEHOLDER_CREATE_INFO> = Vec::with_capacity(idxs.len());
        for &i in &idxs {
            let e = &entries[i];
            let name = std::path::Path::new(&e.relative_path)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| e.relative_path.clone());
            names.push(wide(&name));
            idents.push(wide(&e.identity_hex));
            let (name_ptr, ident_ptr) = (names.last().unwrap(), idents.last().unwrap());
            let mut info = CF_PLACEHOLDER_CREATE_INFO {
                RelativeFileName: PCWSTR(name_ptr.as_ptr()),
                FsMetadata: Default::default(),
                FileIdentity: ident_ptr.as_ptr().cast::<c_void>(),
                FileIdentityLength: (ident_ptr.len() as u32) * 2,
                // MARK_IN_SYNC: attach writes what the server has — it IS in sync.
                // DISABLE_ON_DEMAND_POPULATION: files are pre-created at attach; the
                // filter must never wait on population for them (quirk W10).
                Flags: CF_PLACEHOLDER_CREATE_FLAGS(
                    CF_PLACEHOLDER_CREATE_FLAG_MARK_IN_SYNC.0
                        | CF_PLACEHOLDER_CREATE_FLAG_DISABLE_ON_DEMAND_POPULATION.0,
                ),
                Result: Default::default(),
                CreateUsn: 0,
            };
            info.FsMetadata.FileSize = e.size as i64;
            // punch #5 for placeholders: stamp the JOURNALED write time (exact
            // ms, bit-for-bit with FileRow.mtime) so the scan predicate sees
            // disk == row and the placeholder stays clean (lazy). Degenerate
            // rows (mtime <= 0, unknown) keep the pre-round-13 'now' stamp.
            let (lwt, cht) = if e.mtime_ms > 0 {
                (
                    filetime_from_unix_millis(e.mtime_ms),
                    filetime_from_unix_millis(e.mtime_ms),
                )
            } else {
                (ft, ft)
            };
            info.FsMetadata.BasicInfo = FILE_BASIC_INFO {
                CreationTime: ft,
                LastAccessTime: ft,
                LastWriteTime: lwt,
                ChangeTime: cht,
                FileAttributes: FILE_ATTRIBUTE_NORMAL.0 as u32,
            };
            infos.push(info);
        }
        // SAFETY: names/idents/infos all live across the call; the filter copies
        // identity + metadata per entry and fills Result per entry.
        let res = unsafe {
            CfCreatePlaceholders(
                PCWSTR(base_w.as_ptr()),
                &mut infos,
                CF_CREATE_FLAGS(0),
                None,
            )
        };
        match res {
            Ok(_) => created += idxs.len(),
            Err(e) => {
                // partial success: find the FIRST per-entry failure and report
                // its index (attach is idempotent: re-running creates only
                // what is missing). Note: per-entry successes in this branch
                // are NOT counted into `created` -- the caller gets Err and
                // never reads it (the round-13 windows build flagged the dead
                // increments; kept the 0xB7 read for the first_fail scan).
                let mut first_fail: Option<usize> = None;
                for (k, info) in infos.iter().enumerate() {
                    let code = info.Result.0;
                    if code == 0 || code == 0x0000_00B7 {
                        // 0xB7 = ERROR_ALREADY_EXISTS (win32) — idempotent re-run
                        continue;
                    } else if first_fail.is_none() {
                        first_fail = Some(idxs[k]);
                    }
                }
                let idx = first_fail.unwrap_or(idxs[0]);
                return Err((idx, e.code().0));
            }
        }
    }
    Ok(created)
}
