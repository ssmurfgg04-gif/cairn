//! Metadata + data plane abstraction. The engine is written against this trait so the
//! deterministic sim (ADR-0008) can drive the REAL code against the REAL in-process server
//! with injected faults (partitions = `Unavailable` errors), while production uses tonic
//! clients + presigned HTTP.

use async_trait::async_trait;
use cairn_core::{CairnError, ErrorKind};
use cairn_proto::pb::journal_op::Op as OpKind;
use cairn_proto::pb::{JournalOp, UploadReceipt};

/// One fetched journal entry (cursor replay).
#[derive(Debug, Clone)]
pub struct Entry {
    pub seq: u64,
    pub device_id: String,
    pub op: JournalOp,
    pub server_ts: i64,
}

/// Upload session with presigned PUTs.
#[derive(Debug, Clone)]
pub struct Session {
    pub id: String,
    pub puts: Vec<(String, String)>, // (hash, url)
    pub expires_at: i64,
}

/// Complete-upload outcome.
#[derive(Debug, Clone)]
pub struct CompleteOut {
    pub verified: Vec<String>,
    pub rejected: Vec<String>,
}

/// Wire + storage surface the engine needs (idempotent ops per ADR-0010).
#[async_trait]
pub trait Plane: Send + Sync {
    async fn batch_exists(
        &self,
        tenant: &str,
        hashes: &[String],
    ) -> Result<Vec<String>, CairnError>;
    async fn create_session(
        &self,
        tenant: &str,
        device: &str,
        project: &str,
        missing: &[String],
    ) -> Result<Session, CairnError>;
    async fn complete(
        &self,
        session: &str,
        receipts: &[UploadReceipt],
    ) -> Result<CompleteOut, CairnError>;
    async fn put_presigned(
        &self,
        url: &str,
        bytes: &[u8],
        checksum_hex: &str,
    ) -> Result<(), CairnError>;
    async fn put_manifest(
        &self,
        tenant: &str,
        manifest_hash: &str,
        bytes: &[u8],
    ) -> Result<(), CairnError>;
    async fn get_manifest(&self, tenant: &str, manifest_hash: &str) -> Result<Vec<u8>, CairnError>;
    /// Fetch a stored object by hash (chunk or manifest) — bytes exactly as stored in the
    /// bucket (chunks are the compressed/stored form; manifests are raw). Hydration path.
    async fn fetch_object(&self, tenant: &str, hash_hex: &str) -> Result<Vec<u8>, CairnError>;
    async fn append(
        &self,
        tenant: &str,
        project: &str,
        device: &str,
        request_id: &str,
        op: JournalOp,
        lease_token: u64,
    ) -> Result<(u64, bool), CairnError>;
    async fn fetch_batch(
        &self,
        tenant: &str,
        project: &str,
        after: u64,
        limit: u32,
    ) -> Result<Vec<Entry>, CairnError>;

    /// Acquire a lease on a path (WO6-1 §5: auto-acquire on project-file open).
    /// Returns (fencing token, expires_at millis). Default impl = unsupported so
    /// existing/test planes stay valid; the gRPC plane implements it for real.
    async fn acquire_lease(
        &self,
        _tenant: &str,
        _project: &str,
        _path: &str,
        _device: &str,
        _ttl_ms: u64,
    ) -> Result<(u64, i64), CairnError> {
        Err(CairnError::new(
            ErrorKind::Internal,
            "lease acquisition not supported by this plane",
        ))
    }

    /// Release a previously acquired lease. Default = unsupported (as above).
    async fn release_lease(
        &self,
        _tenant: &str,
        _project: &str,
        _path: &str,
        _device: &str,
        _token: u64,
    ) -> Result<(), CairnError> {
        Err(CairnError::new(
            ErrorKind::Internal,
            "lease release not supported by this plane",
        ))
    }
}

/// Build a FileUpsert op.
#[must_use]
pub fn upsert_op(path: &str, manifest_hash: &str, size: u64, base_seq: u64) -> JournalOp {
    JournalOp {
        op: Some(OpKind::FileUpsert(cairn_proto::pb::FileUpsertOp {
            path: path.into(),
            manifest_hash: manifest_hash.into(),
            size,
            base_seq,
        })),
    }
}

/// Build a Rename op.
#[must_use]
pub fn rename_op(old_path: &str, new_path: &str, manifest_hash: &str, base_seq: u64) -> JournalOp {
    JournalOp {
        op: Some(OpKind::Rename(cairn_proto::pb::RenameOp {
            old_path: old_path.into(),
            new_path: new_path.into(),
            manifest_hash: manifest_hash.into(),
            base_seq,
        })),
    }
}

/// Build a FileDelete op.
#[must_use]
pub fn delete_op(path: &str, base_seq: u64) -> JournalOp {
    JournalOp {
        op: Some(OpKind::FileDelete(cairn_proto::pb::FileDeleteOp {
            path: path.into(),
            base_seq,
        })),
    }
}
