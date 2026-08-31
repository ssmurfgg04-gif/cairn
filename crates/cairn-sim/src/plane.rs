//! In-process plane: the REAL server functions behind cairn-sync's `Plane` trait, with
//! injected fault states (partition). `put_presigned` verifies the checksum exactly as the
//! bucket would (the HTTP layer itself is covered by the M3 e2e).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use cairn_core::hash::Hash;
use cairn_core::{CairnError, ErrorKind};
use cairn_proto::pb::UploadReceipt;
use cairn_server::storage::LocalFsStore;
use cairn_server::ServerState;
use sha2::{Digest, Sha256};

use cairn_sync::plane::{CompleteOut, Entry, Session};

/// Fault state shared across devices.
#[derive(Default)]
pub struct Faults {
    /// When true, every plane call returns `UNAVAILABLE` (network partition).
    pub partition: AtomicBool,
    /// Count of injected partition errors (schedule diagnostics).
    pub partition_errors: AtomicU64,
}

/// Plane bound to one device's identity (tenant/device), shared server state.
pub struct InProcPlane {
    pub state: Arc<ServerState>,
    pub tenant_id: String,
    pub device_id: String,
    pub faults: Arc<Faults>,
}

fn partitioned(f: &Faults) -> Option<CairnError> {
    if f.partition.load(Ordering::SeqCst) {
        f.partition_errors.fetch_add(1, Ordering::SeqCst);
        Some(CairnError::new(ErrorKind::Unavailable, "simulated partition"))
    } else {
        None
    }
}

#[async_trait]
impl cairn_sync::plane::Plane for InProcPlane {
    async fn batch_exists(&self, tenant: &str, hashes: &[String]) -> Result<Vec<String>, CairnError> {
        if let Some(e) = partitioned(&self.faults) {
            return Err(e);
        }
        cairn_server::upload::batch_exists(&self.state, tenant, hashes).await
    }

    async fn create_session(
        &self,
        tenant: &str,
        device: &str,
        project: &str,
        missing: &[String],
    ) -> Result<Session, CairnError> {
        if let Some(e) = partitioned(&self.faults) {
            return Err(e);
        }
        let identity = cairn_server::auth::DeviceIdentity {
            device_id: device.to_string(),
            tenant_id: tenant.to_string(),
            scopes: "sync".into(),
        };
        let out = cairn_server::upload::create_session(&self.state, &identity, project, missing).await?;
        Ok(Session { id: out.session_id, puts: out.puts.into_iter().map(|p| (p.chunk_hash, p.url)).collect(), expires_at: 0 })
    }

    async fn complete(&self, session: &str, receipts: &[UploadReceipt]) -> Result<CompleteOut, CairnError> {
        if let Some(e) = partitioned(&self.faults) {
            return Err(e);
        }
        let identity = cairn_server::auth::DeviceIdentity {
            device_id: self.device_id.clone(),
            tenant_id: self.tenant_id.clone(),
            scopes: "sync".into(),
        };
        let out = cairn_server::upload::complete(&self.state, &identity, session, receipts.to_vec()).await?;
        Ok(CompleteOut { verified: out.verified, rejected: out.rejected })
    }

    async fn put_presigned(&self, url: &str, bytes: &[u8], checksum_hex: &str) -> Result<(), CairnError> {
        if let Some(e) = partitioned(&self.faults) {
            return Err(e);
        }
        // bucket semantics: reject corrupt uploads
        let digest = cairn_core::hash::hex_encode(&Sha256::digest(bytes));
        if digest != checksum_hex {
            return Err(CairnError::new(ErrorKind::ChecksumMismatch, "sim bucket: checksum mismatch"));
        }
        // store at the key the presigned URL names (prefix = ".../objects/")
        let path = url.split("/objects/").nth(1).unwrap_or("");
        let key = path.split('?').next().unwrap_or("");
        if key.is_empty() {
            return Err(CairnError::new(ErrorKind::Internal, "sim bucket: bad url"));
        }
        self.state.store.put(key, bytes).await
    }

    async fn put_manifest(&self, tenant: &str, manifest_hash: &str, bytes: &[u8]) -> Result<(), CairnError> {
        if let Some(e) = partitioned(&self.faults) {
            return Err(e);
        }
        if Hash::of(bytes).hex() != manifest_hash {
            return Err(CairnError::new(ErrorKind::ChecksumMismatch, "manifest hash mismatch"));
        }
        cairn_server::upload::register_manifest(&self.state, tenant, manifest_hash, bytes).await
    }

    async fn get_manifest(&self, tenant: &str, manifest_hash: &str) -> Result<Vec<u8>, CairnError> {
        if let Some(e) = partitioned(&self.faults) {
            return Err(e);
        }
        self.state.store.get(&LocalFsStore::object_key(tenant, manifest_hash)).await
    }

    async fn append(
        &self,
        tenant: &str,
        project: &str,
        device: &str,
        request_id: &str,
        op: cairn_proto::pb::JournalOp,
        lease_token: u64,
    ) -> Result<(u64, bool), CairnError> {
        if let Some(e) = partitioned(&self.faults) {
            return Err(e);
        }
        cairn_server::journal::append(&self.state.db, &self.state.clock, tenant, project, device, request_id, op, lease_token).await
    }

    async fn fetch_batch(&self, tenant: &str, project: &str, after: u64, limit: u32) -> Result<Vec<Entry>, CairnError> {
        if let Some(e) = partitioned(&self.faults) {
            return Err(e);
        }
        cairn_server::journal::batch(&self.state.db, tenant, project, after, limit)
            .await?
            .into_iter()
            .map(|e| Entry { seq: e.seq, device_id: e.device_id, op: e.op, server_ts: e.server_ts })
            .map(Ok)
            .collect()
    }
}
