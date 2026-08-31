//! Error taxonomy — ONE table (SPEC §14, ADR-0010).
//!
//! Retryable (auto, full jitter, max 5, idempotent ops only) · Fatal-client (stop, surface via
//! doctor) · Conflict-class (explicit resolution) · Server-class (respond precisely, never
//! 500-as-catchall). Every error carries a stable code + retryability hint on the wire.

use crate as _; // keep doc lints happy

/// Retry class (wire: `cairn.v4.RetryClass`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryClass {
    /// Auto-retry with full jitter, max 5, idempotent ops only.
    Auto,
    /// Fatal-client: stop and surface via `cairn doctor`.
    Never,
    /// Conflict-class: explicit resolution (CONFLICT, STALE_LEASE, REF_CAS).
    Conflict,
    /// Server-class: structured report, no blind retry.
    Server,
}

/// Stable error codes (keep in lockstep with cairn-proto::ERROR_CODES and docs/ctl-api.md).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// Path diverged from base_seq on a different device → conflict copy path.
    Conflict,
    /// Fencing token stale/expired/mismatched at journal append.
    StaleLease,
    /// Ref CAS failed at fold → retry fold, never lose writes.
    RefCas,
    /// Token invalid/expired/revoked.
    Unauthenticated,
    /// Scope or tenancy denied.
    PermissionDenied,
    /// Object/path/session not found.
    NotFound,
    /// Upload session expired.
    SessionExpired,
    /// Checksum verification failed (bucket rejects corrupt uploads).
    ChecksumMismatch,
    /// BatchExists over the 10k cap.
    BatchTooLarge,
    /// Rate limit / quota.
    RateLimited,
    /// Unexpected internal failure (still carries code + retry hint, never a bare 500).
    Internal,
    /// Transient unavailability (retryable).
    Unavailable,
    /// Client cursor predates journal compaction → snapshot re-sync.
    CompactionRequired,
    /// Upload session cannot accept more receipts.
    SessionFull,
    /// Local manifest verification failure (fatal-client; doctor).
    ManifestFormat,
    /// Chunk failed BLAKE3 verification on ingest (I2 guard).
    ChunkVerification,
    /// Local CAS corruption detected (auto re-download; fatal-client if repeated).
    LocalCasCorrupt,
    /// Compression/decompression failure.
    Compression,
    /// Filesystem/IO error.
    Io,
}

impl ErrorKind {
    /// Stable string code (ADR-0010).
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            ErrorKind::Conflict => "CONFLICT",
            ErrorKind::StaleLease => "STALE_LEASE",
            ErrorKind::RefCas => "REF_CAS",
            ErrorKind::Unauthenticated => "UNAUTHENTICATED",
            ErrorKind::PermissionDenied => "PERMISSION_DENIED",
            ErrorKind::NotFound => "NOT_FOUND",
            ErrorKind::SessionExpired => "SESSION_EXPIRED",
            ErrorKind::ChecksumMismatch => "CHECKSUM_MISMATCH",
            ErrorKind::BatchTooLarge => "BATCH_TOO_LARGE",
            ErrorKind::RateLimited => "RATE_LIMITED",
            ErrorKind::Internal => "INTERNAL",
            ErrorKind::Unavailable => "UNAVAILABLE",
            ErrorKind::CompactionRequired => "COMPACTION_REQUIRED",
            ErrorKind::SessionFull => "SESSION_FULL",
            ErrorKind::ManifestFormat => "CHECKSUM_MISMATCH", // fatal-client family
            ErrorKind::ChunkVerification => "CHECKSUM_MISMATCH",
            ErrorKind::LocalCasCorrupt => "CHECKSUM_MISMATCH",
            ErrorKind::Compression => "CHECKSUM_MISMATCH",
            ErrorKind::Io => "UNAVAILABLE",
        }
    }

    /// Retryability hint (SPEC §14 table). THE table in code.
    #[must_use]
    pub const fn retry_class(self) -> RetryClass {
        match self {
            ErrorKind::Unavailable | ErrorKind::RateLimited | ErrorKind::Io => RetryClass::Auto,
            ErrorKind::Conflict | ErrorKind::StaleLease | ErrorKind::RefCas => RetryClass::Conflict,
            ErrorKind::ManifestFormat
            | ErrorKind::ChunkVerification
            | ErrorKind::LocalCasCorrupt
            | ErrorKind::Unauthenticated
            | ErrorKind::PermissionDenied => RetryClass::Never,
            ErrorKind::NotFound
            | ErrorKind::SessionExpired
            | ErrorKind::SessionFull
            | ErrorKind::ChecksumMismatch
            | ErrorKind::BatchTooLarge
            | ErrorKind::CompactionRequired
            | ErrorKind::Compression
            | ErrorKind::Internal => RetryClass::Server,
        }
    }
}

/// The single error type for the whole engine (thiserror everywhere; no unwrap/panic in prod).
#[derive(Debug, thiserror::Error)]
#[error("{kind:?}: {message}")]
pub struct CairnError {
    /// Taxonomy kind.
    pub kind: ErrorKind,
    /// Human-readable detail (safe to log).
    pub message: String,
}

impl CairnError {
    /// Shorthand constructor.
    #[must_use]
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        CairnError {
            kind,
            message: message.into(),
        }
    }

    /// Stable code for the wire.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        self.kind.code()
    }

    /// Retryability hint for the wire.
    #[must_use]
    pub const fn retry_class(&self) -> RetryClass {
        self.kind.retry_class()
    }
}

impl From<std::io::Error> for CairnError {
    fn from(e: std::io::Error) -> Self {
        CairnError::new(ErrorKind::Io, format!("io: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// §14 truth table spot checks.
    #[test]
    fn retry_matrix_matches_spec() {
        assert_eq!(ErrorKind::RateLimited.retry_class(), RetryClass::Auto);
        assert_eq!(ErrorKind::Unavailable.retry_class(), RetryClass::Auto);
        assert_eq!(ErrorKind::Conflict.retry_class(), RetryClass::Conflict);
        assert_eq!(ErrorKind::StaleLease.retry_class(), RetryClass::Conflict);
        assert_eq!(ErrorKind::RefCas.retry_class(), RetryClass::Conflict);
        assert_eq!(ErrorKind::ManifestFormat.retry_class(), RetryClass::Never);
        assert_eq!(ErrorKind::Unauthenticated.retry_class(), RetryClass::Never);
        assert_eq!(ErrorKind::Internal.retry_class(), RetryClass::Server);
    }

    #[test]
    fn codes_are_stable_strings() {
        assert_eq!(ErrorKind::StaleLease.code(), "STALE_LEASE");
        assert_eq!(ErrorKind::Conflict.code(), "CONFLICT");
        assert_eq!(ErrorKind::RefCas.code(), "REF_CAS");
    }
}
