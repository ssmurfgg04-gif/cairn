//! Tonic service layer: wraps the core ops (journal/leases/auth/storage) behind the frozen
//! cairn.v4 contract. Stateless — horizontal scale is safe (SQLite-compatible SQL backing).

use std::sync::Arc;

use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};

use cairn_core::CairnError;
use cairn_proto::pb::journal_op::Op as OpKind;
use cairn_proto::pb::journal_server::Journal;
use cairn_proto::pb::upload_server::Upload;
use cairn_proto::pb::{
    AppendRequest, AppendResponse, BatchExistsRequest, BatchExistsResponse, CompleteUploadRequest,
    CompleteUploadResponse, CreateUploadSessionRequest, CreateUploadSessionResponse, CursorUpdate,
    GetDownloadUrlRequest, GetDownloadUrlResponse, GetManifestRequest, JournalBatch, JournalEntry,
    ManifestObject, WatchRequest,
};

use crate::error_map::status;
use crate::ServerState;

/// Max hashes per BatchExists call (SPEC §9.1).
pub const BATCH_EXISTS_CAP: usize = 10_000;

fn internal(e: CairnError) -> Status {
    status(&e)
}

// ---------------- Journal ----------------

pub struct JournalSvc {
    pub state: Arc<ServerState>,
}

#[tonic::async_trait]
impl Journal for JournalSvc {
    async fn append(
        &self,
        request: Request<AppendRequest>,
    ) -> Result<Response<AppendResponse>, Status> {
        // authenticate BEFORE consuming the request (metadata needed first)
        let identity = self
            .state
            .authenticate_metadata(&request)
            .await
            .map_err(internal)?;
        let req = request.into_inner();
        let Some(op) = req.op else {
            return Err(Status::invalid_argument("missing op"));
        };
        // authz: caller must be the device it claims (tenant-scope enforced by token claims)
        if identity.device_id != req.device_id || identity.tenant_id != req.tenant_id {
            self.state.audit_denial(&identity, "journal.append").await;
            return Err(Status::permission_denied("device/tenant mismatch"));
        }
        let (seq, dedup) = crate::journal::append(
            &self.state.db,
            &self.state.clock,
            &req.tenant_id,
            &req.project_id,
            &req.device_id,
            &req.request_id,
            op,
            req.lease_token,
        )
        .await
        .map_err(internal)?;
        Ok(Response::new(AppendResponse {
            seq,
            deduplicated: dedup,
        }))
    }

    type WatchStream = ReceiverStream<Result<JournalBatch, Status>>;

    async fn watch(
        &self,
        request: Request<WatchRequest>,
    ) -> Result<Response<Self::WatchStream>, Status> {
        let identity = self
            .state
            .authenticate_metadata(&request)
            .await
            .map_err(internal)?;
        let req = request.into_inner();
        if identity.tenant_id != req.tenant_id {
            self.state.audit_denial(&identity, "journal.watch").await;
            return Err(Status::permission_denied("tenant mismatch"));
        }
        // Watch is a HINT (§7.1): a polling stream from the cursor; replay is the guarantee.
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        let db = self.state.db.clone();
        let (tenant, project, mut cursor) = (req.tenant_id, req.project_id, req.cursor);
        tokio::spawn(async move {
            loop {
                match crate::journal::batch(&db, &tenant, &project, cursor, 256).await {
                    Ok(entries) => {
                        for e in entries {
                            cursor = e.seq;
                            let wire = JournalBatch {
                                entries: vec![JournalEntry {
                                    seq: e.seq,
                                    device_id: e.device_id.clone(),
                                    op: Some(e.op),
                                    server_ts: e.server_ts,
                                }],
                            };
                            if tx.send(Ok(wire)).await.is_err() {
                                return; // client gone
                            }
                        }
                    }
                    Err(_) => return,
                }
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        });
        Ok(Response::new(ReceiverStream::new(rx)))
    }

    async fn update_cursor(
        &self,
        request: Request<CursorUpdate>,
    ) -> Result<Response<cairn_proto::pb::Ack>, Status> {
        let identity = self
            .state
            .authenticate_metadata(&request)
            .await
            .map_err(internal)?;
        let req = request.into_inner();
        if identity.device_id != req.device_id || identity.tenant_id != req.tenant_id {
            self.state.audit_denial(&identity, "journal.cursor").await;
            return Err(Status::permission_denied("device/tenant mismatch"));
        }
        crate::journal::update_cursor(
            &self.state.db,
            &req.tenant_id,
            &req.device_id,
            &req.project_id,
            req.last_seq,
        )
        .await
        .map_err(internal)?;
        Ok(Response::new(cairn_proto::pb::Ack { ok: true }))
    }

    async fn fetch_batch(
        &self,
        request: Request<cairn_proto::pb::FetchBatchRequest>,
    ) -> Result<Response<cairn_proto::pb::FetchBatchResponse>, Status> {
        let identity = self
            .state
            .authenticate_metadata(&request)
            .await
            .map_err(internal)?;
        let req = request.into_inner();
        if identity.tenant_id != req.tenant_id {
            self.state.audit_denial(&identity, "journal.fetch_batch").await;
            return Err(Status::permission_denied("tenant mismatch"));
        }
        let limit = req.limit.clamp(1, 512);
        let entries = crate::journal::batch(
            &self.state.db,
            &req.tenant_id,
            &req.project_id,
            req.after,
            limit,
        )
        .await
        .map_err(internal)?;
        Ok(Response::new(cairn_proto::pb::FetchBatchResponse {
            entries: entries
                .into_iter()
                .map(|e| JournalEntry {
                    seq: e.seq,
                    device_id: e.device_id,
                    op: Some(e.op),
                    server_ts: e.server_ts,
                })
                .collect(),
        }))
    }
}

// ---------------- Upload (data plane control, M3) ----------------

pub struct UploadSvc {
    pub state: Arc<ServerState>,
}

#[tonic::async_trait]
impl Upload for UploadSvc {
    async fn batch_exists(
        &self,
        request: Request<BatchExistsRequest>,
    ) -> Result<Response<BatchExistsResponse>, Status> {
        let identity = self
            .state
            .authenticate_metadata(&request)
            .await
            .map_err(internal)?;
        let req = request.into_inner();
        if identity.tenant_id != req.tenant_id {
            self.state
                .audit_denial(&identity, "upload.batch_exists")
                .await;
            return Err(Status::permission_denied("tenant mismatch"));
        }
        if req.chunk_hashes.len() > BATCH_EXISTS_CAP {
            return Err(cairn_proto::error_status(
                "BATCH_TOO_LARGE",
                cairn_proto::pb::RetryClass::RetryServer,
                format!("batch {} > cap {BATCH_EXISTS_CAP}", req.chunk_hashes.len()),
            ));
        }
        let missing = crate::upload::batch_exists(&self.state, &req.tenant_id, &req.chunk_hashes)
            .await
            .map_err(internal)?;
        Ok(Response::new(BatchExistsResponse { missing }))
    }

    async fn create_upload_session(
        &self,
        request: Request<CreateUploadSessionRequest>,
    ) -> Result<Response<CreateUploadSessionResponse>, Status> {
        let identity = self
            .state
            .authenticate_metadata(&request)
            .await
            .map_err(internal)?;
        let req = request.into_inner();
        if identity.tenant_id != req.tenant_id {
            self.state
                .audit_denial(&identity, "upload.create_session")
                .await;
            return Err(Status::permission_denied("tenant mismatch"));
        }
        let session =
            crate::upload::create_session(&self.state, &identity, &req.project_id, &req.missing)
                .await
                .map_err(internal)?;
        Ok(Response::new(session))
    }

    async fn complete_upload(
        &self,
        request: Request<CompleteUploadRequest>,
    ) -> Result<Response<CompleteUploadResponse>, Status> {
        let identity = self
            .state
            .authenticate_metadata(&request)
            .await
            .map_err(internal)?;
        let req = request.into_inner();
        if identity.tenant_id != req.tenant_id {
            self.state.audit_denial(&identity, "upload.complete").await;
            return Err(Status::permission_denied("tenant mismatch"));
        }
        let out = crate::upload::complete(&self.state, &identity, &req.session_id, req.receipts)
            .await
            .map_err(internal)?;
        Ok(Response::new(out))
    }

    async fn register_manifest(
        &self,
        request: Request<cairn_proto::pb::RegisterManifestRequest>,
    ) -> Result<Response<cairn_proto::pb::Ack>, Status> {
        let identity = self
            .state
            .authenticate_metadata(&request)
            .await
            .map_err(internal)?;
        let req = request.into_inner();
        if identity.tenant_id != req.tenant_id {
            self.state
                .audit_denial(&identity, "upload.register_manifest")
                .await;
            return Err(Status::permission_denied("tenant mismatch"));
        }
        crate::upload::register_manifest(
            &self.state,
            &req.tenant_id,
            &req.manifest_hash,
            &req.body,
        )
        .await
        .map_err(internal)?;
        Ok(Response::new(cairn_proto::pb::Ack { ok: true }))
    }
}

// ---------------- Download (data plane control, M3) ----------------

pub struct DownloadSvc {
    pub state: Arc<ServerState>,
}

#[tonic::async_trait]
impl cairn_proto::pb::download_server::Download for DownloadSvc {
    async fn get_download_url(
        &self,
        request: Request<GetDownloadUrlRequest>,
    ) -> Result<Response<GetDownloadUrlResponse>, Status> {
        let identity = self
            .state
            .authenticate_metadata(&request)
            .await
            .map_err(internal)?;
        let req = request.into_inner();
        if identity.tenant_id != req.tenant_id {
            return Err(Status::permission_denied("tenant mismatch"));
        }
        let (url, expires_at) =
            crate::upload::download_url(&self.state, &req.tenant_id, &req.manifest_hash, req.chunk)
                .await
                .map_err(internal)?;
        Ok(Response::new(GetDownloadUrlResponse { url, expires_at }))
    }

    async fn get_manifest(
        &self,
        request: Request<GetManifestRequest>,
    ) -> Result<Response<ManifestObject>, Status> {
        let identity = self
            .state
            .authenticate_metadata(&request)
            .await
            .map_err(internal)?;
        let req = request.into_inner();
        if identity.tenant_id != req.tenant_id {
            return Err(Status::permission_denied("tenant mismatch"));
        }
        let key = crate::storage::LocalFsStore::object_key(&req.tenant_id, &req.manifest_hash);
        let body = self.state.store.get(&key).await.map_err(internal)?;
        Ok(Response::new(ManifestObject { body }))
    }
}

/// Journal-op path extraction helper (fold job, M4).
#[must_use]
pub fn op_path(op: &OpKind) -> Option<String> {
    match op {
        OpKind::FileUpsert(o) => Some(o.path.clone()),
        OpKind::FileDelete(o) => Some(o.path.clone()),
        OpKind::Rename(r) => Some(r.new_path.clone()),
        OpKind::LeaseEvent(l) => Some(l.path.clone()),
    }
}

// ---------------- Leases ----------------

pub struct LeaseSvc {
    pub state: Arc<ServerState>,
}

#[tonic::async_trait]
impl cairn_proto::pb::lease_server::Lease for LeaseSvc {
    async fn acquire(
        &self,
        request: Request<cairn_proto::pb::AcquireRequest>,
    ) -> Result<Response<cairn_proto::pb::AcquireResponse>, Status> {
        let identity = self
            .state
            .authenticate_metadata(&request)
            .await
            .map_err(internal)?;
        let req = request.into_inner();
        if identity.tenant_id != req.tenant_id || identity.device_id != req.device_id {
            self.state.audit_denial(&identity, "lease.acquire").await;
            return Err(Status::permission_denied("device/tenant mismatch"));
        }
        let ttl = if req.ttl_ms == 0 { 60_000 } else { req.ttl_ms };
        let (token, expires_at) = crate::leases::acquire(
            &self.state.db,
            &self.state.clock,
            &req.tenant_id,
            &req.project_id,
            &req.path,
            &req.device_id,
            ttl,
        )
        .await
        .map_err(internal)?;
        Ok(Response::new(cairn_proto::pb::AcquireResponse {
            token,
            expires_at,
        }))
    }

    async fn renew(
        &self,
        request: Request<cairn_proto::pb::RenewRequest>,
    ) -> Result<Response<cairn_proto::pb::RenewResponse>, Status> {
        let identity = self
            .state
            .authenticate_metadata(&request)
            .await
            .map_err(internal)?;
        let req = request.into_inner();
        if identity.tenant_id != req.tenant_id || identity.device_id != req.device_id {
            return Err(Status::permission_denied("device/tenant mismatch"));
        }
        let expires_at = crate::leases::renew(
            &self.state.db,
            &self.state.clock,
            &req.tenant_id,
            &req.project_id,
            &req.path,
            &req.device_id,
            req.token,
            req.ttl_ms,
        )
        .await
        .map_err(internal)?;
        Ok(Response::new(cairn_proto::pb::RenewResponse { expires_at }))
    }

    async fn release(
        &self,
        request: Request<cairn_proto::pb::ReleaseRequest>,
    ) -> Result<Response<cairn_proto::pb::Ack>, Status> {
        let identity = self
            .state
            .authenticate_metadata(&request)
            .await
            .map_err(internal)?;
        let req = request.into_inner();
        if identity.tenant_id != req.tenant_id {
            return Err(Status::permission_denied("tenant mismatch"));
        }
        crate::leases::release(
            &self.state.db,
            &req.tenant_id,
            &req.project_id,
            &req.path,
            &req.device_id,
            req.token,
        )
        .await
        .map_err(internal)?;
        Ok(Response::new(cairn_proto::pb::Ack { ok: true }))
    }

    async fn list_leases(
        &self,
        request: Request<cairn_proto::pb::ListLeasesRequest>,
    ) -> Result<Response<cairn_proto::pb::ListLeasesResponse>, Status> {
        let identity = self
            .state
            .authenticate_metadata(&request)
            .await
            .map_err(internal)?;
        let req = request.into_inner();
        if identity.tenant_id != req.tenant_id {
            return Err(Status::permission_denied("tenant mismatch"));
        }
        let leases = crate::leases::list(
            &self.state.db,
            &self.state.clock,
            &req.tenant_id,
            &req.project_id,
        )
        .await
        .map_err(internal)?;
        Ok(Response::new(cairn_proto::pb::ListLeasesResponse {
            leases: leases
                .into_iter()
                .map(|l| cairn_proto::pb::LeaseInfo {
                    path: l.path,
                    device_id: l.device_id,
                    token: l.token,
                    expires_at: l.expires_at,
                })
                .collect(),
        }))
    }
}

// ---------------- Auth ----------------

pub struct AuthSvc {
    pub state: Arc<ServerState>,
}

#[tonic::async_trait]
impl cairn_proto::pb::auth_server::Auth for AuthSvc {
    async fn enroll_code(
        &self,
        request: Request<cairn_proto::pb::EnrollCodeRequest>,
    ) -> Result<Response<cairn_proto::pb::EnrollCodeResponse>, Status> {
        // admin action: requires an authenticated admin device OR bootstrap dev mode
        let authed = self
            .state
            .authenticate_metadata(&request)
            .await
            .ok()
            .filter(|i| i.scopes.contains("admin"));
        if authed.is_none() && !self.state.dev_insecure {
            return Err(Status::permission_denied("admin scope required"));
        }
        let req = request.into_inner();
        let code = self
            .state
            .auth
            .enroll_code(&req.tenant_id, &req.email, &req.scopes, 600_000)
            .await;
        crate::db::audit(
            &self.state.db,
            &self.state.clock,
            &req.tenant_id,
            authed
                .as_ref()
                .map_or("bootstrap", |i| i.device_id.as_str()),
            "admin.enroll_code",
            &req.email,
            &req.scopes,
        )
        .await;
        Ok(Response::new(cairn_proto::pb::EnrollCodeResponse {
            code,
            expires_at: self.state.clock.now_millis() + 600_000,
        }))
    }

    async fn enroll(
        &self,
        request: Request<cairn_proto::pb::EnrollRequest>,
    ) -> Result<Response<cairn_proto::pb::EnrollResponse>, Status> {
        let req = request.into_inner();
        let (paseto, identity) = self
            .state
            .auth
            .enroll(
                &self.state.db,
                &req.code,
                &req.device_pubkey,
                &req.device_name,
            )
            .await
            .map_err(internal)?;
        Ok(Response::new(cairn_proto::pb::EnrollResponse {
            paseto,
            expires_at: self.state.clock.now_millis() + 90 * 24 * 3600 * 1000,
            device_id: identity.device_id,
            tenant_id: identity.tenant_id,
        }))
    }

    async fn revoke(
        &self,
        request: Request<cairn_proto::pb::RevokeRequest>,
    ) -> Result<Response<cairn_proto::pb::Ack>, Status> {
        let identity = self
            .state
            .authenticate_metadata(&request)
            .await
            .map_err(internal)?;
        if !identity.scopes.contains("admin") {
            self.state.audit_denial(&identity, "auth.revoke").await;
            return Err(Status::permission_denied("admin scope required"));
        }
        let req = request.into_inner();
        self.state
            .auth
            .revoke(&self.state.db, &req.device_id)
            .await
            .map_err(internal)?;
        Ok(Response::new(cairn_proto::pb::Ack { ok: true }))
    }
}

// ---------------- Projects ----------------

pub struct ProjectSvc {
    pub state: Arc<ServerState>,
}

#[tonic::async_trait]
impl cairn_proto::pb::project_server::Project for ProjectSvc {
    async fn create_project(
        &self,
        request: Request<cairn_proto::pb::CreateProjectRequest>,
    ) -> Result<Response<cairn_proto::pb::ProjectInfo>, Status> {
        let identity = self
            .state
            .authenticate_metadata(&request)
            .await
            .map_err(internal)?;
        let req = request.into_inner();
        if identity.tenant_id != req.tenant_id {
            self.state.audit_denial(&identity, "project.create").await;
            return Err(Status::permission_denied("tenant mismatch"));
        }
        let now = self.state.clock.now_millis();
        sqlx::query(
            "INSERT INTO projects(tenant_id, project_id, name, created_at) VALUES(?1,?2,?3,?4)
             ON CONFLICT(tenant_id, project_id) DO NOTHING",
        )
        .bind(&req.tenant_id)
        .bind(&req.project_id)
        .bind(&req.name)
        .bind(now)
        .execute(&self.state.db)
        .await
        .map_err(|e| Status::internal(format!("project: {e}")))?;
        Ok(Response::new(cairn_proto::pb::ProjectInfo {
            tenant_id: req.tenant_id,
            project_id: req.project_id,
            name: req.name,
            next_lease_token: 0,
            fold_seq: 0,
        }))
    }

    async fn list_projects(
        &self,
        request: Request<cairn_proto::pb::ListProjectsServerRequest>,
    ) -> Result<Response<cairn_proto::pb::ListProjectsServerResponse>, Status> {
        let identity = self
            .state
            .authenticate_metadata(&request)
            .await
            .map_err(internal)?;
        let req = request.into_inner();
        if identity.tenant_id != req.tenant_id {
            return Err(Status::permission_denied("tenant mismatch"));
        }
        let rows = sqlx::query(
            "SELECT tenant_id, project_id, name, next_lease_token, fold_seq FROM projects WHERE tenant_id=?1",
        )
        .bind(&req.tenant_id)
        .fetch_all(&self.state.db)
        .await
        .map_err(|e| Status::internal(format!("projects: {e}")))?;
        use sqlx::Row;
        Ok(Response::new(cairn_proto::pb::ListProjectsServerResponse {
            projects: rows
                .into_iter()
                .map(|r| cairn_proto::pb::ProjectInfo {
                    tenant_id: r.get(0),
                    project_id: r.get(1),
                    name: r.get(2),
                    next_lease_token: r.get::<i64, _>(3).max(0) as u64,
                    fold_seq: r.get::<i64, _>(4).max(0) as u64,
                })
                .collect(),
        }))
    }

    async fn get_project(
        &self,
        request: Request<cairn_proto::pb::GetProjectRequest>,
    ) -> Result<Response<cairn_proto::pb::ProjectInfo>, Status> {
        let identity = self
            .state
            .authenticate_metadata(&request)
            .await
            .map_err(internal)?;
        let req = request.into_inner();
        if identity.tenant_id != req.tenant_id {
            return Err(Status::permission_denied("tenant mismatch"));
        }
        let row = sqlx::query(
            "SELECT tenant_id, project_id, name, next_lease_token, fold_seq FROM projects
             WHERE tenant_id=?1 AND project_id=?2",
        )
        .bind(&req.tenant_id)
        .bind(&req.project_id)
        .fetch_optional(&self.state.db)
        .await
        .map_err(|e| Status::internal(format!("project: {e}")))?;
        let Some(r) = row else {
            return Err(Status::not_found("project"));
        };
        use sqlx::Row;
        Ok(Response::new(cairn_proto::pb::ProjectInfo {
            tenant_id: r.get(0),
            project_id: r.get(1),
            name: r.get(2),
            next_lease_token: r.get::<i64, _>(3).max(0) as u64,
            fold_seq: r.get::<i64, _>(4).max(0) as u64,
        }))
    }
}
