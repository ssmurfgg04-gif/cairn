//! Identifiers: UUIDv7 request ids (idempotency, SPEC §7.1) and device ids.

/// Generate a UUIDv7 string (time-ordered request ids; server dedupes on UNIQUE(request_id)).
#[must_use]
pub fn new_request_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

/// Content-derived idempotency key (WO6-4 soak finding): identical pushes of the
/// SAME (path, content-version, stat-version) dedup to ONE journal entry no matter
/// how many racing enqueues (watcher vs scan) or crash-recovery replays produce
/// them. A genuine re-save changes mtime and/or manifest → new id → a fresh,
/// legitimate journal entry (keeps A→B→A undo arcs correct, which a pure
/// content hash would collapse).
/// Format: `req-` + 32 hex (TEXT PK; not UUID-shaped by design).
pub fn request_id_for(
    tenant: &str,
    project: &str,
    path: &str,
    manifest_hex: &str,
    size: u64,
    mtime_millis: i64,
) -> String {
    let h = blake3::hash(
        format!("{tenant}\n{project}\n{path}\n{manifest_hex}\n{size}\n{mtime_millis}").as_bytes(),
    );
    let hex = h.to_hex();
    format!("req-{hex}")
}

/// Content-derived idempotency key for state records (ADR-0031 Phase 1).
/// Same shape as [`request_id_for`] — `req-state-` + the full BLAKE3-256 hex —
/// over (tenant, project, family, key, payload, ts, device). Two enqueues of
/// the SAME record (watcher-style races, crash-recovery replays, a caller
/// retrying a fire-and-forget publish) dedupe to ONE journal entry; a genuine
/// re-publish (new ts or payload) gets a fresh id. Append families key records
/// by content (see `cairn_sync::state_records`), so their request ids are
/// naturally stable across retries.
pub fn state_record_request_id(
    tenant: &str,
    project: &str,
    family: &str,
    key: &str,
    payload: &[u8],
    ts_ms: i64,
    device: &str,
) -> String {
    let mut h = blake3::Hasher::new();
    h.update(tenant.as_bytes());
    h.update(b"\n");
    h.update(project.as_bytes());
    h.update(b"\n");
    h.update(family.as_bytes());
    h.update(b"\n");
    h.update(key.as_bytes());
    h.update(b"\n");
    h.update(payload);
    h.update(b"\n");
    h.update(&ts_ms.to_le_bytes());
    h.update(b"\n");
    h.update(device.as_bytes());
    format!("req-state-{}", h.finalize().to_hex())
}

/// New random device id (short, readable).
#[must_use]
pub fn new_device_id() -> String {
    format!("dev-{}", uuid::Uuid::now_v7().simple())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_ids_are_unique() {
        let a = new_request_id();
        let b = new_request_id();
        assert_ne!(a, b);
        assert_eq!(a.len(), 36);
    }

    #[test]
    fn derived_request_ids_are_deterministic_and_version_sensitive() {
        let a = request_id_for("t", "p", "/x/f.prproj", "aa", 10, 100);
        let b = request_id_for("t", "p", "/x/f.prproj", "aa", 10, 100);
        assert_eq!(
            a, b,
            "same version must derive the same id (race/crash dedup)"
        );
        let mtime = request_id_for("t", "p", "/x/f.prproj", "aa", 10, 101);
        let manifest = request_id_for("t", "p", "/x/f.prproj", "ab", 10, 100);
        assert_ne!(a, mtime, "re-save (new mtime) must get a fresh id");
        assert_ne!(a, manifest, "content change must get a fresh id");
        assert!(a.starts_with("req-"));
    }

    #[test]
    fn device_ids_prefixed() {
        assert!(new_device_id().starts_with("dev-"));
    }

    #[test]
    fn state_record_request_ids_are_deterministic_and_content_sensitive() {
        let payload = br#"{"role":"editor"}"#.to_vec();
        let a = state_record_request_id("t", "p", "member", "dev-1", &payload, 100, "dev-A");
        let b = state_record_request_id("t", "p", "member", "dev-1", &payload, 100, "dev-A");
        assert_eq!(
            a, b,
            "same record must derive the same id (race/replay dedup)"
        );
        assert!(a.starts_with("req-state-"), "state ids are namespaced: {a}");
        assert_eq!(a.len(), "req-state-".len() + 64, "full blake3-256 hex");
        // any input change → fresh id
        assert_ne!(
            a,
            state_record_request_id("t", "p", "member", "dev-1", &payload, 101, "dev-A")
        );
        assert_ne!(
            a,
            state_record_request_id("t", "p", "member", "dev-2", &payload, 100, "dev-A")
        );
        assert_ne!(
            a,
            state_record_request_id("t", "p", "audit", "dev-1", &payload, 100, "dev-A")
        );
        assert_ne!(
            a,
            state_record_request_id("t2", "p", "member", "dev-1", &payload, 100, "dev-A")
        );
        assert_ne!(
            a,
            state_record_request_id("t", "p", "member", "dev-1", &payload, 100, "dev-B")
        );
        // The two id namespaces can never collide: request_id_for output is
        // `req-` + hex (hex alphabet only), so no output can start with
        // "req-state-" ('s' is not a hex digit).
        let file_id = request_id_for("t", "p", "member", "x", 0, 0);
        assert!(!file_id.starts_with("req-state-"));
        assert_ne!(a, file_id);
    }
}
