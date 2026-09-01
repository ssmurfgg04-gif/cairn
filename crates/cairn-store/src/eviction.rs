//! Eviction policy (WO6-2): studios store 2–4 TB/seat; local NVMe cannot hold it.
//! The daemon runs a periodic sweep that keeps the sync root's disk at or above a
//! free-space target by evicting LRU, unpinned, age-guarded chunks from the local
//! CAS. Pinned files (ctl Pin) are protected by the chunk pin bits; actively-used
//! files are protected by the min-age guard; dirty rows are protected by the push
//! path touching their chunks (fresh atime) — and on Windows, CfDehydratePlaceholder
//! fails at the OS layer for oplocked (open) files.

// The ONLY unsafe in this crate: free-space probes (statvfs / GetDiskFreeSpaceExW).
// Both calls write into caller-owned, fully-initialized structs and take
// NUL-terminated paths — no aliasing, no lifetime traps. Everything else in the
// module is safe, and the POLICY math is pure + unit-tested.

use std::path::Path;

use cairn_core::{CairnError, ErrorKind};

use crate::Cas;

// The ONLY unsafe in this crate: free-space probes (statvfs / GetDiskFreeSpaceExW).
// Both calls write into caller-owned, fully-initialized structs and take
// NUL-terminated paths — no aliasing, no lifetime traps. Everything else in the
// module is safe, and the POLICY math is pure + unit-tested.

/// Free-space snapshot in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskSpace {
    pub free: u64,
    pub total: u64,
}

#[cfg(unix)]
pub fn disk_space(path: &Path) -> Result<DiskSpace, CairnError> {
    use std::os::unix::ffi::OsStrExt as _;
    let mut c = path.as_os_str().as_bytes().to_vec();
    c.push(0);
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: st is a valid, fully-initialized statvfs; c is NUL-terminated.
    let rc = unsafe { libc::statvfs(c.as_ptr().cast(), std::ptr::addr_of_mut!(st)) };
    if rc != 0 {
        return Err(CairnError::new(ErrorKind::Io, "statvfs failed"));
    }
    let f_bsize: u64 = st.f_bsize as u64;
    let free = st.f_bavail as u64 * f_bsize;
    let total = st.f_blocks as u64 * f_bsize;
    Ok(DiskSpace { free, total })
}

#[cfg(windows)]
pub fn disk_space(path: &Path) -> Result<DiskSpace, CairnError> {
    use std::os::windows::ffi::OsStrExt as _;
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let mut w: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // windows-rs 0.58 flattens the ULARGE_INTEGER out-params to *mut u64.
    let mut free = 0u64;
    let mut total = 0u64;
    let mut _total_free = 0u64;
    // SAFETY: w is NUL-terminated; out pointers are valid locals.
    let rc = unsafe {
        GetDiskFreeSpaceExW(
            PCWSTR(w.as_mut_ptr()),
            Some(&mut free),
            Some(&mut total),
            Some(&mut _total_free),
        )
    };
    rc.map_err(|e| CairnError::new(ErrorKind::Io, format!("GetDiskFreeSpaceExW: {e}")))?;
    Ok(DiskSpace { free, total })
}

/// PURE policy: how many CAS bytes must go so the disk reaches `target_free_pct`?
/// Returns None when no eviction is needed. Overridable in tests without touching
/// real disks (the sweep feeds measured values in).
pub fn eviction_target_bytes(
    total: u64,
    free: u64,
    target_free_pct: u64,
    cas_live_bytes: u64,
) -> Option<u64> {
    if total == 0 || target_free_pct == 0 || target_free_pct > 99 {
        return None;
    }
    let want_free = total.saturating_mul(target_free_pct) / 100;
    if free >= want_free {
        return None;
    }
    let must_free = want_free - free;
    Some(cas_live_bytes.min(must_free))
}

/// Sweep report (ctl/dashboard surface).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct EvictReport {
    pub needed: bool,
    pub evicted_chunks: usize,
    pub freed_bytes: u64,
    pub target_bytes: u64,
    pub free_before: u64,
    pub total: u64,
}

/// One eviction pass over `store`'s local CAS. Env knobs (read by the CALLER —
/// the daemon loop — so the kill switch and intervals stay process-policy):
/// target free % (default 60), min chunk age (default 3600s).
pub fn evict_sweep(
    store: &crate::Store,
    target_free_pct: u64,
    min_age_secs: i64,
) -> Result<EvictReport, CairnError> {
    let space = disk_space(store.root())?;
    let cas = Cas::open(&store.root().join("blobs"), store.conn_handle())?;
    let live = cas.live_bytes()?;
    let mut report = EvictReport {
        free_before: space.free,
        total: space.total,
        ..EvictReport::default()
    };
    let Some(target) = eviction_target_bytes(space.total, space.free, target_free_pct, live) else {
        return Ok(report); // not needed
    };
    report.needed = true;
    report.target_bytes = target;
    let evicted = cas.evict_to_policy(live.saturating_sub(target), min_age_secs)?;
    report.evicted_chunks = evicted.len();
    // freed bytes = sizes of evicted rows (recompute from DB sum delta is simpler)
    let after = cas.live_bytes()?;
    report.freed_bytes = live.saturating_sub(after);
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_triggers_only_below_target() {
        // 100 GB disk, 50 GB free, target 60% → free 10 GB from CAS
        assert_eq!(eviction_target_bytes(100, 50, 60, 500), Some(10));
        // free already above target → no-op
        assert_eq!(eviction_target_bytes(100, 61, 60, 500), None);
        // CAS smaller than the deficit → evict everything CAS has (cap)
        assert_eq!(eviction_target_bytes(100, 50, 60, 3), Some(3));
        // degenerate inputs
        assert_eq!(eviction_target_bytes(0, 0, 60, 10), None);
        assert_eq!(eviction_target_bytes(100, 10, 100, 10), None);
        assert_eq!(eviction_target_bytes(100, 10, 0, 10), None);
    }

    #[test]
    fn disk_space_reads_something_sane() {
        let dir = tempfile::tempdir().unwrap();
        let s = disk_space(dir.path()).expect("statvfs on tempdir");
        assert!(s.total > 0, "total must be positive");
        assert!(s.free <= s.total, "free cannot exceed total");
    }

    #[test]
    fn age_guard_protects_fresh_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::Store::open(
            dir.path(),
            std::sync::Arc::new(cairn_core::clock::WallClock),
        )
        .unwrap();
        let cas = crate::Cas::open(&dir.path().join("blobs"), store.conn_handle()).unwrap();
        let a = cairn_core::hash::Hash::of(b"alpha");
        let b = cairn_core::hash::Hash::of(b"beta");
        cas.put(&a, b"alpha").unwrap();
        cas.put(&b, b"beta").unwrap();
        // huge future atime would make chunks fresh; simulate by asserting the
        // age guard excludes everything when min_age is astronomical
        let evicted = cas.evict_to_policy(0, i64::MAX / 2_000_000).unwrap();
        assert!(
            evicted.is_empty(),
            "age guard must protect chunks newer than min_age"
        );
        // with no guard, eviction proceeds
        let evicted = cas.evict_to(0).unwrap();
        assert_eq!(evicted.len(), 2, "LRU evicts both unpinned chunks");
        assert!(!cas.contains(&a) && !cas.contains(&b));
    }
}
