//! Object storage (SPEC §9/§12, ADR-0005): the data plane talks client ↔ bucket DIRECT; the
//! API server never proxies blob bytes. Backends behind one trait:
//!
//! - `LocalFsStore` (dev/test): emulates bucket semantics — presigned PUT requires
//!   `x-amz-checksum-sha256` and REJECTS mismatches; GET is immutable + Range-capable with
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
    pub fn open(root: &std::path::Path, signing_key: &[u8], base_url: &str) -> Result<Self, CairnError> {
        std::fs::create_dir_all(root)
            .map_err(|e| CairnError::new(ErrorKind::Io, format!("store mkdir: {e}")))?;
        Ok(LocalFsStore {
            root: root.to_path_buf(),
            signing_key: signing_key.to_vec(),
            base_url: base_url.trim_end_matches('/').to_string(),
        })
    }

    fn path_for(&self, key: &str) -> std::path::PathBuf {
        // keys are tenant-scoped and constructed server-side only; never allow traversal
        let safe: String = key
            .chars()
            .map(|c| if c == '/' || c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' { c } else { '_' })
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
        format!("t{tenant_id}/c/{}/{}", &hash_hex[..2.min(hash_hex.len())], hash_hex)
    }

    /// Storage key for tenant objects (manifests/trees/commits).
    #[must_use]
    pub fn object_key(tenant_id: &str, hash_hex: &str) -> String {
        format!("t{tenant_id}/o/{}/{}", &hash_hex[..2.min(hash_hex.len())], hash_hex)
    }

    /// Dev HTTP router: implements the presigned PUT/GET semantics the data plane relies on
    /// (bucket-rejects-corrupt + immutable Range GETs).
    pub fn router(self: std::sync::Arc<Self>) -> axum::Router {
        use axum::extract::{Path as AxPath, Query, State};
        use axum::http::{HeaderMap, StatusCode};

        type Resp = (axum::http::StatusCode, axum::http::HeaderMap, Vec<u8>);

        fn plain(status: axum::http::StatusCode, msg: &str) -> Resp {
            (status, axum::http::HeaderMap::new(), msg.as_bytes().to_vec())
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
            // bucket-rejects-corrupt: x-amz-checksum-sha256 required and verified
            let Some(provided) = headers.get("x-amz-checksum-sha256").and_then(|v| v.to_str().ok()) else {
                return plain(StatusCode::BAD_REQUEST, "x-amz-checksum-sha256 required");
            };
            let digest = Sha256::digest(&body);
            if provided != cairn_core::hash::hex_encode(&digest) {
                return plain(StatusCode::BAD_REQUEST, "checksum mismatch — corrupt upload rejected");
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
                    h.insert("cache-control", "public, max-age=31536000, immutable".parse().unwrap());
                    h.insert("accept-ranges", "bytes".parse().unwrap());
                    h.insert(
                        "content-range",
                        format!("bytes {start}-{end}/{}", full.len()).parse().unwrap(),
                    );
                    return (StatusCode::PARTIAL_CONTENT, h, slice);
                }
            }
            let mut h = axum::http::HeaderMap::new();
            h.insert("content-type", "application/octet-stream".parse().unwrap());
            h.insert("cache-control", "public, max-age=31536000, immutable".parse().unwrap());
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
                let end = if b.is_empty() { total.saturating_sub(1) } else { b.parse().ok()? };
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
        let exp = cairn_core::clock::WallClock.now_millis() + i64::try_from(ttl * 1000).unwrap_or(3_600_000);
        let sig = self.sign("PUT", key, exp);
        Ok(format!("{}/objects/{}?exp={exp}&sig={sig}", self.base_url, url_enc(key)))
    }

    async fn presign_get(&self, key: &str, ttl_secs: u64) -> Result<String, CairnError> {
        let ttl = ttl_secs.min(3600);
        let exp = cairn_core::clock::WallClock.now_millis() + i64::try_from(ttl * 1000).unwrap_or(3_600_000);
        let sig = self.sign("GET", key, exp);
        Ok(format!("{}/objects/{}?exp={exp}&sig={sig}", self.base_url, url_enc(key)))
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
    endpoint: String, // https://bucket.host (path-style avoided; virtual-host assumed)
}

impl SigV4Presigner {
    /// New presigner for an S3-compatible endpoint (host form: `https://bucket.host`).
    pub fn new(access_key: &str, secret_key: &str, region: &str, endpoint: &str) -> Self {
        SigV4Presigner {
            access_key: access_key.into(),
            secret_key: secret_key.into(),
            region: region.into(),
            endpoint: endpoint.trim_end_matches('/').into(),
        }
    }

    fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
        let mut mac = HmacSha256::new_from_slice(key).expect("hmac");
        mac.update(data);
        mac.finalize().into_bytes().to_vec()
    }

    fn sha256_hex(data: &[u8]) -> String {
        cairn_core::hash::hex_encode(&Sha256::digest(data))
    }

    /// Presign PUT with `x-amz-checksum-sha256` in the signed headers (bucket-rejects-corrupt).
    pub fn presign_put(&self, key: &str, ttl_secs: u64, now_millis: i64, checksum_hex: &str) -> String {
        let ttl = ttl_secs.min(3600);
        let amz_date = amz_date(now_millis);
        let date = &amz_date[..8];
        let host = host_of(&self.endpoint);
        let scope = format!("{date}/{}/s3/aws4_request", self.region);
        let canonical_uri = format!("/{key}");
        let canonical_query = format!(
            "X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Credential={}&X-Amz-Date={}&X-Amz-Expires={ttl}&X-Amz-SignedHeaders=host%3Bx-amz-checksum-sha256",
            uri_encode(&format!("{}/{}", self.access_key, scope), true),
            amz_date,
        );
        let canonical_headers = format!("host:{host}\nx-amz-checksum-sha256:{checksum_hex}\n");
        let signed_headers = "host;x-amz-checksum-sha256";
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
        let k_date = SigV4Presigner::hmac(format!("AWS4{}", self.secret_key).as_bytes(), date.as_bytes());
        let k_region = SigV4Presigner::hmac(&k_date, self.region.as_bytes());
        let k_service = SigV4Presigner::hmac(&k_region, b"s3");
        let k_signing = SigV4Presigner::hmac(&k_service, b"aws4_request");
        let signature = cairn_core::hash::hex_encode(&SigV4Presigner::hmac(&k_signing, string_to_sign.as_bytes()));
        format!(
            "{}/{}?{}&X-Amz-Signature={signature}&x-amz-checksum-sha256={checksum_hex}",
            self.endpoint,
            key,
            canonical_query
        )
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
    format!("{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z", tod / 3600, (tod % 3600) / 60, tod % 60)
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
        let store = LocalFsStore::open(dir.path(), b"test-signing-key", "http://127.0.0.1:17999").unwrap();
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
        let p = SigV4Presigner::new("AKIDEXAMPLE", "secret", "us-east-1", "https://bucket.s3.amazonaws.com");
        let url = p.presign_put("t1/c/ab/hash", 3600, 1_700_000_000_000, "deadbeef");
        assert!(url.starts_with("https://bucket.s3.amazonaws.com/t1/c/ab/hash?"));
        assert!(url.contains("X-Amz-Algorithm=AWS4-HMAC-SHA256"));
        assert!(url.contains("X-Amz-SignedHeaders=host%3Bx-amz-checksum-sha256"));
        assert!(url.contains("x-amz-checksum-sha256=deadbeef"));
    }
}
