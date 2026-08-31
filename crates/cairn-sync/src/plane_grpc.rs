//! Production plane: real tonic gRPC clients + presigned HTTP, bound to one device identity.
//! Every call carries `authorization: Bearer <paseto>` metadata; wire statuses are mapped
//! back through the structured ErrorDetail so `CONFLICT` / `STALE_LEASE` semantics survive
//! the round trip (the engine branches on them, §7.1/§8).
//!
//! Note on session checksums: `CreateUploadSession.checksums` binds presigned PUTs on real
//! S3 backends. The dev local-fs backend enforces the checksum at the bucket instead
//! (x-amz-checksum-sha256 header vs bytes), which is what WO1 exercises end-to-end.

use async_trait::async_trait;
use cairn_core::hash::Hash;
use cairn_core::{CairnError, ErrorKind};
use cairn_proto::pb::download_client::DownloadClient;
use cairn_proto::pb::journal_client::JournalClient;
use cairn_proto::pb::upload_client::UploadClient;
use cairn_proto::pb::UploadReceipt;
use tonic::metadata::MetadataValue;
use tonic::transport::{Channel, Endpoint};
use tonic::Request;

use crate::plane::{CompleteOut, Entry, Plane, Session};

/// Error-code string → ErrorKind (inverse of cairn-core `code()`; unknown codes stay
/// Internal with the server message preserved).
fn kind_for_code(code: &str) -> ErrorKind {
    match code {
        "CONFLICT" => ErrorKind::Conflict,
        "STALE_LEASE" => ErrorKind::StaleLease,
        "REF_CAS" => ErrorKind::RefCas,
        "UNAUTHENTICATED" => ErrorKind::Unauthenticated,
        "PERMISSION_DENIED" => ErrorKind::PermissionDenied,
        "NOT_FOUND" => ErrorKind::NotFound,
        "SESSION_EXPIRED" => ErrorKind::SessionExpired,
        "CHECKSUM_MISMATCH" => ErrorKind::ChecksumMismatch,
        "BATCH_TOO_LARGE" => ErrorKind::BatchTooLarge,
        "RATE_LIMITED" => ErrorKind::RateLimited,
        "UNAVAILABLE" => ErrorKind::Unavailable,
        "COMPACTION_REQUIRED" => ErrorKind::CompactionRequired,
        "SESSION_FULL" => ErrorKind::SessionFull,
        _ => ErrorKind::Internal,
    }
}

fn from_wire(s: &tonic::Status) -> CairnError {
    let d = cairn_proto::error_detail(s);
    CairnError::new(kind_for_code(&d.code), format!("{}: {}", d.code, d.message))
}

/// Shared endpoint dialer: http (plaintext) or https with optional custom CA.
pub async fn connect_channel(url: &str, ca_pem: Option<&[u8]>) -> Result<Channel, CairnError> {
    let mut endpoint = Endpoint::from_shared(url.to_string())
        .map_err(|e| CairnError::new(ErrorKind::Io, format!("bad server addr: {e}")))?
        .connect_timeout(std::time::Duration::from_secs(5))
        .tcp_nodelay(true);
    if url.starts_with("https://") {
        let mut tls = tonic::transport::ClientTlsConfig::new();
        if let Some(ca) = ca_pem {
            tls = tls.ca_certificate(tonic::transport::Certificate::from_pem(ca));
        }
        let host = url
            .trim_start_matches("https://")
            .split(':')
            .next()
            .unwrap_or("localhost");
        tls = tls.domain_name(host.to_string());
        endpoint = endpoint
            .tls_config(tls)
            .map_err(|e| CairnError::new(ErrorKind::Io, format!("client tls: {e}")))?;
    }
    endpoint
        .connect()
        .await
        .map_err(|e| CairnError::new(ErrorKind::Unavailable, format!("grpc connect: {e}")))
}

/// Plane over real gRPC. One shared `Channel` (HTTP/2 multiplexed); clients are cloned per
/// call (cheap handle clone, no reconnect).
pub struct GrpcPlane {
    channel: Channel,
    pub url: String,
    pub token: String,
    pub tenant_id: String,
    pub http: reqwest::Client,
}

impl GrpcPlane {
    /// Connect to the metadata server (`http(s)://host:port`); `tenant_id` is stamped into
    /// every payload that carries a tenant field (server cross-checks it against the token).
    /// `ca_pem` overrides the trust roots for `https://` endpoints (self-signed dev certs).
    pub async fn connect(
        url: &str,
        token: &str,
        tenant_id: &str,
        ca_pem: Option<&[u8]>,
    ) -> Result<Self, CairnError> {
        let channel = connect_channel(url, ca_pem).await?;
        Ok(GrpcPlane {
            channel,
            url: url.to_string(),
            token: token.to_string(),
            tenant_id: tenant_id.to_string(),
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(120))
                .build()
                .map_err(|e| CairnError::new(ErrorKind::Io, format!("http client: {e}")))?,
        })
    }

    fn authed<T>(&self, inner: T) -> Result<Request<T>, CairnError> {
        let mut req = Request::new(inner);
        // bounded wire waits (ADR-0010): a hung call must surface as a retryable error,
        // never park the sync loop forever. Loops re-enter; the engine retries.
        req.set_timeout(std::time::Duration::from_secs(30));
        let v = MetadataValue::try_from(format!("Bearer {}", self.token))
            .map_err(|e| CairnError::new(ErrorKind::Internal, format!("auth header: {e}")))?;
        req.metadata_mut().insert("authorization", v);
        Ok(req)
    }
}

#[async_trait]
impl Plane for GrpcPlane {
    async fn batch_exists(
        &self,
        tenant: &str,
        hashes: &[String],
    ) -> Result<Vec<String>, CairnError> {
        let mut c = UploadClient::new(self.channel.clone());
        let req = self.authed(cairn_proto::pb::BatchExistsRequest {
            tenant_id: tenant.into(),
            chunk_hashes: hashes.to_vec(),
        })?;
        let out = c.batch_exists(req).await.map_err(|s| from_wire(&s))?;
        Ok(out.into_inner().missing)
    }

    async fn create_session(
        &self,
        tenant: &str,
        device: &str,
        project: &str,
        missing: &[String],
    ) -> Result<Session, CairnError> {
        let mut c = UploadClient::new(self.channel.clone());
        let req = self.authed(cairn_proto::pb::CreateUploadSessionRequest {
            tenant_id: tenant.into(),
            device_id: device.into(),
            project_id: project.into(),
            missing: missing.to_vec(),
            checksums: vec![], // dev backend verifies at the bucket (see crate docs)
        })?;
        let out = c
            .create_upload_session(req)
            .await
            .map_err(|s| from_wire(&s))?;
        let inner = out.into_inner();
        let expires_at = inner.puts.first().map(|p| p.expires_at).unwrap_or_default();
        Ok(Session {
            id: inner.session_id,
            puts: inner
                .puts
                .into_iter()
                .map(|p| (p.chunk_hash, p.url))
                .collect(),
            expires_at,
        })
    }

    async fn complete(
        &self,
        session: &str,
        receipts: &[UploadReceipt],
    ) -> Result<CompleteOut, CairnError> {
        let mut c = UploadClient::new(self.channel.clone());
        let req = self.authed(cairn_proto::pb::CompleteUploadRequest {
            session_id: session.into(),
            tenant_id: self.tenant_id.clone(),
            receipts: receipts.to_vec(),
        })?;
        let out = c.complete_upload(req).await.map_err(|s| from_wire(&s))?;
        let inner = out.into_inner();
        Ok(CompleteOut {
            verified: inner.verified,
            rejected: inner.rejected,
        })
    }

    async fn put_presigned(
        &self,
        url: &str,
        bytes: &[u8],
        checksum_hex: &str,
    ) -> Result<(), CairnError> {
        let r = self
            .http
            .put(url)
            .header("x-amz-checksum-sha256", checksum_hex)
            .body(bytes.to_vec())
            .send()
            .await;
        match r {
            Ok(resp) if resp.status().is_success() => Ok(()),
            Ok(resp) => Err(CairnError::new(
                ErrorKind::Unavailable,
                format!("presigned PUT: HTTP {}", resp.status()),
            )),
            Err(e) => Err(CairnError::new(
                ErrorKind::Unavailable,
                format!("presigned PUT: {e}"),
            )),
        }
    }

    async fn put_manifest(
        &self,
        tenant: &str,
        manifest_hash: &str,
        bytes: &[u8],
    ) -> Result<(), CairnError> {
        if Hash::of(bytes).hex() != manifest_hash {
            return Err(CairnError::new(
                ErrorKind::ChecksumMismatch,
                "manifest hash mismatch",
            ));
        }
        let mut c = UploadClient::new(self.channel.clone());
        let req = self.authed(cairn_proto::pb::RegisterManifestRequest {
            tenant_id: tenant.into(),
            manifest_hash: manifest_hash.into(),
            body: bytes.to_vec(),
        })?;
        c.register_manifest(req).await.map_err(|s| from_wire(&s))?;
        Ok(())
    }

    async fn get_manifest(&self, tenant: &str, manifest_hash: &str) -> Result<Vec<u8>, CairnError> {
        let mut c = DownloadClient::new(self.channel.clone());
        let req = self.authed(cairn_proto::pb::GetManifestRequest {
            tenant_id: tenant.into(),
            manifest_hash: manifest_hash.into(),
        })?;
        let out = c.get_manifest(req).await.map_err(|s| from_wire(&s))?;
        Ok(out.into_inner().body)
    }

    async fn fetch_object(&self, tenant: &str, hash_hex: &str) -> Result<Vec<u8>, CairnError> {
        let mut c = DownloadClient::new(self.channel.clone());
        let req = self.authed(cairn_proto::pb::GetDownloadUrlRequest {
            tenant_id: tenant.into(),
            manifest_hash: hash_hex.into(),
            path: String::new(),
            chunk: true, // hydration fetches stored CHUNKS (chunk-key namespace)
        })?;
        let out = c.get_download_url(req).await.map_err(|s| from_wire(&s))?;
        let url = out.into_inner().url;
        let r = self.http.get(&url).send().await;
        match r {
            Ok(resp) if resp.status().is_success() => {
                let bytes = resp.bytes().await.map_err(|e| {
                    CairnError::new(ErrorKind::Unavailable, format!("object GET: {e}"))
                })?;
                Ok(bytes.to_vec())
            }
            Ok(resp) => Err(CairnError::new(
                ErrorKind::Unavailable,
                format!("object GET: HTTP {}", resp.status()),
            )),
            Err(e) => Err(CairnError::new(
                ErrorKind::Unavailable,
                format!("object GET: {e}"),
            )),
        }
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
        let mut c = JournalClient::new(self.channel.clone());
        let req = self.authed(cairn_proto::pb::AppendRequest {
            tenant_id: tenant.into(),
            project_id: project.into(),
            device_id: device.into(),
            request_id: request_id.into(),
            op: Some(op),
            lease_token,
        })?;
        let out = c.append(req).await.map_err(|s| from_wire(&s))?;
        let inner = out.into_inner();
        Ok((inner.seq, inner.deduplicated))
    }

    async fn fetch_batch(
        &self,
        tenant: &str,
        project: &str,
        after: u64,
        limit: u32,
    ) -> Result<Vec<Entry>, CairnError> {
        let mut c = JournalClient::new(self.channel.clone());
        let req = self.authed(cairn_proto::pb::FetchBatchRequest {
            tenant_id: tenant.into(),
            project_id: project.into(),
            after,
            limit: limit.clamp(1, 512),
        })?;
        let out = c.fetch_batch(req).await.map_err(|s| from_wire(&s))?;
        Ok(out
            .into_inner()
            .entries
            .into_iter()
            .map(|e| Entry {
                seq: e.seq,
                device_id: e.device_id,
                op: e.op.clone().unwrap_or_default(),
                server_ts: e.server_ts,
            })
            .collect())
    }
}
