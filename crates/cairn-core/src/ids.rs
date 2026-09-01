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
}
