//! The application/service layer (ADR-0030): the JSON-shaped API the
//! loopback console (and anything else local — the tray, the NLE panel)
//! calls. The architecture review's "UI/application boundary 7/10" finding
//! was that `dashboard.rs` had become a SECOND application layer: handlers
//! opened stores, walked runtimes, wrote review files, and ran ffmpeg
//! directly, so every surface that wanted the same behavior (CLI parity,
//! tray, tests) had to re-implement it.
//!
//! The boundary now is:
//!
//! ```text
//! HTTP adapter (dashboard.rs)   — parse, respond, security gate ONLY
//!        ↓
//! service (this module)         — the application API: one function per
//!        ↓                        console operation, typed in/out
//! domain (projects/review/tl/…) — the sync engine, stores, OS
//! ```
//!
//! Rules for code that lives here:
//! * NO axum types in service signatures (serde_json + plain types only) —
//!   the service must stay testable and callable from non-HTTP surfaces;
//! * RBAC and policy checks happen HERE, not in the adapter — the HTTP
//!   layer cannot forget them;
//! * read-only views live in `views`, state-changing actions in `actions`.

pub mod actions;
pub mod views;

use std::path::{Path, PathBuf};

use cairn_core::clock::WallClock;
use cairn_store::Store;

/// Open the daemon's home store, or nothing — every view degrades to an
/// honest empty shape instead of an invented zero.
pub fn open_store(home: &Path) -> Option<Store> {
    Store::open(home, std::sync::Arc::new(WallClock)).ok()
}

/// Unix ms as i64 (the dashboard's clock vocabulary).
pub fn now_ms_i64() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Map a file row's raw local_state to the badge vocabulary editors
/// already know from cloud drives: local / syncing / synced / conflict.
pub fn file_badge(row: &cairn_store::FileRow) -> &'static str {
    match row.local_state.as_str() {
        "conflict" => "conflict",
        "synced" => "synced",
        "dirty" => "syncing",
        _ => "syncing",
    }
}

/// What humans call the project at this root: the folder name they typed
/// when they attached it (audit #4 — "cairn-test2" is a DB id, "Brand
/// Film" is the project).
pub fn display_name_of(project_id: &str, workspace: &Path) -> String {
    workspace
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| project_id.to_string())
}

/// Resolve a project-relative path inside the project's attached root,
/// refusing traversal (`..`, absolute paths, drive letters, UNC). The
/// quick-actions must never become an arbitrary-file-read primitive.
pub fn safe_join(root: &Path, rel: &str) -> Option<PathBuf> {
    if rel.is_empty() {
        return None;
    }
    let rel_path = std::path::Path::new(rel);
    if rel_path.is_absolute() {
        return None;
    }
    // components like "..", reserved device names, drive-letter colons
    for comp in rel_path.components() {
        match comp {
            std::path::Component::Normal(_) => {}
            std::path::Component::CurDir => {}
            _ => return None, // ParentDir, Prefix (C:), RootDir, UNC
        }
    }
    let joined = root.join(rel_path);
    // belt and braces: canonicalize (when it exists) and re-check prefix
    if let Ok(canon) = joined.canonicalize() {
        let root_canon = root.canonicalize().ok()?;
        if !canon.starts_with(&root_canon) {
            return None;
        }
        Some(canon)
    } else {
        Some(joined)
    }
}

/// Marker-export outcome for `views::markers` — a non-JSON body with
/// content type + filename, or a not-found the adapter maps to 404.
pub enum Export {
    Ready {
        body: Vec<u8>,
        content_type: String,
        filename: String,
    },
    NotFound(String),
}

/// File download outcome for `actions::resolve_download` — the streaming
/// itself stays in the HTTP adapter (it is the one genuinely HTTP-shaped
/// operation: chunked body + content headers).
pub enum Download {
    /// Stream `full` as `name` (Content-Length: len).
    Ready {
        full: PathBuf,
        name: String,
        len: u64,
    },
    ProjectNotAttached,
    TraversalRefused,
    /// Placeholder (or deleted): the recall hint, not a 500.
    NotMaterialized,
    NotAFile,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        tempfile::tempdir().unwrap().keep()
    }

    #[test]
    fn safe_join_refuses_traversal_and_accepts_normal_paths() {
        let root = tmp();
        std::fs::write(root.join("clip.txt"), b"x").unwrap();
        std::fs::create_dir(root.join("sub")).unwrap();
        std::fs::write(root.join("sub").join("a.txt"), b"x").unwrap();

        // inside-the-root paths resolve
        assert!(safe_join(&root, "clip.txt").is_some());
        assert!(safe_join(&root, "sub/a.txt").is_some());
        // traversal and absolute paths are refused
        assert!(safe_join(&root, "../escape").is_none());
        assert!(safe_join(&root, "/etc/passwd").is_none());
        assert!(safe_join(&root, "").is_none());
        // deep-but-legal stays legal
        assert!(safe_join(&root, "sub/a.txt").is_some());
        // ANY ParentDir component is refused — even mid-path where it
        // would resolve inside the root. Conservative on purpose: the
        // quick-actions must never become an arbitrary-file-read primitive.
        assert!(safe_join(&root, "sub/../clip.txt").is_none());
        // drive-letter prefixes are refused where the OS parses them
        // (Component::Prefix); on unix a backslash name is just an odd
        // file INSIDE the project — still confined, so still legal there
        #[cfg(windows)]
        assert!(safe_join(&root, "C:\\windows").is_none());
    }

    #[test]
    fn file_badge_maps_raw_states_to_editor_vocabulary() {
        let mk = |state: &str| cairn_store::FileRow {
            path: "x".into(),
            project_id: "p".into(),
            manifest_hash: None,
            size: 1,
            mode: "file".into(),
            mtime: 1,
            local_state: state.into(),
        };
        assert_eq!(file_badge(&mk("conflict")), "conflict");
        assert_eq!(file_badge(&mk("synced")), "synced");
        assert_eq!(file_badge(&mk("dirty")), "syncing");
        assert_eq!(file_badge(&mk("weird")), "syncing");
    }
}
