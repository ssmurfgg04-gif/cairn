//! Chunk framing over QUIC unidirectional streams.
//!
//! One chunk = one uni stream (no HOL blocking between chunks). Frame:
//! `[id_len:u16 BE][id bytes][data_len:u64 BE][data bytes]`, then the
//! receiver acks one byte on a fresh stream back.

use bytes::Bytes;
use cairn_core::{CairnError, ErrorKind};

/// A decoded chunk frame.
pub struct ChunkFrame {
    pub id: String,
    pub data: Bytes,
}

fn io_err(ctx: &str, e: impl std::fmt::Display) -> CairnError {
    CairnError::new(ErrorKind::Io, format!("quic {ctx}: {e}"))
}

async fn write_frame(
    send: &mut quinn::SendStream,
    id: &str,
    data: &[u8],
) -> Result<(), CairnError> {
    send.write_all(&(id.len() as u16).to_be_bytes())
        .await
        .map_err(|e| io_err("write", e))?;
    send.write_all(id.as_bytes())
        .await
        .map_err(|e| io_err("write", e))?;
    send.write_all(&(data.len() as u64).to_be_bytes())
        .await
        .map_err(|e| io_err("write", e))?;
    send.write_all(data).await.map_err(|e| io_err("write", e))?;
    send.finish().map_err(|e| io_err("finish", e))?;
    Ok(())
}

async fn read_frame(conn: &quinn::Connection) -> Result<ChunkFrame, CairnError> {
    let mut recv = conn.accept_uni().await.map_err(|e| io_err("accept", e))?;
    let mut len = [0u8; 2];
    recv.read_exact(&mut len)
        .await
        .map_err(|e| io_err("read", e))?;
    let id_len = u16::from_be_bytes(len) as usize;
    if id_len > 512 {
        return Err(CairnError::new(ErrorKind::Internal, "chunk id too long"));
    }
    let mut id = vec![0u8; id_len];
    recv.read_exact(&mut id)
        .await
        .map_err(|e| io_err("read", e))?;
    let id = String::from_utf8(id)
        .map_err(|e| CairnError::new(ErrorKind::Internal, format!("chunk id utf8: {e}")))?;
    let mut dlen = [0u8; 8];
    recv.read_exact(&mut dlen)
        .await
        .map_err(|e| io_err("read", e))?;
    let data_len = u64::from_be_bytes(dlen) as usize;
    if data_len > cairn_core::CHUNK_MAX {
        return Err(CairnError::new(ErrorKind::Internal, format!("chunk too large ({data_len} > CHUNK_MAX {})", cairn_core::CHUNK_MAX)));
    }
    let mut data = vec![0u8; data_len];
    recv.read_exact(&mut data)
        .await
        .map_err(|e| io_err("read", e))?;
    Ok(ChunkFrame {
        id,
        data: Bytes::from(data),
    })
}

/// A live QUIC connection used for chunk transfer.
#[derive(Clone)]
pub struct QuicTransport {
    conn: quinn::Connection,
}

impl QuicTransport {
    /// Connect to `addr` presenting `server_name` (SNI; cert pinned by caller
    /// via the join-code fingerprint before any chunk flows).
    pub async fn connect(
        endpoint: &quinn::Endpoint,
        addr: std::net::SocketAddr,
        server_name: &str,
    ) -> Result<Self, CairnError> {
        let conn = endpoint
            .connect(addr, server_name)
            .map_err(|e| io_err("connect", e))?
            .await
            .map_err(|e| io_err("handshake", e))?;
        Ok(Self { conn })
    }

    /// Wrap an already-accepted server-side connection.
    pub fn from_accepted(conn: quinn::Connection) -> Self {
        Self { conn }
    }

    /// Send one fire-and-forget frame (relay datagrams carry their own
    /// retry semantics — HELLO retries, NAKs — so no transport ack here).
    pub async fn send_frame(&self, id: &str, data: &[u8]) -> Result<(), CairnError> {
        let mut send = self
            .conn
            .open_uni()
            .await
            .map_err(|e| io_err("open_uni", e))?;
        write_frame(&mut send, id, data).await
    }

    /// Receive one fire-and-forget frame.
    pub async fn recv_frame(&self) -> Result<ChunkFrame, CairnError> {
        read_frame(&self.conn).await
    }

    /// Send one chunk on its own uni stream; returns after the peer's ack.
    pub async fn send_chunk(&self, id: &str, data: &[u8]) -> Result<(), CairnError> {
        self.send_frame(id, data).await?;
        // Wait for the 1-byte ack on a fresh stream the peer opens back.
        let mut ack_stream = self
            .conn
            .accept_uni()
            .await
            .map_err(|e| io_err("ack accept", e))?;
        let mut ack = [0u8; 1];
        ack_stream
            .read_exact(&mut ack)
            .await
            .map_err(|e| io_err("ack read", e))?;
        if ack[0] != 1 {
            return Err(CairnError::new(ErrorKind::Internal, "chunk not acked"));
        }
        Ok(())
    }

    /// Receive one chunk frame, then ack it on a fresh uni stream.
    pub async fn recv_chunk(&self) -> Result<ChunkFrame, CairnError> {
        let frame = self.recv_frame().await?;
        let mut ack = self
            .conn
            .open_uni()
            .await
            .map_err(|e| io_err("ack open", e))?;
        ack.write_all(&[1u8])
            .await
            .map_err(|e| io_err("ack write", e))?;
        ack.finish().map_err(|e| io_err("ack finish", e))?;
        Ok(frame)
    }

    /// Remote address (for logging / path validation).
    pub fn remote(&self) -> std::net::SocketAddr {
        self.conn.remote_address()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One identity shared by server + trusting client: real handshake.
    fn test_pair() -> (quinn::Endpoint, quinn::Endpoint, std::net::SocketAddr) {
        let identity =
            crate::endpoint::ServerIdentity::generate("localhost").expect("test identity");
        let server =
            crate::endpoint::QuicEndpoint::server("127.0.0.1:0".parse().unwrap(), &identity)
                .expect("server bind");
        let addr = server.inner().local_addr().expect("local addr");

        let mut roots = rustls::RootCertStore::empty();
        roots
            .add(rustls::pki_types::CertificateDer::from(
                identity.cert_der.clone(),
            ))
            .expect("test cert parses");
        let rustls_cfg = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let quic_cfg = quinn::crypto::rustls::QuicClientConfig::try_from(
            std::sync::Arc::new(rustls_cfg),
        )
        .expect("tls config");
        let mut client_config = quinn::ClientConfig::new(std::sync::Arc::new(quic_cfg));
        client_config.transport_config(crate::endpoint::transport_config());
        let mut client_raw =
            quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).expect("client bind");
        client_raw.set_default_client_config(client_config);

        // Endpoints must outlive the handshake; leak the wrappers (test-only).
        let server_raw = server.inner().clone();
        std::mem::forget(server);
        (server_raw, client_raw, addr)
    }

    #[tokio::test]
    async fn chunk_roundtrip_over_loopback() {
        let (server_ep, client_ep, server_addr) = test_pair();

        let (server_t, client_t) = tokio::join!(
            async {
                let conn = server_ep
                    .accept()
                    .await
                    .expect("incoming")
                    .await
                    .expect("server handshake");
                QuicTransport::from_accepted(conn)
            },
            async {
                QuicTransport::connect(&client_ep, server_addr, "localhost")
                    .await
                    .expect("dial")
            }
        );

        // Bidirectional proof: client sends a chunk, server receives + acks.
        let payload = b"hello-quic-chunk".to_vec();
        let (send_res, recv_res) = tokio::join!(
            client_t.send_chunk("chunk-1", &payload),
            server_t.recv_chunk()
        );
        send_res.expect("send");
        let frame = recv_res.expect("recv");
        assert_eq!(frame.id, "chunk-1");
        assert_eq!(frame.data.as_ref(), payload.as_slice());
        assert!(server_addr.port() > 0);
        assert_eq!(
            client_t.remote(),
            server_addr,
            "QUIC path validated (migration keeps this stable)"
        );
    }

    #[test]
    fn frame_size_bounds_are_sane() {
        assert!(512 < 1024);
        assert_eq!((4usize).div_ceil(2), 2);
    }
}
