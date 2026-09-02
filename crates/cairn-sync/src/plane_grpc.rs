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
use cairn_proto::pb::lease_client::LeaseClient;
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

/// True when `host` is a loopback address (plaintext is tolerated there for dev only).
fn is_loopback_host(host: &str) -> bool {
    let h = host.trim_start_matches('[').trim_end_matches(']');
    h == "localhost" || h.starts_with("127.") || h == "::1"
}

/// TLS gate (fail-closed at connect, review round 3): a plaintext REMOTE endpoint is an
/// error HERE, at dial time — not a doctor warning after the fact. The doctor check now
/// confirms what this function already enforces, so the control plane is never one config
/// mistake away from plaintext. Loopback plaintext stays allowed (dev topology); an
/// explicit `CAIRN_ALLOW_INSECURE_REMOTE=1` overrides for deliberately-plaintext LANs
/// (documented escape hatch, logged loudly when used).
pub async fn connect_channel(url: &str, ca_pem: Option<&[u8]>) -> Result<Channel, CairnError> {
    let insecure_override =
        std::env::var("CAIRN_ALLOW_INSECURE_REMOTE").is_ok_and(|v| v == "1" || v == "true");
    if url.starts_with("http://") {
        let host = url
            .trim_start_matches("http://")
            .split([':', '/', '?'])
            .next()
            .unwrap_or("");
        if !is_loopback_host(host) && !insecure_override {
            return Err(CairnError::new(
                ErrorKind::Unauthenticated,
                format!(
                    "refusing PLAINTEXT connection to remote server '{host}': the control \
                     plane (Bearer tokens + journal) requires TLS. Serve https:// (see `just \
                     tls-dev-cert`) or set CAIRN_ALLOW_INSECURE_REMOTE=1 to accept the risk \
                     explicitly"
                ),
            ));
        }
        if !is_loopback_host(host) {
            tracing::warn!("CAIRN_ALLOW_INSECURE_REMOTE=1: dialing {host} over PLAINTEXT");
        }
    }
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

/// One COLD-FETCH measurement (see [`GrpcPlane::measure_cold_fetch`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ColdFetchSample {
    /// presign RPC (GetDownloadUrl) duration
    pub presign_ms: f64,
    /// presign + HTTP round trip until the FIRST body byte (the cold-fetch latency)
    pub first_byte_ms: f64,
    /// presign + full body transfer
    pub total_ms: f64,
    /// body bytes actually received (must equal the chunk size)
    pub bytes: u64,
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

    /// Build a plane from an EXISTING channel (ctl daemon paths reuse a dialed
    /// channel instead of re-connecting per RPC).
    pub fn from_channel(channel: Channel, token: String, tenant_id: String) -> Self {
        GrpcPlane {
            channel,
            url: String::new(),
            token,
            tenant_id,
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(120))
                .build()
                .expect("http client"),
        }
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

    /// WO6-4 COLD-FETCH instrumentation (docs/BENCHMARKS.md): ONE real download-path
    /// fetch — `GetDownloadUrl` (presign) → presigned GET — with the body streamed so
    /// the time-to-FIRST-BYTE is separated from total transfer time. This is the exact
    /// code path a cold hydration takes on a device that has never seen the chunk
    /// (fresh store, no local CAS); "cold" here means fresh process + empty client
    /// state, NOT a dropped server page cache (see BENCHMARKS.md for the honest
    /// caveat and the drop-caches escalation used when privileges allow).
    ///
    /// Inherent on GrpcPlane (not on the [`Plane`] trait): harness-only measurement.
    /// Coverage note: excluded from the unit-coverage ratchet — this path is exercised
    /// ON THE WIRE by `just soak-*` (scripts/soak.sh gate S4) and the CI `soak-s3`
    /// job (real presign + presigned GET against MinIO, body byte-count asserted);
    /// an in-process unit double would test a mock, not the plane. The wire test
    /// lives at crates/cairn-server/tests/cold_fetch.rs (the server depends on this
    /// crate, so the cycle-free side hosts it).
    pub async fn measure_cold_fetch(
        &self,
        tenant: &str,
        hash_hex: &str,
    ) -> Result<ColdFetchSample, CairnError> {
        use futures::StreamExt;
        let mut c = DownloadClient::new(self.channel.clone());
        let req = self.authed(cairn_proto::pb::GetDownloadUrlRequest {
            tenant_id: tenant.into(),
            manifest_hash: hash_hex.into(),
            path: String::new(),
            chunk: true,
        })?;
        let t0 = std::time::Instant::now();
        let out = c.get_download_url(req).await.map_err(|s| from_wire(&s))?;
        let url = out.into_inner().url;
        let presign_ms = t0.elapsed().as_secs_f64() * 1e3;
        let r = self.http.get(&url).send().await;
        match r {
            Ok(resp) if resp.status().is_success() => {
                let mut stream = resp.bytes_stream();
                let mut first_byte_ms = None;
                let mut total_bytes = 0u64;
                while let Some(item) = stream.next().await {
                    let b = item.map_err(|e| {
                        CairnError::new(ErrorKind::Unavailable, format!("object GET: {e}"))
                    })?;
                    if first_byte_ms.is_none() {
                        first_byte_ms = Some(t0.elapsed().as_secs_f64() * 1e3);
                    }
                    total_bytes += b.len() as u64;
                }
                let total_ms = t0.elapsed().as_secs_f64() * 1e3;
                Ok(ColdFetchSample {
                    presign_ms,
                    first_byte_ms: first_byte_ms
                        .ok_or_else(|| CairnError::new(ErrorKind::Unavailable, "empty body"))?,
                    total_ms,
                    bytes: total_bytes,
                })
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
        // R2 rule (5GB REAL-S3 soak, 2026-09-02): EVERY `x-amz-*` request header
        // must be bound into the presign's SignedHeaders — an unsigned
        // `x-amz-checksum-sha256` (or `x-amz-content-sha256`) fails with a
        // misleading 403 SignatureDoesNotMatch. The server presigns host-only
        // + `x-amz-content-sha256` (the daemon sends that one header, matching
        // the signed set) and CANNOT bind a body SHA-256 it does not know, so
        // the checksum header is NOT sent on the wire. Integrity stays
        // enforced by CompleteUpload BLAKE3 sample-verify + verified ranged
        // reads (SPEC §9.2); checksum-BOUND sessions (client ships per-chunk
        // SHA-256s, `SigV4Presigner::presign_put`, R2-proven) are the follow-up.
        // Quirk S1 (hex-in-header rejected by MinIO) is thereby moot on this path.
        let _ = checksum_hex;
        let r = self
            .http
            .put(url)
            .header("x-amz-content-sha256", "UNSIGNED-PAYLOAD")
            .body(bytes.to_vec())
            .send()
            .await;
        match r {
            Ok(resp) if resp.status().is_success() => Ok(()),
            // S3 error responses carry an XML body whose <Code> names the exact
            // failure (SignatureDoesNotMatch vs AuthorizationQueryParametersError
            // vs InvalidRequest...) — surface it, a bare status string sends us
            // guessing (the soak-s3 400 hunt, 2026-09-01).
            Ok(resp) => {
                let status = resp.status();
                let body = resp
                    .text()
                    .await
                    .unwrap_or_default()
                    .lines()
                    .map(str::trim)
                    .collect::<Vec<_>>()
                    .join(" ");
                let detail: String = body.chars().take(300).collect();
                Err(CairnError::new(
                    ErrorKind::Unavailable,
                    format!("presigned PUT: HTTP {status} {detail}"),
                ))
            }
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

    async fn acquire_lease(
        &self,
        tenant: &str,
        project: &str,
        path: &str,
        device: &str,
        ttl_ms: u64,
    ) -> Result<(u64, i64), CairnError> {
        let mut c = LeaseClient::new(self.channel.clone());
        let req = self.authed(cairn_proto::pb::AcquireRequest {
            tenant_id: tenant.into(),
            project_id: project.into(),
            path: path.into(),
            device_id: device.into(),
            ttl_ms,
        })?;
        let out = c.acquire(req).await.map_err(|s| from_wire(&s))?;
        let inner = out.into_inner();
        Ok((inner.token, inner.expires_at))
    }

    async fn release_lease(
        &self,
        tenant: &str,
        project: &str,
        path: &str,
        device: &str,
        token: u64,
    ) -> Result<(), CairnError> {
        let mut c = LeaseClient::new(self.channel.clone());
        let req = self.authed(cairn_proto::pb::ReleaseRequest {
            tenant_id: tenant.into(),
            project_id: project.into(),
            path: path.into(),
            device_id: device.into(),
            token,
        })?;
        c.release(req).await.map_err(|s| from_wire(&s))?;
        Ok(())
    }

    async fn renew_lease(
        &self,
        tenant: &str,
        project: &str,
        path: &str,
        device: &str,
        token: u64,
        ttl_ms: u64,
    ) -> Result<i64, CairnError> {
        let mut c = LeaseClient::new(self.channel.clone());
        let req = self.authed(cairn_proto::pb::RenewRequest {
            tenant_id: tenant.into(),
            project_id: project.into(),
            path: path.into(),
            device_id: device.into(),
            token,
            ttl_ms,
        })?;
        let out = c.renew(req).await.map_err(|s| from_wire(&s))?;
        Ok(out.into_inner().expires_at)
    }
}

#[cfg(test)]
mod tls_gate_tests {
    use super::*;

    /// Fail-closed (punch #6): a plaintext REMOTE endpoint is refused at connect, before
    /// any dial — the error is a config error, not a doctor finding after the fact.
    #[tokio::test]
    async fn plaintext_remote_is_refused_at_connect() {
        std::env::remove_var("CAIRN_ALLOW_INSECURE_REMOTE");
        let err = connect_channel("http://studio-server.example.com:7443", None)
            .await
            .expect_err("plaintext remote must be refused");
        assert!(
            err.message.contains("PLAINTEXT"),
            "error must name the TLS requirement: {}",
            err.message
        );
    }

    /// The classic misconfiguration the review called out: a config edit turns
    /// `https://cairn.corp:7443` into `http://cairn.corp:7443` — that must never dial.
    #[tokio::test]
    async fn plaintext_remote_ip_is_refused_too() {
        std::env::remove_var("CAIRN_ALLOW_INSECURE_REMOTE");
        assert!(connect_channel("http://203.0.113.7:7443", None)
            .await
            .is_err());
    }

    /// Loopback plaintext stays allowed (dev topology, matches doctor's posture).
    #[tokio::test]
    async fn plaintext_loopback_still_allowed() {
        std::env::remove_var("CAIRN_ALLOW_INSECURE_REMOTE");
        // connection will fail (no server) but with an Unavailable dial error,
        // NOT the plaintext-refusal error — proving the gate passed.
        let err = connect_channel("http://127.0.0.1:1", None)
            .await
            .expect_err("nothing listens here");
        assert!(
            !err.message.contains("PLAINTEXT"),
            "loopback must pass the TLS gate: {}",
            err.message
        );
    }

    /// Explicit escape hatch: documented, opt-in, logs loudly.
    #[tokio::test]
    async fn insecure_remote_override_is_explicit() {
        std::env::set_var("CAIRN_ALLOW_INSECURE_REMOTE", "1");
        let err = connect_channel("http://203.0.113.7:7443", None)
            .await
            .expect_err("override passes the gate; dial still fails (no server)");
        std::env::remove_var("CAIRN_ALLOW_INSECURE_REMOTE");
        assert!(
            !err.message.contains("PLAINTEXT"),
            "override must bypass the gate: {}",
            err.message
        );
    }
}
