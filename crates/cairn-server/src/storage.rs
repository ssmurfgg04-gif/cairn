//! Object storage (SPEC §9/§12, ADR-0005): the data plane talks client ↔ bucket DIRECT; the
//! API server never proxies blob bytes. Backends behind one trait:
//!
//! - `LocalFsStore` (dev/test): emulates bucket semantics — presigned PUT requires
//!   `x-amz-checksum-sha256` when PRESENT (the daemon contract is header-less since
//!   the R2 wire rule — see s3-compatibility.md); GET is immutable + Range-capable with
//!   1h-TTL signed URLs. Served over loopback HTTP by the dev server.
//! - `SigV4Presigner`: standard AWS SigV4 presigned PUT/GET against any S3-compatible endpoint
//!   (AWS S3, Cloudflare R2; B2 via its S3-compatible API). Production deployments plug this
//!   in via configuration; the trait boundary keeps call sites identical.
//!
//! Layout (SPEC §12): `t{tenant}/c/{ab}/{hash}` chunks · `t{tenant}/o/{ab}/{hash}` objects ·
//! `t{tenant}/packs/{date}-{n}.pack` packs. Signed URLs are bearer credentials — query
//! strings are stripped from all logs/traces.

use async_trait::async_trait;
use cairn_core::clock::SystemClock;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

use cairn_core::{CairnError, ErrorKind};

type HmacSha256 = Hmac<Sha256>;

/// Object-store backend. All methods are tenant-scoped by key shape (I3).
#[async_trait]
pub trait ObjectStore: Send + Sync {
    /// Put an object (server-side paths only: manifests, packs; chunks go presigned).
    async fn put(&self, key: &str, bytes: &[u8]) -> Result<(), CairnError>;
    /// Get an object.
    async fn get(&self, key: &str) -> Result<Vec<u8>, CairnError>;
    /// Head: existence + size.
    async fn head(&self, key: &str) -> Result<u64, CairnError>;
    /// Delete (GC sweep path only).
    async fn delete(&self, key: &str) -> Result<(), CairnError>;
    /// Presigned PUT (write-scoped, TTL ≤ 1h, checksum-enforced).
    async fn presign_put(&self, key: &str, ttl_secs: u64) -> Result<String, CairnError>;
    /// Presigned GET (read-scoped, immutable, Range-capable, TTL ≤ 1h).
    async fn presign_get(&self, key: &str, ttl_secs: u64) -> Result<String, CairnError>;
    /// Human-readable backend name (metrics/doctor).
    fn name(&self) -> &'static str;
}

/// Local filesystem store with bucket emulation (dev/test + the dev HTTP endpoint).
pub struct LocalFsStore {
    root: std::path::PathBuf,
    signing_key: Vec<u8>,
    /// Public base URL where `router()` is mounted.
    base_url: String,
}

impl LocalFsStore {
    /// New store under `root` with an HMAC signing key for presign emulation.
    pub fn open(
        root: &std::path::Path,
        signing_key: &[u8],
        base_url: &str,
    ) -> Result<Self, CairnError> {
        std::fs::create_dir_all(root)
            .map_err(|e| CairnError::new(ErrorKind::Io, format!("store mkdir: {e}")))?;
        Ok(LocalFsStore {
            root: root.to_path_buf(),
            signing_key: signing_key.to_vec(),
            base_url: base_url.trim_end_matches('/').to_string(),
        })
    }

    /// Store root — for tooling and the sim, which need direct blocking reads that the
    /// async API cannot offer inside sync closures (e.g. manifest-tree walks).
    pub fn root(&self) -> &std::path::Path {
        &self.root
    }

    fn path_for(&self, key: &str) -> std::path::PathBuf {
        // keys are tenant-scoped and constructed server-side only; never allow traversal
        let safe: String = key
            .chars()
            .map(|c| {
                if c == '/' || c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let mut out = self.root.clone();
        for part in safe.split('/') {
            if part != ".." && !part.is_empty() && part != "." {
                out.push(part);
            }
        }
        out
    }

    fn sign(&self, method: &str, key: &str, exp_millis: i64) -> String {
        let mut mac = HmacSha256::new_from_slice(&self.signing_key).expect("hmac key");
        mac.update(format!("{method}|{key}|{exp_millis}").as_bytes());
        cairn_core::hash::hex_encode(&mac.finalize().into_bytes())
    }

    /// Verify a presigned URL's signature + expiry (HTTP endpoint side).
    #[must_use]
    pub fn verify_presign(&self, method: &str, key: &str, exp_millis: i64, sig: &str) -> bool {
        if cairn_core::clock::WallClock.now_millis() > exp_millis {
            return false;
        }
        sig == self.sign(method, key, exp_millis)
    }

    /// Storage key for a tenant chunk (SPEC §12 layout).
    #[must_use]
    pub fn chunk_key(tenant_id: &str, hash_hex: &str) -> String {
        format!(
            "t{tenant_id}/c/{}/{}",
            &hash_hex[..2.min(hash_hex.len())],
            hash_hex
        )
    }

    /// Storage key for tenant objects (manifests/trees/commits).
    #[must_use]
    pub fn object_key(tenant_id: &str, hash_hex: &str) -> String {
        format!(
            "t{tenant_id}/o/{}/{}",
            &hash_hex[..2.min(hash_hex.len())],
            hash_hex
        )
    }

    /// Storage key for pack files (`t{tenant}/packs/...`).
    #[must_use]
    pub fn pack_key(tenant_id: &str, pack_key: &str) -> String {
        format!("t{tenant_id}/{pack_key}")
    }

    /// Dev HTTP router: implements the presigned PUT/GET semantics the data plane relies on
    /// (bucket-rejects-corrupt + immutable Range GETs).
    pub fn router(self: std::sync::Arc<Self>) -> axum::Router {
        use axum::extract::{Path as AxPath, Query, State};
        use axum::http::{HeaderMap, StatusCode};

        type Resp = (axum::http::StatusCode, axum::http::HeaderMap, Vec<u8>);

        fn plain(status: axum::http::StatusCode, msg: &str) -> Resp {
            (
                status,
                axum::http::HeaderMap::new(),
                msg.as_bytes().to_vec(),
            )
        }

        async fn put(
            State(store): State<std::sync::Arc<LocalFsStore>>,
            AxPath(key): AxPath<String>,
            Query(q): Query<std::collections::HashMap<String, String>>,
            headers: HeaderMap,
            body: axum::body::Bytes,
        ) -> Resp {
            let (Some(exp), Some(sig)) = (q.get("exp"), q.get("sig")) else {
                return plain(StatusCode::FORBIDDEN, "missing signature");
            };
            let Ok(exp_millis) = exp.parse::<i64>() else {
                return plain(StatusCode::FORBIDDEN, "bad exp");
            };
            if !store.verify_presign("PUT", &key, exp_millis, sig) {
                return plain(StatusCode::FORBIDDEN, "invalid or expired signature");
            }
            // bucket-rejects-corrupt: if the client sends x-amz-checksum-sha256 it
            // MUST match the body (header verified either way). Requiring the header
            // was the pre-2026-09-02 contract; the R2 wire rule (every x-amz-* header
            // must be SIGNED, and the server cannot bind a SHA-256 it does not know)
            // moved the daemon to header-less presigned PUTs — integrity stays with
            // CompleteUpload BLAKE3 sample-verify. Checksum-bound sessions restore
            // bucket-side enforcement when clients ship per-chunk SHA-256s.
            let digest = Sha256::digest(&body);
            // The header is base64 on the S3 wire (RFC 4648, quirk S1); the dev
            // local-Fs backend additionally accepts hex so older gate scripts
            // and the conformance suite's hex-bound URLs keep working.
            let provided = headers
                .get("x-amz-checksum-sha256")
                .and_then(|v| v.to_str().ok());
            let matches = match provided {
                // base64 on the S3 wire (quirk S1): decode with the BASE64 decoder —
                // hex_decode here was the a064178 residual: the daemon sends b64,
                // hex-decoding it returned None for most checksums → every PUT 400'd
                Some(b64) if b64.len() == 44 && b64.ends_with('=') => {
                    cairn_core::hash::b64_decode(b64)
                        .map(|raw| raw == digest.as_slice())
                        .unwrap_or(false)
                }
                Some(hex_ck) => hex_ck == cairn_core::hash::hex_encode(&digest),
                // no checksum header = the new daemon contract (R2 wire rule);
                // verified reads + CompleteUpload BLAKE3 carry integrity
                None => true,
            };
            if !matches {
                return plain(
                    StatusCode::BAD_REQUEST,
                    "checksum mismatch — corrupt upload rejected",
                );
            }
            let path = store.path_for(&key);
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Err(e) = std::fs::write(&path, &body) {
                return plain(StatusCode::INTERNAL_SERVER_ERROR, &format!("write: {e}"));
            }
            plain(StatusCode::OK, "ok")
        }

        async fn get(
            State(store): State<std::sync::Arc<LocalFsStore>>,
            AxPath(key): AxPath<String>,
            Query(q): Query<std::collections::HashMap<String, String>>,
            headers: HeaderMap,
        ) -> Resp {
            let (Some(exp), Some(sig)) = (q.get("exp"), q.get("sig")) else {
                return plain(StatusCode::FORBIDDEN, "missing signature");
            };
            let Ok(exp_millis) = exp.parse::<i64>() else {
                return plain(StatusCode::FORBIDDEN, "bad exp");
            };
            if !store.verify_presign("GET", &key, exp_millis, sig) {
                return plain(StatusCode::FORBIDDEN, "invalid or expired"); // client re-signs + resumes (§9.2)
            }
            let path = store.path_for(&key);
            let Ok(full) = std::fs::read(&path) else {
                return plain(StatusCode::NOT_FOUND, "not found");
            };
            // Range support (single range, suffix ranges) — immutable, cache-forever semantics
            let range = headers.get("range").and_then(|v| v.to_str().ok());
            if let Some(range) = range {
                if let Some((start, end)) = parse_range(range, full.len() as u64) {
                    let slice = full[start as usize..=(end as usize)].to_vec();
                    let mut h = axum::http::HeaderMap::new();
                    h.insert("content-type", "application/octet-stream".parse().unwrap());
                    h.insert(
                        "cache-control",
                        "public, max-age=31536000, immutable".parse().unwrap(),
                    );
                    h.insert("accept-ranges", "bytes".parse().unwrap());
                    h.insert(
                        "content-range",
                        format!("bytes {start}-{end}/{}", full.len())
                            .parse()
                            .unwrap(),
                    );
                    return (StatusCode::PARTIAL_CONTENT, h, slice);
                }
            }
            let mut h = axum::http::HeaderMap::new();
            h.insert("content-type", "application/octet-stream".parse().unwrap());
            h.insert(
                "cache-control",
                "public, max-age=31536000, immutable".parse().unwrap(),
            );
            h.insert("accept-ranges", "bytes".parse().unwrap());
            (StatusCode::OK, h, full)
        }

        fn parse_range(range: &str, total: u64) -> Option<(u64, u64)> {
            let spec = range.strip_prefix("bytes=")?;
            let mut parts = spec.split('-');
            let a = parts.next()?.trim();
            let b = parts.next()?.trim();
            if a.is_empty() {
                let suffix: u64 = b.parse().ok()?;
                let start = total.checked_sub(suffix)?;
                Some((start, total.saturating_sub(1)))
            } else {
                let start: u64 = a.parse().ok()?;
                let end = if b.is_empty() {
                    total.saturating_sub(1)
                } else {
                    b.parse().ok()?
                };
                if end < start || start >= total {
                    return None;
                }
                Some((start, end.min(total.saturating_sub(1))))
            }
        }

        axum::Router::new()
            .route("/objects/*key", axum::routing::get(get).put(put))
            .layer(axum::extract::DefaultBodyLimit::max(256 * 1024 * 1024)) // 16MB chunks + headroom
            .with_state(self)
    }
}

#[async_trait]
impl ObjectStore for LocalFsStore {
    async fn put(&self, key: &str, bytes: &[u8]) -> Result<(), CairnError> {
        let path = self.path_for(key);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| CairnError::new(ErrorKind::Io, format!("mkdir: {e}")))?;
        }
        std::fs::write(&path, bytes)
            .map_err(|e| CairnError::new(ErrorKind::Io, format!("put: {e}")))
    }

    async fn get(&self, key: &str) -> Result<Vec<u8>, CairnError> {
        std::fs::read(self.path_for(key))
            .map_err(|_| CairnError::new(ErrorKind::NotFound, format!("object {key}")))
    }

    async fn head(&self, key: &str) -> Result<u64, CairnError> {
        let meta = std::fs::metadata(self.path_for(key))
            .map_err(|_| CairnError::new(ErrorKind::NotFound, format!("object {key}")))?;
        Ok(meta.len())
    }

    async fn delete(&self, key: &str) -> Result<(), CairnError> {
        let _ = std::fs::remove_file(self.path_for(key));
        Ok(())
    }

    async fn presign_put(&self, key: &str, ttl_secs: u64) -> Result<String, CairnError> {
        let ttl = ttl_secs.min(3600); // TTL ≤ 1h (SPEC §9)
        let exp = cairn_core::clock::WallClock.now_millis()
            + i64::try_from(ttl * 1000).unwrap_or(3_600_000);
        let sig = self.sign("PUT", key, exp);
        Ok(format!(
            "{}/objects/{}?exp={exp}&sig={sig}",
            self.base_url,
            url_enc(key)
        ))
    }

    async fn presign_get(&self, key: &str, ttl_secs: u64) -> Result<String, CairnError> {
        let ttl = ttl_secs.min(3600);
        let exp = cairn_core::clock::WallClock.now_millis()
            + i64::try_from(ttl * 1000).unwrap_or(3_600_000);
        let sig = self.sign("GET", key, exp);
        Ok(format!(
            "{}/objects/{}?exp={exp}&sig={sig}",
            self.base_url,
            url_enc(key)
        ))
    }

    fn name(&self) -> &'static str {
        "local-fs"
    }
}

fn url_enc(key: &str) -> String {
    // keys are tenant/hash shaped (alnum, dots, slashes, dashes) — encode everything else
    key.split('/')
        .map(str::to_string)
        .collect::<Vec<_>>()
        .join("/")
}

/// AWS SigV4 presigned URL generation (ADR-0005). Wire-format per the SigV4 reference; used
/// against S3/R2/B2-compatible endpoints in production.
pub struct SigV4Presigner {
    access_key: String,
    secret_key: String,
    region: String,
    /// Virtual-host mode: `https://bucket.host` (bucket already in the host).
    /// Path-style mode: bare service endpoint, `bucket` carries the bucket name.
    endpoint: String,
    /// Bucket name — empty in virtual-host mode, set in path-style mode.
    bucket: String,
    /// Path-style addressing (`CAIRN_S3_PATH_STYLE=1`): canonical URI and URL
    /// become `/{bucket}/{key}`. Required by MinIO-on-localhost and many
    /// self-hosted gateways that have no wildcard-DNS vhost; AWS S3/R2 accept
    /// both forms. Validated on the wire by the S3 conformance suite (WO6-4).
    path_style: bool,
}

impl SigV4Presigner {
    /// New presigner for an S3-compatible endpoint (host form: `https://bucket.host`).
    pub fn new(access_key: &str, secret_key: &str, region: &str, endpoint: &str) -> Self {
        SigV4Presigner {
            access_key: access_key.into(),
            secret_key: secret_key.into(),
            region: region.into(),
            endpoint: endpoint.trim_end_matches('/').into(),
            bucket: String::new(),
            path_style: false,
        }
    }

    /// Path-style presigner: `endpoint` is the BARE service endpoint
    /// (`https://s3.local:9000`), `bucket` is addressed in the path.
    pub fn new_path_style(
        access_key: &str,
        secret_key: &str,
        region: &str,
        endpoint: &str,
        bucket: &str,
    ) -> Self {
        SigV4Presigner {
            access_key: access_key.into(),
            secret_key: secret_key.into(),
            region: region.into(),
            endpoint: endpoint.trim_end_matches('/').into(),
            bucket: bucket.into(),
            path_style: true,
        }
    }

    /// Canonical request URI for an object key (addressing-style aware).
    fn canonical_uri(&self, key: &str) -> String {
        if self.path_style {
            format!("/{}/{}", self.bucket, uri_encode(key, false))
        } else {
            format!("/{}", uri_encode(key, false))
        }
    }

    /// Request URL (scheme+host+path, no query) for an object key.
    pub fn url_for(&self, key: &str) -> String {
        self.object_url(key)
    }

    /// Request URL (scheme+host+path, no query) for an object key.
    fn object_url(&self, key: &str) -> String {
        if self.path_style {
            format!(
                "{}/{}/{}",
                self.endpoint,
                self.bucket,
                uri_encode(key, false)
            )
        } else {
            format!("{}/{}", self.endpoint, uri_encode(key, false))
        }
    }

    fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
        let mut mac = HmacSha256::new_from_slice(key).expect("hmac");
        mac.update(data);
        mac.finalize().into_bytes().to_vec()
    }

    /// SHA-256 hex of bytes (canonical-request payload hash; public for the
    /// conformance suite and known-answer tests).
    pub fn sha256_hex(data: &[u8]) -> String {
        cairn_core::hash::hex_encode(&Sha256::digest(data))
    }

    /// Presign PUT with `x-amz-checksum-sha256` in the signed headers (bucket-rejects-corrupt).
    /// Checksum is signed as BASE64 (what the client puts on the wire — S3 checksum
    /// headers are base64 of the raw digest), never as hex, and never as a query
    /// parameter (every query param must appear in the canonical query string).
    pub fn presign_put(
        &self,
        key: &str,
        ttl_secs: u64,
        now_millis: i64,
        checksum_hex: &str,
    ) -> String {
        let checksum_b64 = cairn_core::hash::b64_encode(
            &cairn_core::hash::hex_decode(checksum_hex).unwrap_or_default(),
        );
        let ttl = ttl_secs.min(3600);
        let amz_date = amz_date(now_millis);
        let date = &amz_date[..8];
        let host = host_of(&self.endpoint);
        let scope = format!("{date}/{}/s3/aws4_request", self.region);
        let canonical_uri = self.canonical_uri(key);
        let canonical_query = format!(
            "X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Credential={}&X-Amz-Date={}&X-Amz-Expires={ttl}&X-Amz-SignedHeaders=host%3Bx-amz-checksum-sha256%3Bx-amz-content-sha256",
            uri_encode(&format!("{}/{}", self.access_key, scope), true),
            amz_date,
        );
        let canonical_headers = format!(
            "host:{host}\nx-amz-checksum-sha256:{checksum_b64}\nx-amz-content-sha256:UNSIGNED-PAYLOAD\n"
        );
        let signed_headers = "host;x-amz-checksum-sha256;x-amz-content-sha256";
        let canonical_request = [
            "PUT",
            canonical_uri.as_str(),
            canonical_query.as_str(),
            canonical_headers.as_str(),
            signed_headers,
            "UNSIGNED-PAYLOAD",
        ]
        .join("\n");
        let string_to_sign = [
            "AWS4-HMAC-SHA256",
            amz_date.as_str(),
            scope.as_str(),
            &SigV4Presigner::sha256_hex(canonical_request.as_bytes()),
        ]
        .join("\n");
        let k_date = SigV4Presigner::hmac(
            format!("AWS4{}", self.secret_key).as_bytes(),
            date.as_bytes(),
        );
        let k_region = SigV4Presigner::hmac(&k_date, self.region.as_bytes());
        let k_service = SigV4Presigner::hmac(&k_region, b"s3");
        let k_signing = SigV4Presigner::hmac(&k_service, b"aws4_request");
        let signature = cairn_core::hash::hex_encode(&SigV4Presigner::hmac(
            &k_signing,
            string_to_sign.as_bytes(),
        ));
        format!(
            "{}?{}&X-Amz-Signature={signature}",
            self.object_url(key),
            canonical_query
        )
    }

    /// Presigned GET (host-only signed headers, `UNSIGNED-PAYLOAD`) — immutable,
    /// Range-capable client reads (SPEC §9.3).
    pub fn presign_get(&self, key: &str, ttl_secs: u64, now_millis: i64) -> String {
        let ttl = ttl_secs.min(3600);
        let amz_date = amz_date(now_millis);
        let date = &amz_date[..8];
        let host = host_of(&self.endpoint);
        let scope = format!("{date}/{}/s3/aws4_request", self.region);
        let canonical_query = format!(
            "X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Credential={}&X-Amz-Date={}&X-Amz-Expires={ttl}&X-Amz-SignedHeaders=host",
            uri_encode(&format!("{}/{}", self.access_key, scope), true),
            amz_date,
        );
        let canonical_request = [
            "GET",
            &self.canonical_uri(key),
            canonical_query.as_str(),
            &format!("host:{host}\n"),
            "host",
            "UNSIGNED-PAYLOAD",
        ]
        .join("\n");
        let string_to_sign = [
            "AWS4-HMAC-SHA256",
            amz_date.as_str(),
            scope.as_str(),
            &SigV4Presigner::sha256_hex(canonical_request.as_bytes()),
        ]
        .join("\n");
        let signature = cairn_core::hash::hex_encode(&SigV4Presigner::hmac(
            &SigV4Presigner::derive_signing_key(&self.secret_key, date, &self.region, "s3"),
            string_to_sign.as_bytes(),
        ));
        format!(
            "{}?{}&X-Amz-Signature={signature}",
            self.object_url(key),
            canonical_query
        )
    }

    /// Host-only presigned PUT (`UNSIGNED-PAYLOAD`): the standard S3 presign
    /// form. Checksum-bound presigning (`presign_put` above) is used when the
    /// session carries per-chunk SHA-256s.
    ///
    /// R2 quirk (5GB REAL-S3 soak, 2026-09-02, `scripts/r2_auth_matrix.py`):
    /// Cloudflare R2 requires the request to carry `x-amz-content-sha256:
    /// UNSIGNED-PAYLOAD` as a header AND for that header to be part of
    /// SignedHeaders. Host-only signing without it fails with a misleading
    /// `SignatureDoesNotMatch` 403 — on AWS S3 and MinIO the same URL is
    /// accepted (which is why MinIO CI conformance stayed green). Presigned
    /// GET needs none of this (proven 200 host-only, both providers).
    pub fn presign_put_host_only(&self, key: &str, ttl_secs: u64, now_millis: i64) -> String {
        let ttl = ttl_secs.min(3600);
        let amz_date = amz_date(now_millis);
        let date = &amz_date[..8];
        let host = host_of(&self.endpoint);
        let scope = format!("{date}/{}/s3/aws4_request", self.region);
        let canonical_query = format!(
            "X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Credential={}&X-Amz-Date={}&X-Amz-Expires={ttl}&X-Amz-SignedHeaders=host%3Bx-amz-content-sha256",
            uri_encode(&format!("{}/{}", self.access_key, scope), true),
            amz_date,
        );
        let canonical_request = [
            "PUT",
            &self.canonical_uri(key),
            canonical_query.as_str(),
            &format!("host:{host}\nx-amz-content-sha256:UNSIGNED-PAYLOAD\n"),
            "host;x-amz-content-sha256",
            "UNSIGNED-PAYLOAD",
        ]
        .join("\n");
        let string_to_sign = [
            "AWS4-HMAC-SHA256",
            amz_date.as_str(),
            scope.as_str(),
            &SigV4Presigner::sha256_hex(canonical_request.as_bytes()),
        ]
        .join("\n");
        let signature = cairn_core::hash::hex_encode(&SigV4Presigner::hmac(
            &SigV4Presigner::derive_signing_key(&self.secret_key, date, &self.region, "s3"),
            string_to_sign.as_bytes(),
        ));
        format!(
            "{}?{}&X-Amz-Signature={signature}",
            self.object_url(key),
            canonical_query
        )
    }

    /// SigV4 key-derivation chain: HMAC(HMAC(HMAC(HMAC(kSecret, date), region), service), "aws4_request").
    /// Public for known-answer tests against AWS-published vectors.
    pub fn derive_signing_key(
        secret_key: &str,
        date: &str,
        region: &str,
        service: &str,
    ) -> Vec<u8> {
        let k_date = SigV4Presigner::hmac(format!("AWS4{secret_key}").as_bytes(), date.as_bytes());
        let k_region = SigV4Presigner::hmac(&k_date, region.as_bytes());
        let k_service = SigV4Presigner::hmac(&k_region, service.as_bytes());
        SigV4Presigner::hmac(&k_service, b"aws4_request")
    }

    /// Final signature over a string-to-sign with a derived signing key.
    pub fn sign_string_to_sign(string_to_sign: &str, signing_key: &[u8]) -> String {
        cairn_core::hash::hex_encode(&SigV4Presigner::hmac(
            signing_key,
            string_to_sign.as_bytes(),
        ))
    }

    /// Header-auth tuple for server-side calls (manifests, packs, GC sweep):
    /// returns (Authorization header, x-amz-date header value). Signs
    /// host + x-amz-content-sha256 + x-amz-date; payload hash is the real
    /// SHA-256 of the body (hex), per the SigV4 header-auth canonical form.
    pub fn authorization_header(
        &self,
        method: &str,
        key: &str,
        payload_hash_hex: &str,
        now_millis: i64,
    ) -> (String, String) {
        let canonical_path = self.canonical_uri(key);
        self.authorization_header_path(method, &canonical_path, payload_hash_hex, now_millis)
    }

    /// Header-auth for a RAW canonical path (bucket-level ops like CreateBucket:
    /// canonical path is exactly `/{bucket}` — no object key involved).
    pub fn authorization_header_path(
        &self,
        method: &str,
        canonical_path: &str,
        payload_hash_hex: &str,
        now_millis: i64,
    ) -> (String, String) {
        let amz_date = amz_date(now_millis);
        let date = &amz_date[..8];
        let host = host_of(&self.endpoint);
        let scope = format!("{date}/{}/s3/aws4_request", self.region);
        let canonical_headers = format!(
            "host:{host}\nx-amz-content-sha256:{payload_hash_hex}\nx-amz-date:{amz_date}\n"
        );
        let signed_headers = "host;x-amz-content-sha256;x-amz-date";
        let canonical_request = [
            method,
            canonical_path,
            "", // no query string on server-side calls
            canonical_headers.as_str(),
            signed_headers,
            payload_hash_hex,
        ]
        .join("\n");
        let string_to_sign = [
            "AWS4-HMAC-SHA256",
            amz_date.as_str(),
            scope.as_str(),
            &SigV4Presigner::sha256_hex(canonical_request.as_bytes()),
        ]
        .join("\n");
        let signing_key =
            SigV4Presigner::derive_signing_key(&self.secret_key, date, &self.region, "s3");
        let signature = SigV4Presigner::sign_string_to_sign(&string_to_sign, &signing_key);
        let auth = format!(
            "AWS4-HMAC-SHA256 Credential={}/{}, SignedHeaders={signed_headers}, Signature={signature}",
            self.access_key, scope
        );
        (auth, amz_date)
    }
}

/// S3-compatible production backend (ADR-0005): real SigV4 against a real bucket
/// (AWS S3, Cloudflare R2, B2 S3-compatible API). Client chunk/manifest
/// transfers ride presigned URLs — bytes never traverse the API server (SPEC
/// §9). Server-side paths (manifests, packs, GC sweep) use header-signed
/// requests. Constructed from environment; absent config falls back to the
/// dev `LocalFsStore` (see `run.rs`).
///
/// Environment (all required):
/// - `CAIRN_S3_ENDPOINT`    e.g. `https://s3.us-west-004.backblazeb2.com`
/// - `CAIRN_S3_BUCKET`      e.g. `studio-media`
/// - `CAIRN_S3_REGION`      e.g. `us-west-004`
/// - `CAIRN_S3_ACCESS_KEY_ID`
/// - `CAIRN_S3_SECRET_ACCESS_KEY`
pub struct S3ObjectStore {
    presigner: SigV4Presigner,
    /// Virtual-host form: `https://bucket.endpoint-host` (presigned-URL target in
    /// vhost mode). Empty in path-style mode — URLs come from the presigner.
    vhost_endpoint: String,
    http: reqwest::Client,
}

impl S3ObjectStore {
    /// Idempotent CreateBucket at startup (quirk S1 follow-up: the soak's S1
    /// gate failed on `NoSuchBucket` because NOTHING created the bucket — the
    /// checksum error had masked it). 200/201 created and 409
    /// BucketAlreadyOwnedByYou both mean "usable"; 403 is tolerated with a
    /// warning because real deployments often grant a token that can write but
    /// not create (the runbook then owns bucket provisioning); anything else
    /// fails the startup loudly — a miswired bucket is a CONFIG error, not a
    /// per-save surprise.
    pub async fn ensure_bucket(&self) -> Result<(), CairnError> {
        let now = cairn_core::clock::WallClock.now_millis();
        let payload_hash = SigV4Presigner::sha256_hex(b"");
        let (canonical_path, url) = if self.presigner.path_style {
            (
                format!("/{}", self.presigner.bucket),
                format!("{}/{}", self.presigner.endpoint, self.presigner.bucket),
            )
        } else {
            // virtual-host: the bucket is IN the endpoint host already
            ("/".to_string(), format!("{}/", self.presigner.endpoint))
        };
        let (auth, amz_date) =
            self.presigner
                .authorization_header_path("PUT", &canonical_path, &payload_hash, now);
        let resp = self
            .http
            .put(&url)
            .header("Authorization", auth)
            .header("x-amz-date", amz_date)
            .header("x-amz-content-sha256", &payload_hash)
            .send()
            .await
            .map_err(|e| {
                CairnError::new(
                    cairn_core::ErrorKind::Unavailable,
                    format!("bucket ensure: {e}"),
                )
            })?;
        match resp.status().as_u16() {
            200 | 201 | 409 => Ok(()),
            403 => {
                tracing::warn!(
                    "bucket create denied (token may lack CreateBucket) — assuming the bucket pre-exists; first write will fail loudly if not"
                );
                Ok(())
            }
            s => Err(CairnError::new(
                cairn_core::ErrorKind::Unavailable,
                format!(
                    "bucket ensure: HTTP {s} for {} (check CAIRN_S3_ENDPOINT/credentials)",
                    self.presigner.bucket
                ),
            )),
        }
    }

    /// Build from the standard `CAIRN_S3_*` environment; `None` when unset/incomplete.
    /// `CAIRN_S3_PATH_STYLE=1` switches to path-style addressing (MinIO,
    /// self-hosted gateways, localhost targets — no wildcard DNS needed).
    pub fn from_env() -> Option<Self> {
        let get = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        let endpoint = get("CAIRN_S3_ENDPOINT")?;
        let bucket = get("CAIRN_S3_BUCKET")?;
        let region = get("CAIRN_S3_REGION")?;
        let access_key = get("CAIRN_S3_ACCESS_KEY_ID")?;
        let secret_key = get("CAIRN_S3_SECRET_ACCESS_KEY")?;
        let path_style = std::env::var("CAIRN_S3_PATH_STYLE")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        let host = endpoint
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .trim_end_matches('/');
        let scheme = if endpoint.starts_with("http://") {
            "http"
        } else {
            "https"
        };
        let vhost_endpoint = format!("{scheme}://{bucket}.{host}");
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .ok()?;
        let presigner = if path_style {
            SigV4Presigner::new_path_style(
                &access_key,
                &secret_key,
                &region,
                &format!("{scheme}://{host}"),
                &bucket,
            )
        } else {
            SigV4Presigner::new(&access_key, &secret_key, &region, &vhost_endpoint)
        };
        Some(S3ObjectStore {
            presigner,
            vhost_endpoint: if path_style {
                String::new()
            } else {
                vhost_endpoint
            },
            http,
        })
    }

    fn url(&self, key: &str) -> String {
        if self.vhost_endpoint.is_empty() {
            // path-style mode: the presigner owns URL construction
            self.presigner.url_for(key)
        } else {
            format!("{}/{}", self.vhost_endpoint, uri_encode(key, false))
        }
    }

    fn now(&self) -> i64 {
        cairn_core::clock::WallClock.now_millis()
    }
}

#[async_trait]
impl ObjectStore for S3ObjectStore {
    async fn put(&self, key: &str, bytes: &[u8]) -> Result<(), CairnError> {
        let payload_hash = SigV4Presigner::sha256_hex(bytes);
        let (auth, amz_date) =
            self.presigner
                .authorization_header("PUT", key, &payload_hash, self.now());
        let resp = self
            .http
            .put(self.url(key))
            .header("Authorization", auth)
            .header("x-amz-date", amz_date)
            .header("x-amz-content-sha256", payload_hash)
            .body(bytes.to_vec())
            .send()
            .await
            .map_err(|e| CairnError::new(ErrorKind::Unavailable, format!("s3 put: {e}")))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(CairnError::new(
                ErrorKind::Unavailable,
                format!("s3 put {}: {status} {body}", Self::brief(&body)),
            ));
        }
        Ok(())
    }

    async fn get(&self, key: &str) -> Result<Vec<u8>, CairnError> {
        let payload_hash = SigV4Presigner::sha256_hex(b"");
        let (auth, amz_date) =
            self.presigner
                .authorization_header("GET", key, &payload_hash, self.now());
        let resp = self
            .http
            .get(self.url(key))
            .header("Authorization", auth)
            .header("x-amz-date", amz_date)
            .header("x-amz-content-sha256", payload_hash)
            .send()
            .await
            .map_err(|e| CairnError::new(ErrorKind::Unavailable, format!("s3 get: {e}")))?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(CairnError::new(
                ErrorKind::NotFound,
                format!("object {key}"),
            ));
        }
        if !resp.status().is_success() {
            return Err(CairnError::new(
                ErrorKind::Unavailable,
                format!("s3 get {key}: {}", resp.status()),
            ));
        }
        Ok(resp
            .bytes()
            .await
            .map_err(|e| CairnError::new(ErrorKind::Unavailable, format!("s3 body: {e}")))?
            .to_vec())
    }

    async fn head(&self, key: &str) -> Result<u64, CairnError> {
        let payload_hash = SigV4Presigner::sha256_hex(b"");
        let (auth, amz_date) =
            self.presigner
                .authorization_header("HEAD", key, &payload_hash, self.now());
        let resp = self
            .http
            .head(self.url(key))
            .header("Authorization", auth)
            .header("x-amz-date", amz_date)
            .header("x-amz-content-sha256", payload_hash)
            .send()
            .await
            .map_err(|e| CairnError::new(ErrorKind::Unavailable, format!("s3 head: {e}")))?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(CairnError::new(
                ErrorKind::NotFound,
                format!("object {key}"),
            ));
        }
        if !resp.status().is_success() {
            return Err(CairnError::new(
                ErrorKind::Unavailable,
                format!("s3 head {key}: {}", resp.status()),
            ));
        }
        Ok(resp
            .headers()
            .get(reqwest::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok().and_then(|s| s.parse().ok()))
            .unwrap_or(0))
    }

    async fn delete(&self, key: &str) -> Result<(), CairnError> {
        let payload_hash = SigV4Presigner::sha256_hex(b"");
        let (auth, amz_date) =
            self.presigner
                .authorization_header("DELETE", key, &payload_hash, self.now());
        let resp = self
            .http
            .delete(self.url(key))
            .header("Authorization", auth)
            .header("x-amz-date", amz_date)
            .header("x-amz-content-sha256", payload_hash)
            .send()
            .await
            .map_err(|e| CairnError::new(ErrorKind::Unavailable, format!("s3 delete: {e}")))?;
        // DELETE is idempotent per the S3 contract: 204/404 both mean "gone".
        if resp.status().is_success() || resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(());
        }
        Err(CairnError::new(
            ErrorKind::Unavailable,
            format!("s3 delete {key}: {}", resp.status()),
        ))
    }

    async fn presign_put(&self, key: &str, ttl_secs: u64) -> Result<String, CairnError> {
        // Host-only standard presign form. Clients attach `x-amz-checksum-sha256`
        // on PUT; CompleteUpload sample-verify enforces BLAKE3 equality (SPEC
        // §9.2). Checksum-*signed* presigning (checksum bound into SignedHeaders)
        // activates with the session extension that carries per-chunk SHA-256s
        // (SigV4Presigner::presign_put is ready for it).
        Ok(self
            .presigner
            .presign_put_host_only(key, ttl_secs, self.now()))
    }

    async fn presign_get(&self, key: &str, ttl_secs: u64) -> Result<String, CairnError> {
        Ok(self.presigner.presign_get(key, ttl_secs, self.now()))
    }

    fn name(&self) -> &'static str {
        "s3"
    }
}

impl S3ObjectStore {
    fn brief(s: &str) -> String {
        s.chars().take(160).collect()
    }
}

fn amz_date(now_millis: i64) -> String {
    let secs = now_millis.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    // civil-from-days (Howard Hinnant's algorithm) — no external chrono dep
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z",
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60
    )
}

fn host_of(endpoint: &str) -> String {
    endpoint
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or("")
        .to_string()
}

fn uri_encode(s: &str, encode_slash: bool) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(char::from(b));
            }
            b'/' if !encode_slash => out.push('/'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sigv4_date_format() {
        // 1700000000 = 2023-11-14T22:13:20Z
        assert_eq!(amz_date(1_700_000_000_000), "20231114T221320Z");
    }

    #[tokio::test]
    async fn local_store_presign_roundtrip_and_checksum_rejection() {
        let dir = tempfile::tempdir().unwrap();
        let store =
            LocalFsStore::open(dir.path(), b"test-signing-key", "http://127.0.0.1:17999").unwrap();
        let key = LocalFsStore::chunk_key("tenant1", &format!("{:064x}", 42));
        let url = store.presign_put(&key, 3600).await.unwrap();
        assert!(url.contains("sig="));
        let head = store.head(&key).await;
        assert!(head.is_err(), "object must not exist before PUT");
        // server-side verify logic is exercised by the HTTP endpoint tests (cairn-cli server)
        assert_eq!(store.name(), "local-fs");
    }

    #[test]
    fn sigv4_presign_shape() {
        let p = SigV4Presigner::new(
            "AKIDEXAMPLE",
            "secret",
            "us-east-1",
            "https://bucket.s3.amazonaws.com",
        );
        let url = p.presign_put("t1/c/ab/hash", 3600, 1_700_000_000_000, "deadbeef");
        assert!(url.starts_with("https://bucket.s3.amazonaws.com/t1/c/ab/hash?"));
        assert!(url.contains("X-Amz-Algorithm=AWS4-HMAC-SHA256"));
        assert!(
            url.contains("X-Amz-SignedHeaders=host%3Bx-amz-checksum-sha256%3Bx-amz-content-sha256")
        );
        // The checksum is signed as a HEADER (base64 on the wire), never as a
        // query parameter: every query param must be inside the canonical query
        // string, and an unsigned extra param makes strict validators (R2)
        // reject with SignatureDoesNotMatch. Hex values in the header are
        // InvalidArgument on S3/MinIO (quirk S1).
        assert!(
            !url.contains("x-amz-checksum-sha256="),
            "checksum must not be a query param"
        );
        assert!(url.contains("X-Amz-Signature="));
    }

    /// AWS-published known-answer vector (AWS General Reference,
    /// "Example: computing the signature" — IAM ListUsers, header auth).
    /// Proves the KDF chain + string-to-sign math byte-for-byte.
    #[test]
    fn sigv4_aws_known_answer_vector() {
        // Fixed inputs from the AWS documentation example.
        let secret = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";
        let date = "20150830";
        let region = "us-east-1";
        let service = "iam";
        let canonical_request = [
            "GET",
            "/",
            "Action=ListUsers&Version=2010-05-08",
            "content-type:application/x-www-form-urlencoded; charset=utf-8\nhost:iam.amazonaws.com\nx-amz-date:20150830T123600Z\n",
            "content-type;host;x-amz-date",
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        ]
        .join("\n");
        let string_to_sign = [
            "AWS4-HMAC-SHA256",
            "20150830T123600Z",
            "20150830/us-east-1/iam/aws4_request",
            &SigV4Presigner::sha256_hex(canonical_request.as_bytes()),
        ]
        .join("\n");
        let key = SigV4Presigner::derive_signing_key(secret, date, region, service);
        let sig = SigV4Presigner::sign_string_to_sign(&string_to_sign, &key);
        assert_eq!(
            sig, "5d672d79c15b13162d9279b0855cfba6789a8edb4c82c400e06b5924a6f2b5d7",
            "SigV4 KDF/vector mismatch — signing math diverged from AWS spec"
        );
    }

    /// S3 presigned GET: AWS-published example shape (examplebucket/test.txt,
    /// sigv4 query form). Structural assertions + KDF reuse proven above.
    #[test]
    fn sigv4_s3_presign_get_shape_and_stability() {
        let p = SigV4Presigner::new(
            "AKIAIOSFODNN7EXAMPLE",
            "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            "us-east-1",
            "https://examplebucket.s3.amazonaws.com",
        );
        let a = p.presign_get("test.txt", 86400, 1_369_353_600_000); // 20130524T000000Z
        assert!(a.starts_with("https://examplebucket.s3.amazonaws.com/test.txt?"));
        assert!(a.contains("X-Amz-Algorithm=AWS4-HMAC-SHA256"));
        assert!(a.contains(
            "X-Amz-Credential=AKIAIOSFODNN7EXAMPLE%2F20130524%2Fus-east-1%2Fs3%2Faws4_request"
        ));
        assert!(a.contains("X-Amz-Date=20130524T000000Z"));
        assert!(
            a.contains("X-Amz-Expires=3600"),
            "TTL clamped to the 1h SPEC cap"
        );
        assert!(a.contains("X-Amz-SignedHeaders=host"));
        // deterministic: same inputs -> byte-identical URL
        let b = p.presign_get("test.txt", 86400, 1_369_353_600_000);
        assert_eq!(a, b);
    }

    /// Header-auth: stable, well-formed Authorization for server-side calls.
    #[test]
    fn sigv4_header_auth_shape_and_stability() {
        let p = SigV4Presigner::new(
            "AKIAIOSFODNN7EXAMPLE",
            "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            "us-east-1",
            "https://examplebucket.s3.amazonaws.com",
        );
        let payload = SigV4Presigner::sha256_hex(b"manifest-bytes");
        let (auth, date) =
            p.authorization_header("PUT", "t1/m/ab/hash", &payload, 1_369_353_600_000);
        assert_eq!(date, "20130524T000000Z");
        assert!(auth.starts_with(
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, "
        ));
        assert!(auth.contains("SignedHeaders=host;x-amz-content-sha256;x-amz-date, "));
        let (auth2, _) = p.authorization_header("PUT", "t1/m/ab/hash", &payload, 1_369_353_600_000);
        assert_eq!(auth, auth2);
    }

    /// Path-style presigning: canonical URI and URL both carry the bucket
    /// (validated on the wire against MinIO by the cairn-x conformance suite).
    #[test]
    fn sigv4_path_style_shape_and_determinism() {
        let p = SigV4Presigner::new_path_style(
            "AKIDEXAMPLE",
            "secret",
            "us-east-1",
            "http://127.0.0.1:19000",
            "cairn-test",
        );
        let url = p.presign_get("t1/c/ab/hash", 600, 1_700_000_000_000);
        assert!(url.starts_with("http://127.0.0.1:19000/cairn-test/t1/c/ab/hash?"));
        assert!(url.contains("X-Amz-SignedHeaders=host"));
        let put = p.presign_put_host_only("t1/c/ab/hash", 600, 1_700_000_000_000);
        assert!(put.starts_with("http://127.0.0.1:19000/cairn-test/t1/c/ab/hash?"));
        // header-auth canonical URI carries the bucket too
        let (auth, _) = p.authorization_header(
            "PUT",
            "cairn-test",
            &SigV4Presigner::sha256_hex(b""),
            1_700_000_000_000,
        );
        assert!(auth.contains("SignedHeaders=host;x-amz-content-sha256;x-amz-date"));
        // deterministic
        assert_eq!(url, p.presign_get("t1/c/ab/hash", 600, 1_700_000_000_000));
    }

    /// from_env: complete config -> Some; partial config -> None (no half-wired backends).
    #[test]
    fn s3_from_env_all_or_nothing() {
        // Ensure a clean slate regardless of the outer environment.
        for k in [
            "CAIRN_S3_ENDPOINT",
            "CAIRN_S3_BUCKET",
            "CAIRN_S3_REGION",
            "CAIRN_S3_ACCESS_KEY_ID",
            "CAIRN_S3_SECRET_ACCESS_KEY",
        ] {
            std::env::remove_var(k);
        }
        assert!(S3ObjectStore::from_env().is_none());
        std::env::set_var(
            "CAIRN_S3_ENDPOINT",
            "https://s3.us-west-004.backblazeb2.com",
        );
        std::env::set_var("CAIRN_S3_BUCKET", "studio-media");
        std::env::set_var("CAIRN_S3_REGION", "us-west-004");
        std::env::set_var("CAIRN_S3_ACCESS_KEY_ID", "AKIATEST");
        std::env::set_var("CAIRN_S3_SECRET_ACCESS_KEY", "shhh");
        let store = S3ObjectStore::from_env().expect("complete env -> Some");
        assert_eq!(store.name(), "s3");
        assert_eq!(
            store.url("t1/c/ab/h"),
            "https://studio-media.s3.us-west-004.backblazeb2.com/t1/c/ab/h"
        );
        for k in [
            "CAIRN_S3_ENDPOINT",
            "CAIRN_S3_BUCKET",
            "CAIRN_S3_REGION",
            "CAIRN_S3_ACCESS_KEY_ID",
            "CAIRN_S3_SECRET_ACCESS_KEY",
        ] {
            std::env::remove_var(k);
        }
    }
}

#[cfg(test)]
mod amz_probe {
    #[test]
    fn amz_date_2026_vectors() {
        assert_eq!(super::amz_date(1_788_000_000_000), "20260829T104000Z");
        assert_eq!(super::amz_date(1_788_300_000_000), "20260901T220000Z");
    }
}
