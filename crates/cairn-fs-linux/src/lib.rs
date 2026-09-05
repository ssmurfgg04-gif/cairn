//! Cairn Linux filesystem integration (SPEC §10): FUSE (fuser) mount presenting the synced
//! project tree with placeholder semantics — metadata from the local `files` table, content
//! via the header cache (I1: first byte <50ms cached) with chunk streaming + BLAKE3
//! verification on ingest.
//!
//! Write-back (this crate, ADR-0014): editors open files through the mount, acquire an
//! EPHEMERAL PID-BOUND lease (15s TTL, 5s heartbeat), spool edits to a staging temp file,
//! and the LAST close commits — chunk → CAS → manifest tree (children first) → file row
//! (dirty; the sync engine owns the push) → header-cache refresh. A crashed editor's lease
//! is reaped by pid probe on the next acquire: conflicts self-heal, no manual pen.
//!
//! Native passthrough (`.prodsys` layouts, `.cairn-native-collab` marker) skips leases
//! entirely — the vendor's own engine arbitrates, Cairn stands down (Phase 1).
//!
//! Mounting requires `/dev/fuse` (absent in headless CI): mount tests are `#[ignore]` and
//! run on FUSE-enabled machines. Cross-platform watching lives in `cairn-sync::watch`.

#![forbid(unsafe_code)]

use std::sync::Arc;

use cairn_core::CairnError;
use cairn_store::{Cas, Store};

mod fs_impl;

pub use fs_impl::{CairnFs, FsMetricsSnapshot, HEARTBEAT_SECS, LEASE_TTL_MS};

/// Build a filesystem handle for a project (mount via [`CairnFs::mount`]).
pub fn for_project(store: Store, cas: Cas, project_id: &str) -> Result<Arc<CairnFs>, CairnError> {
    for_project_device(store, cas, project_id, "fuse-mount")
}

/// [`for_project`] with an explicit device id (recorded on lease rows).
pub fn for_project_device(
    store: Store,
    cas: Cas,
    project_id: &str,
    device_id: &str,
) -> Result<Arc<CairnFs>, CairnError> {
    // WO6-5 reader-pool fix: FUSE reads fan in concurrently — dedicated readers
    // keep header serves off the store's single write connection. 8 readers
    // (ADR-0025): the r2d2 pool caps there, matching the burst bench config.
    let headers = cairn_store::HeaderCache::with_read_pool(
        store.conn_handle(),
        &store.root().join("db.sqlite"),
        8,
    );
    Ok(Arc::new(CairnFs::with_device(
        store, cas, headers, project_id, device_id,
    )))
}

/// Mount (blocking; requires /dev/fuse + the `fuse` build feature).
#[cfg(feature = "fuse")]
pub fn mount(fs: Arc<CairnFs>, mountpoint: &std::path::Path) -> Result<(), CairnError> {
    fs.mount(mountpoint)
}

/// Spawn the lease heartbeat (ADR-0014 Phase 3): every [`HEARTBEAT_SECS`] renew the
/// pid-bound leases of all files currently open for write through this mount. Returns
/// the JoinHandle so the daemon can observe a dead heartbeat.
pub fn spawn_heartbeat(fs: Arc<CairnFs>) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("cairn-fuse-heartbeat".into())
        .spawn(move || loop {
            if fs.is_stopped() {
                return; // shutdown() after unmount — the daemon must terminate
            }
            std::thread::sleep(std::time::Duration::from_secs(HEARTBEAT_SECS));
            if fs.is_stopped() {
                return;
            }
            fs.heartbeat_once();
        })
        .expect("spawn heartbeat thread")
}
