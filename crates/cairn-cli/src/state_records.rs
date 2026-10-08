//! State-record publish glue (ADR-0031 Phase 1): the fire-and-forget bridge
//! between daemon/CLI surfaces and the sync outbox.
//!
//! WHY THIS EXISTS: the merge + enqueue machinery lives in
//! `cairn_sync::state_records` (one place, shared with replay), but daemon
//! surfaces (ADR-0030 service actions, the RBAC guard, CLI member commands)
//! run where no engine handle exists — the engine is constructed per sync-loop
//! iteration. The outbox IS the send queue, so the correct publish shape here
//! is: validate → apply locally → durable outbox enqueue, and let the next
//! sync pass (≤1s cadence) carry it. Durable-before-send means a crash or a
//! temporarily down daemon loses nothing (I2) — the entry waits pending.
//!
//! DISCIPLINE: every publish in this module is FIRE-AND-FORGET — a record
//! publish failure is logged and swallowed, never propagated. A primary
//! action (member edit, review publish, an enforced RBAC decision) must not
//! fail because its synced side-effect could not be enqueued. The honest
//! failure mode is "this record did not sync", visible in the missing row.
//!
//! PAYLOADS live here, not at call sites: each family hook owns the exact
//! JSON shape it publishes, so the wire form of a record has exactly one
//! definition (the same single-definition discipline as the proto).

use std::path::Path;

use cairn_core::rbac::Member;
use cairn_store::Store;

use cairn_core::clock::SystemClock;

use crate::audit::{AuditEntry, AuditFile};

/// The resolved publish target: which journal/namespace a record rides.
/// `project_id` is the server journal scope, `local_ns` the store row
/// namespace (equal except for additional roots, ADR-0019 §2), `author_id`
/// the journal authorship used for own-op suppression and LWW tie-breaks.
pub struct RecordTarget {
    pub tenant_id: String,
    pub project_id: String,
    pub local_ns: String,
    pub author_id: String,
}

/// The daemon path: resolve the first runtime of `project_id` (empty = first
/// attached project, the same fallback the review actions use). `None` = the
/// project is not attached — there is nothing to publish into.
pub async fn resolve_target(
    state: &crate::daemon::DaemonState,
    project_id: &str,
) -> Option<RecordTarget> {
    let store = crate::service::open_store(&state.home)?;
    let rt = if project_id.is_empty() {
        state.projects.list().await.into_iter().next()?
    } else {
        state.projects.find_by_project(project_id).await?
    };
    let identity = crate::projects::load_identity(&store)?;
    Some(RecordTarget {
        tenant_id: identity.tenant_id,
        project_id: rt.project_id.clone(),
        local_ns: rt.local_ns.clone(),
        author_id: rt.author_id.clone(),
    })
}

/// The CLI path (no daemon): find the workspace binding in the home store
/// whose registered root matches `root`, and read the login identity. `None`
/// = never logged in / root not attached — publishing is skipped (debug log).
pub fn resolve_target_for_root(store: &Store, root: &Path) -> Option<RecordTarget> {
    let identity = crate::projects::load_identity(store)?;
    let want = root.canonicalize().ok()?.to_string_lossy().into_owned();
    for ns in bound_namespaces(store) {
        if cairn_sync::workspace::workspace_dir(store, &ns)
            .to_string_lossy()
            .eq_ignore_ascii_case(&want)
        {
            // split the namespace back into (project, root_id) — the author id
            // mirrors attach's construction exactly (ADR-0019 §2)
            let (project_id, root_id) = match ns.split_once(cairn_sync::workspace::ROOT_NS_SEP) {
                Some((p, r)) => (p.to_string(), r.to_string()),
                None => (ns.clone(), String::new()),
            };
            return Some(RecordTarget {
                tenant_id: identity.tenant_id.clone(),
                project_id,
                local_ns: ns,
                author_id: cairn_sync::workspace::author_id(&identity.device_id, &root_id),
            });
        }
    }
    tracing::debug!(
        root = %root.display(),
        "state record publish skipped: root has no bound namespace"
    );
    None
}

/// Every bound namespace (`workspace:<ns>` meta keys), deterministic order.
fn bound_namespaces(store: &Store) -> Vec<String> {
    let conn = store.conn_handle();
    let conn = conn.lock().expect("store poisoned");
    let mut stmt = match conn.prepare("SELECT key FROM meta WHERE key LIKE 'workspace:%'") {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    stmt.query_map([], |r| r.get::<_, String>(0))
        .map(|rows| {
            rows.filter_map(|r| r.ok())
                .map(|k| k.trim_start_matches("workspace:").to_string())
                .collect()
        })
        .unwrap_or_default()
}

/// The funnel: enqueue (validate → local apply → durable outbox) and LOG any
/// failure. Returns Ok/Err for tests; call sites may simply `let _ =`.
fn enqueue(
    store: &Store,
    target: &RecordTarget,
    family: &str,
    key: &str,
    payload: Vec<u8>,
    tombstone: bool,
) {
    let ts_ms = cairn_core::clock::WallClock.now_millis();
    let res = cairn_sync::state_records::enqueue_state_record(
        store,
        cairn_sync::state_records::PublishParams {
            tenant_id: &target.tenant_id,
            project_id: &target.project_id,
            local_ns: &target.local_ns,
            device_id: &target.author_id,
            family,
            key,
            payload: &payload,
            ts_ms,
            tombstone,
        },
    );
    if let Err(e) = res {
        tracing::warn!(
            family,
            key = %key,
            error = %e.message,
            "state record publish failed (primary action unaffected)"
        );
    }
}

// ---------- family-shaped hooks (one JSON shape per family, defined HERE) ----------

/// `member` family: roster add/update (LWW register keyed by device_id).
pub fn publish_member(store: &Store, target: &RecordTarget, m: &Member) {
    match serde_json::to_vec(m) {
        Ok(payload) => enqueue(store, target, "member", &m.device_id, payload, false),
        Err(e) => tracing::warn!("member record payload build failed (not synced): {e}"),
    }
}

/// `member` family: removal marker (tombstone — the roster is LWW, so the
/// tombstone carries the removal to every device; the winning rank is the
/// removal's own ts).
pub fn publish_member_removal(store: &Store, target: &RecordTarget, device_id: &str) {
    let payload = serde_json::json!({ "device_id": device_id, "removed": true });
    match serde_json::to_vec(&payload) {
        Ok(bytes) => enqueue(store, target, "member", device_id, bytes, true),
        Err(e) => tracing::warn!("member removal payload build failed (not synced): {e}"),
    }
}

/// `audit` family: one RBAC decision (append-only union; key = the audit
/// entry's content id, so the same decision enqueued twice converges — the
/// exact id the local ledger already dedupes by).
pub fn publish_audit(store: &Store, target: &RecordTarget, entry: &AuditEntry) {
    let key = AuditFile::id_for(entry);
    match serde_json::to_vec(entry) {
        Ok(payload) => enqueue(store, target, "audit", &key, payload, false),
        Err(e) => tracing::warn!("audit record payload build failed (not synced): {e}"),
    }
}

/// `review_version` family: one append to the version stack (key = the
/// assigned stack number; payload = the ReviewVersion JSON).
pub fn publish_review_version(
    store: &Store,
    target: &RecordTarget,
    version_json: Vec<u8>,
    number: u32,
) {
    enqueue(
        store,
        target,
        "review_version",
        &number.to_string(),
        version_json,
        false,
    );
}

/// `review_link` family: minted guest link (LWW register keyed by token).
pub fn publish_review_link(store: &Store, target: &RecordTarget, link_json: Vec<u8>, token: &str) {
    enqueue(store, target, "review_link", token, link_json, false);
}

/// `review_link` family: revoke tombstone (the Phase-2 cross-machine revoke
/// substrate — every portal will consult this before honoring the link).
pub fn publish_review_link_revocation(store: &Store, target: &RecordTarget, token: &str) {
    let payload = serde_json::json!({ "token": token, "revoked": true });
    match serde_json::to_vec(&payload) {
        Ok(bytes) => enqueue(store, target, "review_link", token, bytes, true),
        Err(e) => tracing::warn!("link revocation payload build failed (not synced): {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cairn_core::clock::WallClock;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn tmp() -> (tempfile::TempDir, Store, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), Arc::new(WallClock)).unwrap();
        let root = dir.path().join("Brand Film");
        std::fs::create_dir_all(&root).unwrap();
        (dir, store, root)
    }

    fn target() -> RecordTarget {
        RecordTarget {
            tenant_id: "t1".into(),
            project_id: "brand-film".into(),
            local_ns: "brand-film".into(),
            author_id: "dev-A".into(),
        }
    }

    #[test]
    fn member_hook_enqueues_lww_record_with_assigned_key() {
        let (_d, store, _root) = tmp();
        let m = Member {
            device_id: "dev-b".into(),
            name: "Bob".into(),
            role: cairn_core::rbac::Role::Colorist,
            added_at_ms: 42,
            added_by: "dev-A".into(),
        };
        publish_member(&store, &target(), &m);
        // local apply happened (own-op suppression contract) with the record
        // keyed by the member's device id
        let row = store
            .get_state_record("brand-film", "member", "dev-b")
            .unwrap();
        assert!(!row.tombstone);
        let json: serde_json::Value = serde_json::from_slice(&row.payload).unwrap();
        assert_eq!(json["role"], "colorist", "role serializes kebab-case");
        assert_eq!(row.device_id, "dev-A", "authorship is the publishing root");
        // durable op in the outbox for the next sync pass
        assert_eq!(
            cairn_store::Outbox::new(store.conn_handle())
                .pending("brand-film", 10)
                .len(),
            1
        );
    }

    #[test]
    fn member_removal_hook_enqueues_tombstone() {
        let (_d, store, _root) = tmp();
        publish_member_removal(&store, &target(), "dev-b");
        let row = store
            .get_state_record("brand-film", "member", "dev-b")
            .unwrap();
        assert!(
            row.tombstone,
            "removal rides a tombstone (Phase-2 substrate)"
        );
    }

    #[test]
    fn audit_hook_keys_records_by_content_id() {
        let (_d, store, _root) = tmp();
        let e = AuditEntry {
            ts_ms: 1,
            device: "dev-A".into(),
            role: "editor".into(),
            action: "ctl/detach-root".into(),
            project: "brand-film".into(),
            allowed: false,
        };
        publish_audit(&store, &target(), &e);
        publish_audit(&store, &target(), &e); // same decision again: converges
        let rows = store.list_state_records("brand-film", "audit");
        assert_eq!(
            rows.len(),
            1,
            "append family: union by the audit content id"
        );
        let json: serde_json::Value = serde_json::from_slice(&rows[0].payload).unwrap();
        assert_eq!(json["allowed"], false);
        assert_eq!(json["action"], "ctl/detach-root");
    }

    #[test]
    fn review_hooks_cover_version_link_and_revocation() {
        let (_d, store, _root) = tmp();
        publish_review_version(
            &store,
            &target(),
            br#"{"number":1,"label":"cut"}"#.to_vec(),
            1,
        );
        // append family: record_id is a content hash — match by key, not id
        let versions = store.list_state_records("brand-film", "review_version");
        assert!(versions.iter().any(|r| r.key == "1"), "version 1 recorded");

        publish_review_link(&store, &target(), br#"{"token":"tok-1"}"#.to_vec(), "tok-1");
        let link = store
            .get_state_record("brand-film", "review_link", "tok-1")
            .unwrap();
        assert!(!link.tombstone);

        publish_review_link_revocation(&store, &target(), "tok-1");
        let link = store
            .get_state_record("brand-film", "review_link", "tok-1")
            .unwrap();
        assert!(link.tombstone, "revocation tombstones the link record");
    }

    #[test]
    fn cli_target_resolution_finds_the_bound_namespace() {
        let (dir, store, root) = tmp();
        // no identity → None
        assert!(resolve_target_for_root(&store, &root).is_none());
        crate::projects::save_identity(
            &store,
            &crate::projects::Identity {
                server_url: "http://127.0.0.1:9".into(),
                token: "t".into(),
                device_id: "dev-A".into(),
                tenant_id: "t1".into(),
                tls_ca: None,
            },
        )
        .unwrap();
        // identity but no binding → None
        assert!(resolve_target_for_root(&store, &root).is_none());
        // bind the root → target resolves with the plain ns (default root)
        cairn_sync::workspace::set_workspace(&store, "brand-film", &root).unwrap();
        let t = resolve_target_for_root(&store, &root).unwrap();
        assert_eq!(
            (t.project_id.as_str(), t.local_ns.as_str()),
            ("brand-film", "brand-film")
        );
        assert_eq!(t.author_id, "dev-A");
        assert_eq!(t.tenant_id, "t1");
        // additional root (ADR-0019 §2): ns `pid#rid`, project scope stays `pid`
        let root2 = dir.path().join("Second Root");
        std::fs::create_dir_all(&root2).unwrap();
        cairn_sync::workspace::set_workspace_ns(&store, "brand-film#abc123", &root2).unwrap();
        let t2 = resolve_target_for_root(&store, &root2).unwrap();
        assert_eq!(t2.local_ns, "brand-film#abc123");
        assert_eq!(
            t2.project_id, "brand-film",
            "journal scope never carries the root id"
        );
        assert_eq!(t2.author_id, "dev-A#abc123", "authorship mirrors attach");
    }
}
