//! Per-project workspace binding (WO1 AttachRoot walking skeleton).
//!
//! The engine historically assumed `store.root()/workspace` (the sim layout). Attaching a
//! REAL user directory registers its absolute path in the store's `meta` table under
//! `workspace:<project_id>`; `Engine::rooted`, the scanner and the hydrator all resolve
//! through [`workspace_dir`]. Stores without a binding keep the sim default, so the
//! deterministic sim is untouched.
//!
//! Round 15 (ADR-0019 §2): MULTI-ROOT — one login/daemon can attach SEVERAL local
//! directories to the SAME project. Each additional root gets a `root_id`; the
//! local store namespace for that root is `<project_id>#<root_id>` (rows, cursor,
//! outbox, leases — fully isolated row tables per root), and its journal
//! authorship is `<device_id>#<root_id>` so the engine's own-op suppression
//! ("already folded locally") only ever skips entries from the SAME root —
//! the cross-root entries apply normally, which is exactly the two-device
//! convergence contract the W-matrix used to need two CAIRN_HOMEs for. The
//! FIRST (legacy) root keeps the plain namespace and the plain device id, so
//! existing stores, cursors and journals behave byte-identically after upgrade.

use std::path::{Path, PathBuf};

use cairn_core::{CairnError, ErrorKind};
use cairn_store::Store;

/// Namespace separator between a project id and a root id. Not valid in a
/// slugified project id (`project_id_from_name` folds it to `-`), so the
/// split is unambiguous.
pub const ROOT_NS_SEP: char = '#';

/// Meta key prefix for a project's attached workspace.
fn key(project_id: &str) -> String {
    format!("workspace:{project_id}")
}

/// Meta key for one registered root of a project.
fn root_key(project_id: &str, root_id: &str) -> String {
    format!("root:{project_id}:{root_id}")
}

/// Register (or replace) the attached root for a project.
pub fn set_workspace(store: &Store, project_id: &str, root: &Path) -> Result<(), CairnError> {
    let abs = root
        .canonicalize()
        .map_err(|e| CairnError::new(ErrorKind::Io, format!("canonicalize root: {e}")))?;
    store.meta_set(&key(project_id), &abs.to_string_lossy())
}

/// Register the workspace binding for a NAMESPACED root (`workspace:<ns>`;
/// the default root's ns is the plain project id, keeping legacy behavior).
pub fn set_workspace_ns(store: &Store, ns: &str, root: &Path) -> Result<(), CairnError> {
    let abs = root
        .canonicalize()
        .map_err(|e| CairnError::new(ErrorKind::Io, format!("canonicalize root: {e}")))?;
    store.meta_set(&key(ns), &abs.to_string_lossy())
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

/// One registered root of a project (round 15: multi-root registry).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootBinding {
    /// `""` for the default (legacy) root, else 8 hex chars.
    pub root_id: String,
    /// Canonical absolute path.
    pub path: PathBuf,
}

/// The local store namespace for a root: the plain project id for the
/// default root (legacy byte-compatibility), `<project_id>#<root_id>` for
/// additional roots. All local row/cursor/outbox keys derive from this.
#[must_use]
pub fn local_ns(project_id: &str, root_id: &str) -> String {
    if root_id.is_empty() {
        project_id.to_string()
    } else {
        format!("{project_id}{ROOT_NS_SEP}{root_id}")
    }
}

/// Journal authorship id for a root: the plain device id for the default
/// root (legacy journals keep suppressing their own entries exactly as
/// before), `<device_id>#<root_id>` for additional roots — decoupling the
/// journal author from the login/socket identity (ADR-0019 §2).
#[must_use]
pub fn author_id(device_id: &str, root_id: &str) -> String {
    if root_id.is_empty() {
        device_id.to_string()
    } else {
        format!("{device_id}{ROOT_NS_SEP}{root_id}")
    }
}

/// Split a namespace back into (project_id, root_id).
#[must_use]
pub fn split_ns(ns: &str) -> (&str, &str) {
    match ns.split_once(ROOT_NS_SEP) {
        Some((p, r)) => (p, r),
        None => (ns, ""),
    }
}

/// Ensure a root_id for an attached path (idempotent by canonical path):
/// - the path already registered → its root_id;
/// - the default slot is free AND (no legacy binding, or the legacy binding
///   IS this path — pre-round-15 stores adopt their root as the default)
///   → `""` (the default root — plain namespace);
/// - anything else → a fresh 8-hex-char root id (blake3 of path+pid+time,
///   collision-safe under the project).
///
/// Does NOT write the workspace binding — attach does that per namespace.
pub fn ensure_root_id(store: &Store, project_id: &str, root: &Path) -> Result<String, CairnError> {
    let abs = root
        .canonicalize()
        .map_err(|e| CairnError::new(ErrorKind::Io, format!("canonicalize root: {e}")))?;
    let abs_s = abs.to_string_lossy().into_owned();
    // existing root with this path?
    let roots = list_roots(store, project_id);
    for b in &roots {
        if b.path == abs {
            return Ok(b.root_id.clone());
        }
    }
    // default slot free AND the legacy binding (pre-round-15) points here
    // (or is absent — first attach ever)? then this becomes the default root.
    let default_taken = roots.iter().any(|b| b.root_id.is_empty());
    let legacy_matches = store
        .meta_get(&key(project_id))
        .as_deref()
        .is_none_or(|p| p.is_empty() || *p == abs_s);
    if !default_taken && legacy_matches {
        store.meta_set(&root_key(project_id, ""), &abs_s)?;
        return Ok(String::new());
    }
    // additional root: fresh id
    let mut material = abs_s.as_bytes().to_vec();
    material.extend_from_slice(project_id.as_bytes());
    material.extend_from_slice(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
            .to_le_bytes()
            .as_slice(),
    );
    let rid = blake3::hash(&material).to_hex()[..8].to_string();
    store.meta_set(&root_key(project_id, &rid), &abs_s)?;
    Ok(rid)
}

/// All registered roots of a project (default root first when present).
#[must_use]
pub fn list_roots(store: &Store, project_id: &str) -> Vec<RootBinding> {
    let prefix = format!("root:{project_id}:");
    let conn = store.conn_handle();
    let Ok(conn) = conn.lock() else {
        return Vec::new();
    };
    let Ok(mut stmt) = conn.prepare("SELECT key, value FROM meta WHERE key LIKE ?1") else {
        return Vec::new();
    };
    let rows = stmt
        .query_map(rusqlite::params![format!("{prefix}%")], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map(|it| it.filter_map(Result::ok).collect::<Vec<_>>())
        .unwrap_or_default();
    let mut out: Vec<RootBinding> = rows
        .into_iter()
        .filter_map(|(k, v)| {
            let rid = k.strip_prefix(&prefix)?.to_string();
            if rid.is_empty() || rid.len() == 8 {
                Some(RootBinding {
                    root_id: rid,
                    path: PathBuf::from(v),
                })
            } else {
                None
            }
        })
        .collect();
    // stable order: default first, then by root_id
    out.sort_by_key(|b| (!b.root_id.is_empty(), b.root_id.clone()));
    out
}

/// Drop one root registration (detach). Removing the default root clears the
/// legacy workspace binding too.
pub fn clear_root(store: &Store, project_id: &str, root_id: &str) -> Result<(), CairnError> {
    let conn = store.conn_handle();
    let conn = conn.lock().expect("store poisoned");
    conn.execute(
        "DELETE FROM meta WHERE key=?1",
        rusqlite::params![root_key(project_id, root_id)],
    )
    .map_err(|e| CairnError::new(ErrorKind::Io, format!("clear root: {e}")))?;
    drop(conn);
    if root_id.is_empty() {
        // the legacy binding pointed at the default root
        let _ = clear_workspace(store, project_id);
    }
    Ok(())
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
        // the binding stores the CANONICAL path: on a Windows runner
        // tempdir() hands back the 8.3 short form (C:\Users\RUNNER~1\...)
        // while canonicalize() returns the long \?\ form — the PATHS are
        // the same directory, the STRINGS are not. Compare the canonical
        // forms (the round-27 beta windows shard caught this: the test
        // only ever ran on linux before, where the two shapes coincide).
        let canon = root
            .path()
            .canonicalize()
            .unwrap_or_else(|_| root.path().to_path_buf());
        assert_eq!(workspace_dir(&store, "p1"), canon);
        clear_workspace(&store, "p1").unwrap();
        assert_eq!(workspace_dir(&store, "p1"), dir.path().join("workspace"));
    }

    #[test]
    fn project_ids_are_slugified() {
        assert_eq!(project_id_from_name("My Project!"), "my-project");
        assert_eq!(project_id_from_name("  "), "project");
        assert_eq!(project_id_from_name("edit-bay-2"), "edit-bay-2");
    }

    #[test]
    fn first_root_is_default_and_namespaces_compose() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), Arc::new(WallClock)).unwrap();
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        // first attach → default root ""
        let r1 = ensure_root_id(&store, "p1", a.path()).unwrap();
        assert_eq!(r1, "");
        assert_eq!(local_ns("p1", &r1), "p1");
        assert_eq!(author_id("dev1", &r1), "dev1");
        // second attach → fresh 8-hex id, namespaced
        let r2 = ensure_root_id(&store, "p1", b.path()).unwrap();
        assert_eq!(r2.len(), 8);
        assert_eq!(local_ns("p1", &r2), format!("p1{ROOT_NS_SEP}{r2}"));
        assert_eq!(author_id("dev1", &r2), format!("dev1{ROOT_NS_SEP}{r2}"));
        assert_eq!(split_ns(&local_ns("p1", &r2)), ("p1", r2.as_str()));
        // idempotent re-attach
        assert_eq!(ensure_root_id(&store, "p1", b.path()).unwrap(), r2);
        assert_eq!(ensure_root_id(&store, "p1", a.path()).unwrap(), "");
        // registry lists both, default first
        let roots = list_roots(&store, "p1");
        assert_eq!(roots.len(), 2);
        assert_eq!(roots[0].root_id, "");
        assert_eq!(roots[0].path, a.path().canonicalize().unwrap());
        assert_eq!(roots[1].root_id, r2);
        // attach writes the per-ns workspace bindings; namespaces resolve to
        // distinct workspace dirs
        set_workspace_ns(&store, "p1", a.path()).unwrap();
        set_workspace_ns(&store, &local_ns("p1", &r2), b.path()).unwrap();
        assert_eq!(
            workspace_dir(&store, "p1"),
            a.path().canonicalize().unwrap()
        );
        assert_eq!(
            workspace_dir(&store, &local_ns("p1", &r2)),
            b.path().canonicalize().unwrap()
        );
        // detaching the second root leaves the default intact
        clear_root(&store, "p1", &r2).unwrap();
        assert_eq!(list_roots(&store, "p1").len(), 1);
    }

    #[test]
    fn legacy_binding_adopts_default_root() {
        // a store from BEFORE round 15 has only workspace:p1 — attaching the
        // same path must adopt the default root (no suffix, no re-push)
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), Arc::new(WallClock)).unwrap();
        let root = tempfile::tempdir().unwrap();
        set_workspace(&store, "p1", root.path()).unwrap();
        let rid = ensure_root_id(&store, "p1", root.path()).unwrap();
        assert_eq!(rid, "");
        // attaching a DIFFERENT path does NOT steal the default slot
        let other = tempfile::tempdir().unwrap();
        let rid2 = ensure_root_id(&store, "p1", other.path()).unwrap();
        assert_eq!(rid2.len(), 8);
        assert_eq!(
            store.meta_get(&key("p1")).as_deref(),
            Some(
                root.path()
                    .canonicalize()
                    .unwrap()
                    .to_string_lossy()
                    .as_ref()
            )
        );
    }
}
