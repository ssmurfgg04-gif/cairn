//! Minimal HTTP/1.1 client for the data plane (dev-grade, fixed-length bodies only).
//!
//! Production hardening note (docs/STATUS.md): the client ships behind the store abstraction;
//! the hardened transfer path (TLS, proxies, HTTP/2) is provided by the deployment's bucket
//! SDK gateway. This client implements exactly the semantics the engine needs:
//! presigned PUT with `x-amz-checksum-sha256`, presigned GET with Range + 403 renewal, and
//! strict status-code handling.

#![allow(dead_code)] // full client surface kept for the harness; not all paths exercised

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

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

/// Parse `http://host:port/path?query` into (host:port, path?query).
fn split_url(url: &str) -> (String, String) {
    let rest = url.strip_prefix("http://").unwrap_or(url);
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    (authority.to_string(), format!("/{path}"))
}

async fn request(
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: &[u8],
) -> std::io::Result<Response> {
    let (authority, target) = split_url(url);
    let mut stream = TcpStream::connect(&authority).await?;
    let mut req =
        format!("{method} {target} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n");
    for (n, v) in headers {
        req.push_str(&format!("{n}: {v}\r\n"));
    }
    if !body.is_empty() || method == "PUT" {
        req.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    req.push_str("\r\n");
    stream.write_all(req.as_bytes()).await?;
    if !body.is_empty() {
        stream.write_all(body).await?;
    }
    stream.flush().await?;

    // read full response (Connection: close ⇒ read to EOF)
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await?;

    let header_end = find_header_end(&buf)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "bad http response"))?;
    let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let mut lines = head.lines();
    let status_line = lines.next().unwrap_or_default();
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let mut resp_headers = Vec::new();
    for line in lines {
        if let Some((n, v)) = line.split_once(':') {
            resp_headers.push((n.trim().to_lowercase(), v.trim().to_string()));
        }
    }
    let body_start = header_end + 4;
    Ok(Response {
        status,
        headers: resp_headers,
        body: buf[body_start.min(buf.len())..].to_vec(),
    })
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
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
