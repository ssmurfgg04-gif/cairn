//! Per-project workspace binding (WO1 AttachRoot walking skeleton).
//!
//! The engine historically assumed `store.root()/workspace` (the sim layout). Attaching a
//! REAL user directory registers its absolute path in the store's `meta` table under
//! `workspace:<project_id>`; `Engine::rooted`, the scanner and the hydrator all resolve
//! through [`workspace_dir`]. Stores without a binding keep the sim default, so the
//! deterministic sim is untouched.

use std::path::{Path, PathBuf};

use cairn_core::{CairnError, ErrorKind};
use cairn_store::Store;

/// Meta key prefix for a project's attached workspace.
fn key(project_id: &str) -> String {
    format!("workspace:{project_id}")
}

/// Register (or replace) the attached root for a project.
pub fn set_workspace(store: &Store, project_id: &str, root: &Path) -> Result<(), CairnError> {
    let abs = root
        .canonicalize()
        .map_err(|e| CairnError::new(ErrorKind::Io, format!("canonicalize root: {e}")))?;
    store.meta_set(&key(project_id), &abs.to_string_lossy())
}

/// Remove the binding (detach).
pub fn clear_workspace(store: &Store, project_id: &str) -> Result<(), CairnError> {
    let conn = store.conn_handle();
    let conn = conn.lock().expect("store poisoned");
    conn.execute(
        "DELETE FROM meta WHERE key=?1",
        rusqlite::params![key(project_id)],
    )
    .map_err(|e| CairnError::new(ErrorKind::Io, format!("clear workspace: {e}")))?;
    Ok(())
}

/// Resolve the workspace directory for a project: the attached root when bound,
/// else the sim-compatible `store.root()/workspace`.
#[must_use]
pub fn workspace_dir(store: &Store, project_id: &str) -> PathBuf {
    if let Some(p) = store.meta_get(&key(project_id)) {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    store.root().join("workspace")
}

/// Project id from a folder name: lowercase, alnum/-/_ only, others folded to '-'.
#[must_use]
pub fn project_id_from_name(name: &str) -> String {
    let mut out = String::new();
    for c in name.trim().chars() {
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
            out.push(c.to_ascii_lowercase());
        } else {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "project".into()
    } else {
        trimmed.chars().take(48).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cairn_core::clock::WallClock;
    use std::sync::Arc;

    #[test]
    fn workspace_binding_roundtrip_and_default() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), Arc::new(WallClock)).unwrap();
        // default (sim layout) when unbound
        assert_eq!(workspace_dir(&store, "p1"), dir.path().join("workspace"));
        let root = tempfile::tempdir().unwrap();
        set_workspace(&store, "p1", root.path()).unwrap();
        assert_eq!(workspace_dir(&store, "p1"), root.path());
        clear_workspace(&store, "p1").unwrap();
        assert_eq!(workspace_dir(&store, "p1"), dir.path().join("workspace"));
    }

    #[test]
    fn project_ids_are_slugified() {
        assert_eq!(project_id_from_name("My Project!"), "my-project");
        assert_eq!(project_id_from_name("  "), "project");
        assert_eq!(project_id_from_name("edit-bay-2"), "edit-bay-2");
    }
}
