//! CairnError → tonic Status mapping with structured ErrorDetail (ADR-0010).
//! Never a bare 500-as-catchall: every failure carries `code + retry_class + message`.

use cairn_core::{CairnError, ErrorKind, RetryClass};
use cairn_proto::pb::RetryClass as PbRetry;

/// Map a `CairnError` onto the wire with its retryability hint.
#[must_use]
pub fn status(err: &CairnError) -> tonic::Status {
    let retry = pb_class(err.retry_class());
    cairn_proto::error_status(err.code(), retry, err.message.clone())
}

/// Shorthand for common server-side constructions.
#[must_use]
pub fn status_of(kind: ErrorKind, message: impl Into<String>) -> tonic::Status {
    status(&CairnError::new(kind, message))
}

fn pb_class(c: RetryClass) -> PbRetry {
    match c {
        RetryClass::Auto => PbRetry::RetryAuto,
        RetryClass::Never => PbRetry::RetryNever,
        RetryClass::Conflict => PbRetry::RetryConflict,
        RetryClass::Server => PbRetry::RetryServer,
    }
}

/// Extract from a wire status back to a `CairnError` (client side).
#[must_use]
pub fn from_status(s: &tonic::Status) -> CairnError {
    let d = cairn_proto::error_detail(s);
    let kind = ErrorKind::Internal; // server sends code strings; client classifies by retry hint
    CairnError::new(kind, format!("{}: {}", d.code, d.message))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conflict_maps_to_aborted_with_hint() {
        let s = status_of(ErrorKind::Conflict, "diverged");
        let d = cairn_proto::error_detail(&s);
        assert_eq!(d.code, "CONFLICT");
        assert_eq!(s.code(), tonic::Code::Aborted);
    }

    #[test]
    fn stale_lease_maps_to_conflict_class() {
        let s = status_of(ErrorKind::StaleLease, "token 7 is stale");
        let d = cairn_proto::error_detail(&s);
        assert_eq!(d.code, "STALE_LEASE");
        assert_eq!(s.code(), tonic::Code::Aborted);
    }
}
