//! Native collaboration passthrough (ADR-0014 Phase 1): when a vendor's OWN multi-user
//! engine owns state arbitration, Cairn STANDS DOWN — no lease is taken, no fencing is
//! imposed. Concurrency yield goes to the vendor engine, and our risk of fighting it
//! goes to zero (the strategy: "do not solve at the byte level what can be eliminated
//! at the structural level").
//!
//! Detected today:
//! - **Premiere Productions**: a `.prodsys` directory is the on-disk marker of Adobe's
//!   shared-production mode. Any file under (or beside) one is arbitrator-free-usable
//!   by Premiere itself — Cairn leasing would only add a second pen to a project that
//!   already has a working one.
//! - **Operator-declared**: a `.cairn-native-collab` marker file in the project root,
//!   containing a mode line (`resolve-collab`, `production`, `custom`). Resolve's
//!   PostgreSQL collab has NO portable on-disk marker in project files, so we do not
//!   pretend to sniff it — operators declare it; honesty over magic (see ADR-0014).
//!
//! Fencing correctness is unaffected either way: a file that syncs WITHOUT a lease
//! simply appends with token 0 (the advisory layer is optional by design, SPEC §8);
//! a file that keeps one behaves exactly as before.

use std::path::Path;

/// What owns write-arbitration for a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeCollab {
    /// Cairn leases + fencing (default).
    Cairn,
    /// Adobe Premiere Productions (`.prodsys` layout) — vendor arbitrates.
    PremiereProductions,
    /// Operator-declared native engine (`.cairn-native-collab` marker).
    OperatorDeclared,
}

impl NativeCollab {
    /// True when Cairn must NOT take leases for this path (Phase 1 stand-down).
    #[must_use]
    pub fn is_passthrough(self) -> bool {
        !matches!(self, NativeCollab::Cairn)
    }
}

/// Marker file that declares a vendor-native collab mode for the whole workspace.
pub const MARKER_FILE: &str = ".cairn-native-collab";

/// Pure detection for contexts with NO workspace root on disk — FUSE mounts serve a
/// virtual tree, so there is nothing to `read_dir` and the marker travels as a synced
/// project file (read from the store, not the filesystem). Covers:
/// 1. the `.prodsys` path-component rule (Premiere Productions), and
/// 2. the operator-declared marker mode (`.cairn-native-collab` content).
///
/// The sibling-directory probe in [`detect`] (a `.prodsys` directory BESIDE the file's
/// tree, found by walking real directories) is resolved mount-side from the synced path
/// set instead — see `cairn-fs-linux::fs_impl` (`NativeLayout`), which reproduces the
/// same semantics against the virtual tree. No proprietary schema is parsed here either.
#[must_use]
pub fn detect_pure(rel_path: &str, marker_mode: Option<&str>) -> NativeCollab {
    // 1. Premiere Productions: any `.prodsys` directory component in the path itself.
    if rel_path.split(['/', '\\']).any(|c| c.ends_with(".prodsys")) {
        return NativeCollab::PremiereProductions;
    }
    // 2. Operator-declared mode (marker content supplied by the caller).
    if let Some(mode) = marker_mode {
        match mode.trim().to_ascii_lowercase().as_str() {
            "resolve-collab" | "production" | "custom" => {
                return NativeCollab::OperatorDeclared;
            }
            "" | "cairn" | "off" => {}
            other => {
                tracing::warn!(
                    mode = %other,
                    "unknown .cairn-native-collab mode — treating as Cairn-leased"
                );
            }
        }
    }
    NativeCollab::Cairn
}

/// Detect the arbitration owner for `rel_path` inside workspace `root`.
/// Pure path logic + one marker read — no project-file parsing, ever (ADR-0014:
/// proprietary schema sniffing is the rejected Phase-4-shaped mistake).
#[must_use]
pub fn detect(root: &Path, rel_path: &str) -> NativeCollab {
    // 1. Premiere Productions: any `.prodsys` directory component in the path
    //    (the production DB dir; .prproj pointers live inside it).
    if rel_path.split(['/', '\\']).any(|c| c.ends_with(".prodsys")) {
        return NativeCollab::PremiereProductions;
    }
    // 2. Sibling `.prodsys` beside the file's directory: `Show/Project.prodsys/` +
    //    `Show/Sequences/scene.prproj` — Productions layouts vary; both are vendor-owned.
    if let Some(dir) = Path::new(rel_path).parent() {
        let mut anc = Some(dir);
        while let Some(d) = anc {
            let probe = root.join(d).read_dir().ok();
            if let Some(entries) = probe {
                for e in entries.flatten() {
                    let name = e.file_name();
                    let name = name.to_string_lossy();
                    if name.ends_with(".prodsys") && e.path().is_dir() {
                        return NativeCollab::PremiereProductions;
                    }
                }
            }
            anc = d.parent();
        }
    }
    // 3. Operator-declared mode for the workspace.
    if let Ok(mode) = std::fs::read_to_string(root.join(MARKER_FILE)) {
        let mode = mode.trim().to_ascii_lowercase();
        match mode.as_str() {
            "resolve-collab" | "production" | "custom" => return NativeCollab::OperatorDeclared,
            "" | "cairn" | "off" => {}
            other => {
                tracing::warn!(mode = %other, "unknown .cairn-native-collab mode — treating as Cairn-leased");
            }
        }
    }
    NativeCollab::Cairn
}

/// Convenience: should Cairn stand down (skip lease acquire) for this path?
#[must_use]
pub fn is_passthrough(root: &Path, rel_path: &str) -> bool {
    detect(root, rel_path).is_passthrough()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prodsys_component_is_passthrough() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(
            detect(root.path(), "Show.prodsys/Show_01.prproj"),
            NativeCollab::PremiereProductions
        );
        assert!(is_passthrough(root.path(), "Show.prodsys/Show_01.prproj"));
        // Windows-style separators too
        assert!(is_passthrough(root.path(), "Show.prodsys\\Show_01.prproj"));
    }

    #[test]
    fn sibling_prodsys_directory_is_passthrough() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("Show/Show.prodsys")).unwrap();
        std::fs::create_dir_all(root.path().join("Show/Sequences")).unwrap();
        std::fs::write(root.path().join("Show/Sequences/scene.prproj"), b"x").unwrap();
        assert!(is_passthrough(root.path(), "Show/Sequences/scene.prproj"));
        // a file OUTSIDE the Show tree stays Cairn-leased
        assert!(!is_passthrough(root.path(), "Other/other.prproj"));
    }

    #[test]
    fn operator_marker_declares_resolve_collab() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join(MARKER_FILE), b"resolve-collab\n").unwrap();
        assert!(is_passthrough(root.path(), "Projects/a.rpp"));
        assert_eq!(
            detect(root.path(), "Projects/a.rpp"),
            NativeCollab::OperatorDeclared
        );
        // explicit off/empty keeps Cairn arbitration
        std::fs::write(root.path().join(MARKER_FILE), b"off").unwrap();
        assert!(!is_passthrough(root.path(), "Projects/a.rpp"));
    }

    #[test]
    fn ordinary_paths_stay_cairn_leaked() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("movie")).unwrap();
        assert_eq!(
            detect(root.path(), "movie/scene.prproj"),
            NativeCollab::Cairn
        );
        assert_eq!(detect(root.path(), "render.exr"), NativeCollab::Cairn);
        // "prodsys" WITHOUT the dot-prefix is just a directory name — not a marker
        assert!(!is_passthrough(root.path(), "prodsys/notes.txt"));
    }

    /// Pure detection (FUSE path): no root on disk — marker content comes from the
    /// synced project file; `.prodsys` component rule identical to `detect`.
    #[test]
    fn detect_pure_matches_detect_semantics() {
        assert_eq!(
            detect_pure("Show.prodsys/Show_01.prproj", None),
            NativeCollab::PremiereProductions
        );
        assert_eq!(
            detect_pure("Sequences/scene.prproj", Some("resolve-collab\n")),
            NativeCollab::OperatorDeclared
        );
        assert_eq!(
            detect_pure("Sequences/scene.prproj", Some("off")),
            NativeCollab::Cairn
        );
        assert_eq!(detect_pure("render.exr", None), NativeCollab::Cairn);
        // "prodsys" without the dot-prefix stays Cairn-leased (same as detect)
        assert_eq!(detect_pure("prodsys/notes.txt", None), NativeCollab::Cairn);
        // agreement with the filesystem-based detector on marker handling
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join(MARKER_FILE), b"production").unwrap();
        assert_eq!(
            detect(root.path(), "a.rpp"),
            detect_pure("a.rpp", Some("production"))
        );
    }
}
