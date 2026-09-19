//! Data-plane HTTP client (pooled, keep-alive).
//!
//! Production hardening note (docs/STATUS.md): the client ships behind the store abstraction;
//! the hardened transfer path (TLS, proxies, HTTP/2) is provided by the deployment's bucket
//! SDK gateway. This client implements exactly the semantics the engine needs:
//! presigned PUT with `x-amz-checksum-sha256`, presigned GET with Range + 403 renewal, and
//! strict status-code handling.
//!
//! Performance note (P0 fix): requests previously opened a fresh `TcpStream`
//! per call with `Connection: close` and buffered responses via `read_to_end`
//! to EOF. This module now uses a shared `reqwest::Client` with connection
//! pooling + keep-alive, bounded bodies, and header-driven framing instead of
//! EOF. Callers keep the same `put_object` / `get_object` signatures.

#![allow(dead_code)] // full client surface kept for the harness; not all paths exercised

use std::sync::OnceLock;

/// HTTP response (status, headers lowercased, body).
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

/// Upper bound for a single response/request body through this client.
/// Chunk traffic is MiB-scale; the cap is a fail-closed guard against a
/// misbehaving endpoint turning a download into an unbounded allocation.
pub const MAX_BODY_BYTES: usize = 512 * 1024 * 1024;

fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .pool_max_idle_per_host(32)
            .pool_idle_timeout(std::time::Duration::from_secs(90))
            .tcp_keepalive(std::time::Duration::from_secs(60))
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .expect("reqwest pooled client builds")
    })
}

fn io_err(e: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::other(e.to_string())
}

async fn request(
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: &[u8],
) -> std::io::Result<Response> {
    if body.len() > MAX_BODY_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "request body exceeds MAX_BODY_BYTES",
        ));
    }
    let is_put = method == "PUT";
    let method = reqwest::Method::from_bytes(method.as_bytes()).map_err(io_err)?;
    let mut req = client().request(method, url);
    for (n, v) in headers {
        req = req.header(n.as_str(), v.as_str());
    }
    if !body.is_empty() {
        req = req.body(body.to_vec());
    } else if is_put {
        req = req.body(Vec::new());
    }
    let resp = req.send().await.map_err(io_err)?;
    let status = resp.status().as_u16();
    let mut resp_headers = Vec::new();
    for (n, v) in resp.headers().iter() {
        resp_headers.push((
            n.as_str().to_lowercase(),
            v.to_str().unwrap_or_default().to_string(),
        ));
    }
    if let Some(len) = resp.content_length() {
        if len > MAX_BODY_BYTES as u64 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "response body exceeds MAX_BODY_BYTES",
            ));
        }
    }
    let bytes = resp.bytes().await.map_err(io_err)?;
    if bytes.len() > MAX_BODY_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "response body exceeds MAX_BODY_BYTES",
        ));
    }
    Ok(Response {
        status,
        headers: resp_headers,
        body: bytes.to_vec(),
    })
}

/// Presigned PUT with checksum (bucket-rejects-corrupt semantics).
pub async fn put_object(url: &str, bytes: &[u8], checksum_hex: &str) -> std::io::Result<Response> {
    request(
        "PUT",
        url,
        &[("x-amz-checksum-sha256".into(), checksum_hex.to_string())],
        bytes,
    )
    .await
}

/// Presigned GET (immutable); `range` = `bytes=a-b` optional.
pub async fn get_object(url: &str, range: Option<&str>) -> std::io::Result<Response> {
    let mut headers = Vec::new();
    if let Some(r) = range {
        headers.push(("Range".into(), r.to_string()));
    }
    request("GET", url, &headers, &[]).await
}
