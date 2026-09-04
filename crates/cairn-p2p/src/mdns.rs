//! Minimal mDNS (RFC 6762 subset) LAN discovery for the cairn signal
//! rendezvous (ADR-0019 §4).
//!
//! What this buys the user: today a swarm joiner needs BOTH the join code
//! AND the host's signal address (`--swarm-signal host:port`). On a trusted
//! LAN the address is discoverable — the host's signal server announces a
//! beacon, and a joiner who holds the code finds the beacon WITHOUT any
//! address configuration ("zero-config LAN join").
//!
//! Trust model (deliberately conservative):
//! - The beacon TXT record carries ONLY a 16-hex FINGERPRINT of the join
//!   code — `blake3("cairn-mdns/v1" ‖ normalized-code)`. The code itself
//!   never travels; a wrong-code joiner's fingerprint does not match the
//!   beacon, so beacons are filtered client-side.
//! - mDNS is spoofable by design. A forged beacon can only REDIRECT a
//!   joiner to an attacker's fake signal server — and that fails exactly
//!   like a wrong `--swarm-signal` today: every registration is HMAC'd with
//!   the cluster key derived from the code (ADR-0017 §7), the fake server
//!   cannot produce valid HMAC'd member cards, peers fail-closed. The
//!   discovery layer NEVER becomes an admission layer.
//! - Discovery is OPTIONAL plumbing: `--swarm-signal` remains the explicit
//!   path; beacons only fill in the address.
//!
//! Engineering notes: hand-rolled DNS packet codec (zero new dependencies —
//! the crate's rule from round 14); names are emitted WITHOUT compression
//! pointers (legal — compression is optional, receivers must accept
//! uncompressed names); only the record types we actually use are decoded
//! (PTR/SRV/TXT); anything unknown is skipped, per RFC 6762's advice for
//! minimal responders. The [`MdnsTransport`] seam mirrors stun.rs: the real
//! transport is multicast UDP 224.0.0.251:5353; tests use an in-memory bus.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::time::Duration;

/// The cairn service name (DNS-SD style, .local mDNS domain).
pub const SERVICE: &str = "_cairn._udp.local";
/// Multicast group + port (RFC 6762 fixed values).
pub const MDNS_GROUP: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 251);
pub const MDNS_PORT: u16 = 5353;

/// One discovered beacon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Beacon {
    /// The signal server's address (source IP of the response + SRV port).
    pub signal_addr: SocketAddr,
    /// TXT map (fp, v, p).
    pub txt: HashMap<String, String>,
}

/// Fingerprint of a join code: 16 hex chars, derivable by any code holder.
#[must_use]
pub fn code_fingerprint(normalized_code: &str) -> String {
    let mut material = b"cairn-mdns/v1".to_vec();
    material.extend_from_slice(normalized_code.as_bytes());
    blake3::hash(&material).to_hex()[..16].to_string()
}

// ---------------------------------------------------------------------------
// DNS wire codec (the subset mDNS needs)
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Header {
    id: u16,
    flags: u16,
    qdcount: u16,
    ancount: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Question {
    Ptr { name: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Record {
    Ptr {
        name: String,
        target: String,
    },
    Srv {
        name: String,
        port: u16,
    },
    Txt {
        name: String,
        kv: Vec<(String, String)>,
    },
}

/// Encode a name as length-prefixed labels (no compression).
fn put_name(out: &mut Vec<u8>, name: &str) {
    for label in name.trim_end_matches('.').split('.') {
        debug_assert!(label.len() < 64, "mDNS label too long: {label}");
        out.push(label.len() as u8);
        out.extend_from_slice(label.as_bytes());
    }
    out.push(0);
}

/// Decode a (possibly compressed) name starting at `pos`; returns
/// (name, next_position_after_name). Compression pointers ARE parsed — real
/// responders (Avahi, mDNSResponder) emit them.
fn get_name(pkt: &[u8], pos: usize) -> Option<(String, usize)> {
    let mut labels: Vec<String> = Vec::new();
    let mut p = pos;
    let mut jumped = false;
    let mut end = None;
    let mut hops = 0;
    loop {
        let len = *pkt.get(p)?;
        if len == 0 {
            if !jumped {
                end = Some(p + 1);
            }
            break;
        }
        if len & 0xC0 == 0xC0 {
            // compression pointer
            let ptr = (((len & 0x3F) as usize) << 8) | *pkt.get(p + 1)? as usize;
            if !jumped {
                end = Some(p + 2);
            }
            hops += 1;
            if hops > 8 {
                return None;
            }
            p = ptr;
            jumped = true;
            continue;
        }
        let l = len as usize;
        let s = pkt.get(p + 1..p + 1 + l)?;
        labels.push(String::from_utf8_lossy(s).into_owned());
        p += 1 + l;
    }
    let name = labels.join(".");
    let next = end.unwrap_or(pkt.len());
    Some((name, next))
}

fn put_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_be_bytes());
}

fn get_u16(pkt: &[u8], pos: usize) -> Option<u16> {
    let b = pkt.get(pos..pos + 2)?;
    Some(u16::from_be_bytes([b[0], b[1]]))
}

fn encode_packet(header: &Header, q: Option<&Question>, answers: &[Record]) -> Vec<u8> {
    let mut out = Vec::with_capacity(256);
    put_u16(&mut out, header.id);
    put_u16(&mut out, header.flags);
    put_u16(&mut out, header.qdcount);
    put_u16(&mut out, header.ancount);
    put_u16(&mut out, 0); // nscount
    put_u16(&mut out, 0); // arcount
    if let Some(Question::Ptr { name }) = q {
        put_name(&mut out, name);
        put_u16(&mut out, 12); // PTR
        put_u16(&mut out, 1); // class IN
    }
    for r in answers {
        match r {
            Record::Ptr { name, target } => {
                put_name(&mut out, name);
                put_u16(&mut out, 12);
                put_u16(&mut out, 1 | 0x8000); // cache-flush (RFC 6762 §10.2)
                out.extend_from_slice(&4500u32.to_be_bytes()); // ttl (4 bytes)
                let mut rd = Vec::new();
                put_name(&mut rd, target);
                put_u16(&mut out, rd.len() as u16);
                out.extend_from_slice(&rd);
            }
            Record::Srv { name, port } => {
                put_name(&mut out, name);
                put_u16(&mut out, 33); // SRV
                put_u16(&mut out, 1 | 0x8000);
                out.extend_from_slice(&4500u32.to_be_bytes()); // ttl
                let mut rd = Vec::new();
                put_u16(&mut rd, 0); // priority
                put_u16(&mut rd, 0); // weight
                put_u16(&mut rd, *port);
                put_name(&mut rd, name); // target = the instance itself
                put_u16(&mut out, rd.len() as u16);
                out.extend_from_slice(&rd);
            }
            Record::Txt { name, kv } => {
                put_name(&mut out, name);
                put_u16(&mut out, 16); // TXT
                put_u16(&mut out, 1 | 0x8000);
                out.extend_from_slice(&4500u32.to_be_bytes()); // ttl
                let mut rd = Vec::new();
                for (k, v) in kv {
                    let entry = format!("{k}={v}");
                    rd.push(entry.len() as u8);
                    rd.extend_from_slice(entry.as_bytes());
                }
                if rd.is_empty() {
                    rd.push(0);
                }
                put_u16(&mut out, rd.len() as u16);
                out.extend_from_slice(&rd);
            }
        }
    }
    out
}

fn parse_packet(pkt: &[u8]) -> Option<(Header, Option<Question>, Vec<Record>)> {
    if pkt.len() < 12 {
        return None;
    }
    let header = Header {
        id: get_u16(pkt, 0)?,
        flags: get_u16(pkt, 2)?,
        qdcount: get_u16(pkt, 4)?,
        ancount: get_u16(pkt, 6)?,
    };
    let mut pos = 12;
    let mut question = None;
    if header.qdcount > 0 {
        let (name, next) = get_name(pkt, pos)?;
        let qtype = get_u16(pkt, next)?;
        pos = next + 4;
        if qtype == 12 {
            question = Some(Question::Ptr { name });
        }
    }
    let mut answers = Vec::new();
    for _ in 0..header.ancount {
        let (name, mut next) = get_name(pkt, pos)?;
        let rtype = get_u16(pkt, next)?;
        next += 8; // class(2) + ttl(4)
        let rdlen = get_u16(pkt, next)? as usize;
        next += 2;
        let rd = pkt.get(next..next + rdlen)?;
        match rtype {
            12 => {
                let (target, _) = get_name(rd, 0)?;
                answers.push(Record::Ptr { name, target });
            }
            33 => {
                // priority(2) weight(2) port(2) target(...)
                let port = get_u16(rd, 4)?;
                answers.push(Record::Srv { name, port });
            }
            16 => {
                let mut kv = Vec::new();
                let mut p = 0;
                while p < rd.len() {
                    let l = rd[p] as usize;
                    p += 1;
                    if l == 0 || p + l > rd.len() {
                        break;
                    }
                    let entry = String::from_utf8_lossy(&rd[p..p + l]).into_owned();
                    if let Some((k, v)) = entry.split_once('=') {
                        kv.push((k.to_string(), v.to_string()));
                    }
                    p += l;
                }
                answers.push(Record::Txt { name, kv });
            }
            _ => {} // unknown record types are skipped (RFC 6762 minimal reader)
        }
        pos = next + rdlen;
    }
    Some((header, question, answers))
}

// ---------------------------------------------------------------------------
// Transport seam (the stun.rs pattern)
// ---------------------------------------------------------------------------

/// Send/receive seam so tests run on an in-memory bus while production uses
/// multicast UDP.
pub trait MdnsTransport: Send + Sync {
    fn send_to(&self, pkt: &[u8], to: SocketAddr) -> std::io::Result<()>;
    fn recv_from(&self) -> std::io::Result<(Vec<u8>, SocketAddr)>;
}

/// Production transport: multicast UDP on 224.0.0.251:5353.
pub struct UdpMdns {
    sock: std::net::UdpSocket,
}

impl UdpMdns {
    pub fn bind() -> std::io::Result<Self> {
        let sock = std::net::UdpSocket::bind(SocketAddrV4::new(MDNS_GROUP, MDNS_PORT))?;
        // best-effort membership; broadcast-only hosts still send
        let _ = sock.set_multicast_loop_v4(true);
        // bounded reads so browse's listen loop and runtime shutdown are
        // never held hostage by a silent network
        sock.set_read_timeout(Some(Duration::from_millis(200)))?;
        Ok(Self { sock })
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.sock.local_addr()
    }
}

impl MdnsTransport for UdpMdns {
    fn send_to(&self, pkt: &[u8], to: SocketAddr) -> std::io::Result<()> {
        self.sock.send_to(pkt, to)?;
        Ok(())
    }

    fn recv_from(&self) -> std::io::Result<(Vec<u8>, SocketAddr)> {
        let mut buf = [0u8; 1500];
        let (n, from) = self.sock.recv_from(&mut buf)?;
        Ok((buf[..n].to_vec(), from))
    }
}

/// In-memory bus for tests (and for loopback smoke where multicast is
/// unavailable): every packet sent to the group address is delivered to
/// every OTHER participant.
type Inbox = Arc<std::sync::Mutex<Vec<(Vec<u8>, SocketAddr)>>>;

pub struct FakeBus {
    members: Arc<std::sync::Mutex<Vec<SocketAddr>>>,
    inbox: Inbox,
    next_port: std::sync::atomic::AtomicU16,
}

impl FakeBus {
    pub fn new() -> Self {
        Self {
            members: Arc::new(std::sync::Mutex::new(Vec::new())),
            inbox: Arc::new(std::sync::Mutex::new(Vec::new())),
            next_port: std::sync::atomic::AtomicU16::new(40000),
        }
    }

    /// Join the bus with a synthetic member address.
    pub fn join(&self) -> FakeEndpoint {
        let port = self
            .next_port
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let addr = SocketAddr::new(
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, (port % 200) as u8)),
            port,
        );
        self.members.lock().unwrap().push(addr);
        FakeEndpoint {
            addr,
            bus_members: Arc::clone(&self.members),
            bus_inbox: Arc::clone(&self.inbox),
        }
    }
}

impl Default for FakeBus {
    fn default() -> Self {
        Self::new()
    }
}

pub struct FakeEndpoint {
    addr: SocketAddr,
    bus_members: Arc<std::sync::Mutex<Vec<SocketAddr>>>,
    bus_inbox: Inbox,
}

impl MdnsTransport for FakeEndpoint {
    fn send_to(&self, pkt: &[u8], to: SocketAddr) -> std::io::Result<()> {
        if to.port() != MDNS_PORT {
            return Ok(());
        }
        // std mutexes: the trait is sync, and send_to runs in async contexts
        let members = self.bus_members.lock().unwrap();
        let mut inbox = self.bus_inbox.lock().unwrap();
        for m in members.iter() {
            if *m != self.addr {
                inbox.push((pkt.to_vec(), self.addr));
            }
        }
        Ok(())
    }

    fn recv_from(&self) -> std::io::Result<(Vec<u8>, SocketAddr)> {
        // bounded wait: mirrors the real transport's read timeout so
        // cancelled browse loops (and runtime shutdown) never leak a spinner
        let deadline = std::time::Instant::now() + Duration::from_millis(200);
        loop {
            {
                let mut inbox = self.bus_inbox.lock().unwrap();
                if let Some(idx) = (0..inbox.len()).find(|_| true) {
                    let (pkt, from) = inbox.remove(idx);
                    return Ok((pkt, from));
                }
            }
            if std::time::Instant::now() >= deadline {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "mdns fake: quiet",
                ));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

// ---------------------------------------------------------------------------
// Beacon announce + browse
// ---------------------------------------------------------------------------

/// TXT payload for our service.
fn txt_entries(fp: &str, port: u16) -> Vec<(String, String)> {
    vec![
        ("v".into(), "1".into()),
        ("fp".into(), fp.into()),
        ("p".into(), port.to_string()),
    ]
}

fn instance_name(fp: &str) -> String {
    format!("cairn-{fp}.{SERVICE}")
}

/// One-shot beacon: the unsolicited response a host's signal server sends.
/// Announces the instance (PTR), its SRV (port) and the TXT fingerprint.
#[must_use]
pub fn beacon_packet(fp: &str, signal_port: u16) -> Vec<u8> {
    let inst = instance_name(fp);
    encode_packet(
        &Header {
            id: 0,
            flags: 0x8400, // response, authoritative
            qdcount: 0,
            ancount: 3,
        },
        None,
        &[
            Record::Ptr {
                name: SERVICE.into(),
                target: inst.clone(),
            },
            Record::Srv {
                name: inst.clone(),
                port: signal_port,
            },
            Record::Txt {
                name: inst,
                kv: txt_entries(fp, signal_port),
            },
        ],
    )
}

/// Query packet for the cairn service (PTR).
#[must_use]
pub fn query_packet() -> Vec<u8> {
    encode_packet(
        &Header {
            id: 0,
            flags: 0x0000,
            qdcount: 1,
            ancount: 0,
        },
        Some(&Question::Ptr {
            name: SERVICE.into(),
        }),
        &[],
    )
}

/// Is this packet a PTR question for our service?
fn is_service_query(pkt: &[u8]) -> bool {
    matches!(parse_packet(pkt), Some((_, Some(Question::Ptr { name }), _)) if name == SERVICE)
}

/// Announcer task: answers service queries with the beacon. Run one per
/// signal server; `tx` is the production transport (or a fake).
pub async fn spawn_announcer(
    tx: Arc<dyn MdnsTransport>,
    fp: String,
    signal_port: u16,
    shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let mut shutdown = shutdown;
    let group = SocketAddr::new(IpAddr::V4(MDNS_GROUP), MDNS_PORT);
    loop {
        tokio::select! {
            _ = shutdown.changed() => return,
            res = tokio::task::spawn_blocking({
                let tx = Arc::clone(&tx);
                move || tx.recv_from()
            }) => {
                match res {
                    Ok(Ok((pkt, _from))) => {
                        if is_service_query(&pkt) {
                            let _ = tx.send_to(&beacon_packet(&fp, signal_port), group);
                        }
                    }
                    _ => tokio::time::sleep(Duration::from_millis(50)).await,
                }
            }
        }
    }
}

/// Browse the LAN for beacons whose fingerprint matches `fp`. Collects for
/// `timeout`, dedups by (addr, port). The caller supplies a transport bound
/// to the mDNS group.
pub async fn browse(tx: Arc<dyn MdnsTransport>, fp: &str, timeout: Duration) -> Vec<Beacon> {
    let group = SocketAddr::new(IpAddr::V4(MDNS_GROUP), MDNS_PORT);
    let _ = tx.send_to(&query_packet(), group);
    // unsolicited beacons (the periodic announcements) also count
    let deadline = tokio::time::Instant::now() + timeout;
    let mut found: Vec<Beacon> = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return found;
        }
        let recv = tokio::task::spawn_blocking({
            let tx = Arc::clone(&tx);
            move || tx.recv_from()
        });
        match tokio::time::timeout(remaining, recv).await {
            Ok(Ok(Err(_))) | Ok(Err(_)) => {} // quiet read: keep listening until the deadline
            Ok(Ok(Ok((pkt, from)))) => {
                if let Some((_, _, answers)) = parse_packet(&pkt) {
                    let mut srv: Option<u16> = None;
                    let mut txt: HashMap<String, String> = HashMap::new();
                    let mut matching = false;
                    for a in &answers {
                        match a {
                            Record::Srv { port, .. } => srv = Some(*port),
                            Record::Txt { kv, .. } => {
                                for (k, v) in kv {
                                    if k == "fp" && v == fp {
                                        matching = true;
                                    }
                                    txt.insert(k.clone(), v.clone());
                                }
                            }
                            Record::Ptr { .. } => {}
                        }
                    }
                    if matching {
                        if let Some(p) = srv.or_else(|| txt.get("p").and_then(|p| p.parse().ok())) {
                            let signal_addr = SocketAddr::new(from.ip(), p);
                            if !found.iter().any(|b| b.signal_addr == signal_addr) {
                                found.push(Beacon { signal_addr, txt });
                            }
                        }
                    }
                }
            }
            _ => return found,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codec_roundtrips_ptr_srv_txt() {
        let pkt = beacon_packet("deadbeefcafebabe", 17780);
        let (h, q, answers) = parse_packet(&pkt).unwrap();
        assert_eq!(h.ancount, 3);
        assert!(q.is_none());
        assert_eq!(answers.len(), 3);
        assert!(matches!(&answers[0], Record::Ptr { target, .. }
            if target == "cairn-deadbeefcafebabe._cairn._udp.local"));
        assert!(matches!(&answers[1], Record::Srv { port: 17780, .. }));
        assert!(
            matches!(&answers[2], Record::Txt { kv, .. } if kv.contains(&("fp".to_string(), "deadbeefcafebabe".to_string())))
        );
    }

    #[test]
    fn query_packet_parses_as_service_question() {
        let pkt = query_packet();
        assert!(is_service_query(&pkt));
    }

    #[test]
    fn names_with_compression_pointer_decode() {
        // hand-build a packet whose answer name uses a compression pointer
        // back into the question (what real responders emit)
        let mut pkt = Vec::new();
        put_u16(&mut pkt, 0); // id
        put_u16(&mut pkt, 0); // flags
        put_u16(&mut pkt, 1); // qdcount: one question
        put_u16(&mut pkt, 1); // ancount: one answer
        put_u16(&mut pkt, 0); // nscount
        put_u16(&mut pkt, 0); // arcount
        put_name(&mut pkt, SERVICE); // question name
        put_u16(&mut pkt, 12);
        put_u16(&mut pkt, 1);
        let qname_len = pkt.len();
        // answer: name = pointer to offset 12
        pkt.push(0xC0);
        pkt.push(12);
        put_u16(&mut pkt, 12); // PTR
        put_u16(&mut pkt, 1);
        put_u16(&mut pkt, 0); // ttl hi
        put_u16(&mut pkt, 120); // ttl lo
        let mut rd = Vec::new();
        put_name(&mut rd, "cairn-x._cairn._udp.local");
        put_u16(&mut pkt, rd.len() as u16);
        pkt.extend_from_slice(&rd);
        let _ = qname_len;
        let (h, q, answers) = parse_packet(&pkt).unwrap();
        assert_eq!(h.qdcount, 1);
        assert!(matches!(q, Some(Question::Ptr { ref name }) if name == SERVICE));
        assert!(matches!(&answers[0], Record::Ptr { name, target }
            if name == SERVICE && target == "cairn-x._cairn._udp.local"));
    }

    #[test]
    fn code_fingerprint_is_stable_and_short() {
        assert_eq!(
            code_fingerprint("enr-1234-ABCD-WXYZ-5678-EFGH"),
            code_fingerprint("enr-1234-ABCD-WXYZ-5678-EFGH")
        );
        assert_ne!(
            code_fingerprint("enr-1234-ABCD-WXYZ-5678-EFGH"),
            code_fingerprint("enr-1234-ABCD-WXYZ-5678-EFGI")
        );
        assert_eq!(code_fingerprint("x").len(), 16);
    }

    #[tokio::test]
    async fn browse_finds_matching_beacon_and_filters_foreign() {
        let bus = Arc::new(FakeBus::new());
        let host = bus.join();
        let joiner = bus.join();
        let fp = code_fingerprint("test-code-1234");

        // host announces (unsolicited beacon to the group)
        let host_tx = Arc::new(host);
        {
            let t = Arc::clone(&host_tx);
            let pkt = beacon_packet(&fp, 17781);
            let group = SocketAddr::new(IpAddr::V4(MDNS_GROUP), MDNS_PORT);
            t.send_to(&pkt, group).unwrap();
        }
        // a foreign-code beacon on the same bus must be ignored
        {
            let t = Arc::clone(&host_tx);
            let pkt = beacon_packet("ffffffffffffffff", 19999);
            let group = SocketAddr::new(IpAddr::V4(MDNS_GROUP), MDNS_PORT);
            t.send_to(&pkt, group).unwrap();
        }

        let found = browse(Arc::new(joiner), &fp, Duration::from_millis(300)).await;
        assert_eq!(found.len(), 1, "exactly the matching beacon");
        let b = &found[0];
        assert_eq!(b.signal_addr.port(), 17781);
        assert_eq!(b.signal_addr.ip(), host_tx.addr.ip());
        assert_eq!(b.txt.get("fp").map(String::as_str), Some(fp.as_str()));
    }
}
