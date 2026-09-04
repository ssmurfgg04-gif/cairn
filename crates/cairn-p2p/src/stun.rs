//! Minimal STUN client (RFC 5389 Binding) — reflexive public-address discovery
//! for the NAT punch path, plus a loopback "fake STUN" for deterministic tests.
//!
//! Scope is deliberately tiny: one Binding Request/Response, XOR-MAPPED-ADDRESS
//! (IPv4 + IPv6), transaction-id matching. No authentication attributes (the
//! signal server is authenticated by HMAC; STUN is best-effort discovery).
//!
//! Header layout (20 bytes) — the classic integration bug this module guards
//! against with a regression test: `message length` lives at bytes `2..4` (BE),
//! BEFORE the magic cookie at `4..8`. Reading the length from the cookie bytes
//! silently parses garbage.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::time::Duration;

use tokio::net::UdpSocket as AsyncUdpSocket;

/// STUN magic cookie (RFC 5389 §6).
const MAGIC: [u8; 4] = [0x21, 0x12, 0xA4, 0x42];
/// Binding Request message type.
const BINDING_REQUEST: u16 = 0x0001;
/// Binding Success Response message type.
const BINDING_SUCCESS: u16 = 0x0101;
/// XOR-MAPPED-ADDRESS attribute type.
const ATTR_XOR_MAPPED: u16 = 0x0020;

const HEADER_LEN: usize = 20;

/// Build a 20-byte Binding Request with transaction id `txid`.
#[must_use]
pub fn binding_request(txid: &[u8; 12]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN);
    out.extend_from_slice(&BINDING_REQUEST.to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes()); // no attributes
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(txid);
    out
}

/// Build a Binding Success Response carrying `XOR-MAPPED-ADDRESS = addr`.
#[must_use]
pub fn binding_success_response(txid: &[u8; 12], addr: SocketAddr) -> Vec<u8> {
    let value = encode_xor_mapped(txid, addr);
    let attr_len = u16::try_from(value.len()).unwrap_or(u16::MAX);
    let mut attrs = Vec::with_capacity(4 + value.len());
    attrs.extend_from_slice(&ATTR_XOR_MAPPED.to_be_bytes());
    attrs.extend_from_slice(&attr_len.to_be_bytes());
    attrs.extend_from_slice(&value);
    // RFC 5389: attributes are padded to 4-byte boundaries; XOR-MAPPED-ADDRESS
    // is 8 or 20 bytes — already aligned, no padding needed.
    let mut out = Vec::with_capacity(HEADER_LEN + attrs.len());
    out.extend_from_slice(&BINDING_SUCCESS.to_be_bytes());
    out.extend_from_slice(&u16::try_from(attrs.len()).unwrap_or(u16::MAX).to_be_bytes());
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(txid);
    out.extend_from_slice(&attrs);
    out
}

fn encode_xor_mapped(txid: &[u8; 12], addr: SocketAddr) -> Vec<u8> {
    let (family, port, ip_bytes): (u8, u16, Vec<u8>) = match addr.ip() {
        IpAddr::V4(ip) => {
            let b = ip.octets();
            let x = [
                b[0] ^ MAGIC[0],
                b[1] ^ MAGIC[1],
                b[2] ^ MAGIC[2],
                b[3] ^ MAGIC[3],
            ];
            (0x01, addr.port(), x.to_vec())
        }
        IpAddr::V6(ip) => {
            let b = ip.octets();
            let mut mask = [0u8; 16];
            mask[..4].copy_from_slice(&MAGIC);
            mask[4..].copy_from_slice(txid);
            let x: Vec<u8> = b.iter().zip(mask.iter()).map(|(a, m)| a ^ m).collect();
            (0x02, addr.port(), x)
        }
    };
    let mut v = Vec::with_capacity(1 + 2 + ip_bytes.len());
    v.push(0); // reserved
    v.push(family);
    v.extend_from_slice(&(port ^ 0x2112).to_be_bytes());
    v.extend_from_slice(&ip_bytes);
    v
}

/// Parse a Binding Success Response: verify magic + transaction id, then walk
/// attributes for XOR-MAPPED-ADDRESS. Returns `None` on any mismatch.
#[must_use]
pub fn parse_response(response: &[u8], expected_txid: &[u8; 12]) -> Option<SocketAddr> {
    if response.len() < HEADER_LEN {
        return None;
    }
    let msg_type = u16::from_be_bytes([response[0], response[1]]);
    // ⚠ regression guard: message length is bytes 2..4 — NOT the cookie at 4..8
    let msg_len = u16::from_be_bytes([response[2], response[3]]) as usize;
    if msg_type != BINDING_SUCCESS {
        return None;
    }
    if response[4..8] != MAGIC {
        return None;
    }
    if response[8..20] != expected_txid[..] {
        return None;
    }
    if response.len() < HEADER_LEN + msg_len {
        return None; // truncated datagram
    }
    let attrs = &response[HEADER_LEN..HEADER_LEN + msg_len];
    let mut i = 0usize;
    while i + 4 <= attrs.len() {
        let attr_type = u16::from_be_bytes([attrs[i], attrs[i + 1]]);
        let attr_len = u16::from_be_bytes([attrs[i + 2], attrs[i + 3]]) as usize;
        let body = attrs.get(i + 4..i + 4 + attr_len)?;
        if attr_type == ATTR_XOR_MAPPED {
            return decode_xor_mapped(expected_txid, body);
        }
        // attribute lengths are padded to 4-byte multiples
        i += 4 + attr_len.div_ceil(4) * 4;
    }
    None
}

fn decode_xor_mapped(txid: &[u8; 12], body: &[u8]) -> Option<SocketAddr> {
    if body.len() < 8 {
        return None;
    }
    let family = body[1];
    let port = u16::from_be_bytes([body[2], body[3]]) ^ 0x2112;
    let ip = match family {
        0x01 => {
            if body.len() < 8 {
                return None;
            }
            let b = [body[4], body[5], body[6], body[7]];
            IpAddr::V4(Ipv4Addr::new(
                b[0] ^ MAGIC[0],
                b[1] ^ MAGIC[1],
                b[2] ^ MAGIC[2],
                b[3] ^ MAGIC[3],
            ))
        }
        0x02 => {
            if body.len() < 20 {
                return None;
            }
            let mut mask = [0u8; 16];
            mask[..4].copy_from_slice(&MAGIC);
            mask[4..].copy_from_slice(txid);
            let mut oct = [0u8; 16];
            for (o, (a, m)) in oct.iter_mut().zip(body[4..20].iter().zip(mask.iter())) {
                *o = a ^ m;
            }
            IpAddr::V6(Ipv6Addr::from(oct))
        }
        _ => return None,
    };
    Some(SocketAddr::new(ip, port))
}

/// Derive a fresh transaction id from entropy material (blake3-truncated).
/// Unguessable to outsiders, collision-safe across rapid retries.
#[must_use]
pub fn fresh_txid(seed_material: &[u8]) -> [u8; 12] {
    let h = blake3::hash(seed_material);
    let mut t = [0u8; 12];
    t.copy_from_slice(&h.as_bytes()[..12]);
    t
}

/// One Binding round-trip over a synced socket: send, await the matching
/// response (bounded), return the reflexive address. Non-matching datagrams
/// (late responses from earlier transactions) are skipped, not fatal.
pub fn discover_public(
    sock: &UdpSocket,
    server: SocketAddr,
    timeout: Duration,
    seed_material: &[u8],
) -> std::io::Result<SocketAddr> {
    let txid = fresh_txid(seed_material);
    let req = binding_request(&txid);
    sock.send_to(&req, server)?;
    let deadline = std::time::Instant::now() + timeout;
    sock.set_read_timeout(Some(timeout))?;
    let mut buf = [0u8; 128];
    loop {
        let now = std::time::Instant::now();
        if now >= deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "stun: no binding response",
            ));
        }
        sock.set_read_timeout(Some(deadline - now))?;
        let (n, _from) = sock.recv_from(&mut buf)?;
        if let Some(addr) = parse_response(&buf[..n], &txid) {
            return Ok(addr);
        }
        // wrong txid/type: a stale response — keep waiting
    }
}

/// Async variant over a tokio socket (the swarm's punch socket).
pub async fn discover_public_async(
    sock: &AsyncUdpSocket,
    server: SocketAddr,
    timeout: Duration,
    seed_material: &[u8],
) -> std::io::Result<SocketAddr> {
    let txid = fresh_txid(seed_material);
    let req = binding_request(&txid);
    sock.send_to(&req, server).await?;
    let mut buf = [0u8; 128];
    loop {
        let recv = tokio::time::timeout(timeout, sock.recv_from(&mut buf));
        let (n, _from) = match recv.await {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => return Err(e),
            Err(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "stun: no binding response",
                ))
            }
        };
        if let Some(addr) = parse_response(&buf[..n], &txid) {
            return Ok(addr);
        }
    }
}

/// A loopback "fake STUN server" for deterministic tests: binds
/// `127.0.0.1:0`, answers every Binding Request with the OBSERVED source
/// address as XOR-MAPPED-ADDRESS (exactly what a real server does).
pub async fn spawn_loopback() -> std::io::Result<(SocketAddr, tokio::task::JoinHandle<()>)> {
    let sock = AsyncUdpSocket::bind("127.0.0.1:0").await?;
    let addr = sock.local_addr()?;
    let task = tokio::spawn(async move {
        let mut buf = [0u8; 128];
        loop {
            let Ok((n, from)) = sock.recv_from(&mut buf).await else {
                return; // socket dropped — test over
            };
            if n < HEADER_LEN || buf[4..8] != MAGIC || buf[0..2] != BINDING_REQUEST.to_be_bytes() {
                continue;
            }
            let mut txid = [0u8; 12];
            txid.copy_from_slice(&buf[8..20]);
            let resp = binding_success_response(&txid, from);
            let _ = sock.send_to(&resp, from).await;
        }
    });
    Ok((addr, task))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn txid() -> [u8; 12] {
        fresh_txid(b"test-seed")
    }

    #[test]
    fn request_header_layout() {
        let t = txid();
        let req = binding_request(&t);
        assert_eq!(req.len(), 20);
        assert_eq!(&req[0..2], &BINDING_REQUEST.to_be_bytes());
        assert_eq!(&req[2..4], &[0, 0], "no attributes → msg_len 0");
        assert_eq!(&req[4..8], &MAGIC);
        assert_eq!(&req[8..20], &t);
    }

    #[test]
    fn response_roundtrip_ipv4() {
        let t = txid();
        let addr: SocketAddr = "102.89.14.3:52144".parse().unwrap();
        let resp = binding_success_response(&t, addr);
        assert_eq!(parse_response(&resp, &t), Some(addr));
    }

    #[test]
    fn response_roundtrip_ipv6() {
        let t = txid();
        let addr: SocketAddr = "[2001:db8::1]:52144".parse().unwrap();
        let resp = binding_success_response(&t, addr);
        assert_eq!(parse_response(&resp, &t), Some(addr));
    }

    /// Regression: msg_len must be read from bytes 2..4 (the length field),
    /// never from the cookie bytes at 4..8.
    #[test]
    fn message_length_read_from_length_field_not_cookie() {
        let t = txid();
        let addr: SocketAddr = "88.99.1.2:5000".parse().unwrap();
        let resp = binding_success_response(&t, addr);
        // cookie = 21 12 A4 42 → a cookie-offset reader would compute a bogus
        // length (0x2112 = 8466) and fail the bounds check; the correct reader
        // sees the full attribute (4-byte header + 8-byte value = 12) and parses.
        assert_eq!(
            u16::from_be_bytes([resp[2], resp[3]]),
            12,
            "message length = full XOR-MAPPED-ADDRESS attribute (hdr+value)"
        );
        assert!(parse_response(&resp, &t).is_some());
    }

    #[test]
    fn wrong_txid_rejected() {
        let t = txid();
        let other = fresh_txid(b"other");
        let resp = binding_success_response(&t, "1.2.3.4:7".parse().unwrap());
        assert_eq!(parse_response(&resp, &other), None);
    }

    #[test]
    fn binding_request_type_rejected_as_response() {
        let t = txid();
        assert_eq!(parse_response(&binding_request(&t), &t), None);
    }

    #[test]
    fn truncated_response_rejected() {
        let t = txid();
        let resp = binding_success_response(&t, "1.2.3.4:7".parse().unwrap());
        assert_eq!(parse_response(&resp[..25], &t), None);
    }

    #[test]
    fn multiple_attributes_walked_until_xor_mapped() {
        let t = txid();
        let addr: SocketAddr = "77.1.2.3:999".parse().unwrap();
        // build: SOFTWARE attr (value 10 bytes, padded to 12) + XOR-MAPPED-ADDRESS
        let mut attrs = Vec::new();
        let software = b"cairn-test";
        attrs.extend_from_slice(&0x8022u16.to_be_bytes());
        attrs.extend_from_slice(&u16::try_from(software.len()).unwrap().to_be_bytes());
        attrs.extend_from_slice(software);
        attrs.push(0);
        attrs.push(0);
        let xm = encode_xor_mapped(&t, addr);
        attrs.extend_from_slice(&ATTR_XOR_MAPPED.to_be_bytes());
        attrs.extend_from_slice(&u16::try_from(xm.len()).unwrap().to_be_bytes());
        attrs.extend_from_slice(&xm);

        let mut resp = Vec::new();
        resp.extend_from_slice(&BINDING_SUCCESS.to_be_bytes());
        resp.extend_from_slice(&u16::try_from(attrs.len()).unwrap().to_be_bytes());
        resp.extend_from_slice(&MAGIC);
        resp.extend_from_slice(&t);
        resp.extend_from_slice(&attrs);
        assert_eq!(parse_response(&resp, &t), Some(addr));
    }

    /// multi_thread flavor: the sync client BLOCKS the calling thread, so the
    /// loopback server task needs a second worker to run concurrently
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn loopback_discovery() {
        let (server, task) = spawn_loopback().await.unwrap();
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let me = sock.local_addr().unwrap();
        let got = discover_public(&sock, server, Duration::from_secs(2), b"seed-1").unwrap();
        assert_eq!(got, me, "loopback STUN reflects our source address");
        let _ = discover_public(&sock, server, Duration::from_secs(2), b"seed-2");
        task.abort();
    }

    #[tokio::test]
    async fn loopback_discovery_async() {
        let (server, task) = spawn_loopback().await.unwrap();
        let sock = AsyncUdpSocket::bind("127.0.0.1:0").await.unwrap();
        let me = sock.local_addr().unwrap();
        let got = discover_public_async(&sock, server, Duration::from_secs(2), b"aseed-1")
            .await
            .unwrap();
        assert_eq!(got, me);
        task.abort();
    }

    #[tokio::test]
    async fn discovery_timeout_when_server_silent() {
        // bind a socket that never answers
        let silent = AsyncUdpSocket::bind("127.0.0.1:0").await.unwrap();
        let server = silent.local_addr().unwrap();
        let sock = AsyncUdpSocket::bind("127.0.0.1:0").await.unwrap();
        let r = discover_public_async(&sock, server, Duration::from_millis(150), b"s").await;
        assert!(r.is_err(), "silent server must time out");
    }
}
