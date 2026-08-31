//! Cairn Linux filesystem integration (SPEC §10): FUSE (fuser) mount presenting the synced
//! project tree with placeholder semantics — metadata from the local `files` table, content
//! via the header cache (I1: first byte <50ms cached) with chunk streaming + BLAKE3
//! verification on ingest.
//!
//! Mounting requires `/dev/fuse` (absent in headless CI): mount tests are `#[ignore]` and
//! run on FUSE-enabled machines. Cross-platform watching lives in `cairn-sync::watch`.

#![forbid(unsafe_code)]

use std::sync::Arc;

use cairn_core::CairnError;
use cairn_store::{Cas, Store};

mod fs_impl;

pub use fs_impl::CairnFs;

/// Build a filesystem handle for a project (mount via [`CairnFs::mount`]).
pub fn for_project(store: Store, cas: Cas, project_id: &str) -> Result<Arc<CairnFs>, CairnError> {
    let headers = cairn_store::HeaderCache::new(store.conn_handle());
    Ok(Arc::new(CairnFs::new(store, cas, headers, project_id)))
}

/// Mount (blocking; requires /dev/fuse + the `fuse` build feature).
#[cfg(feature = "fuse")]
pub fn mount(fs: Arc<CairnFs>, mountpoint: &Path) -> Result<(), CairnError> {
    fs.mount(mountpoint)
}
