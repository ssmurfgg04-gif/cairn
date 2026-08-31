//! Identifiers: UUIDv7 request ids (idempotency, SPEC §7.1) and device ids.

/// Generate a UUIDv7 string (time-ordered request ids; server dedupes on UNIQUE(request_id)).
#[must_use]
pub fn new_request_id() -> String {
    uuid::Uuid::now_v7().to_string()
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
    fn device_ids_prefixed() {
        assert!(new_device_id().starts_with("dev-"));
    }
}
