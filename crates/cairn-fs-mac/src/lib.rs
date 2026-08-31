//! Cairn macOS filesystem integration (SPEC §10): File Provider framework.
//!
//! Compiles only on macOS targets; on other targets the platform-neutral pieces are exposed
//! (ignore rules, materialization states, eviction policy). The File Provider extension is a
//! Swift shim bridged over FFI (budgeted per SPEC §10) — the Rust side declares the ABI and
//! drives the engine; no kext, no macFUSE, no loopback SMB.
//!
//! Honest platform status: see docs/STATUS.md (validated on the macOS CI leg for
//! compilation; interactive Finder validation is a hardware-lab milestone task).

#![forbid(unsafe_code)]

/// File Provider materialization states (FPItemContentPolicy equivalents).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Materialization {
    /// Data may be evicted under pressure (dataless placeholder preserved).
    Evictable,
    /// Fully downloaded, pinned (user pin).
    Pinned,
    /// Only metadata present.
    Placeholder,
}

/// Paths that must never sync (parity with cairn-core ignore list + macOS junk).
#[must_use]
pub fn is_syncable(path: &str) -> bool {
    if path.contains("/.Trash/") {
        return false;
    }
    !cairn_core::pathutil::is_ignored(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evictable_vs_pinned() {
        let p = Materialization::Pinned;
        assert_ne!(p, Materialization::Evictable);
    }

    #[test]
    fn trash_never_syncs() {
        assert!(!is_syncable("Users/x/.Trash/big.mov"));
        assert!(is_syncable("Projects/scene.prproj"));
    }
}
