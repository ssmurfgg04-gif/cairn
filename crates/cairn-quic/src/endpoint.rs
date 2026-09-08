//! QUIC endpoint setup: self-signed identity + tuned transport.
//!
//! Studios run their own signal server, so self-signed certs pinned by the
//! join-code fingerprint are the right model (no public CA needed on LAN).
//! WAN deployments should terminate behind their own CA; the fingerprint
//! check in `transport::connect` stays either way.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use cairn_core::{CairnError, ErrorKind};

/// Self-signed server identity (cert DER + key DER) for one swarm host.
pub struct ServerIdentity {
    pub cert_der: Vec<u8>,
    pub key_der: Vec<u8>,
}

impl ServerIdentity {
    /// Generate a fresh self-signed identity for `subject` (e.g. signal host).
    pub fn generate(subject: &str) -> Result<Self, CairnError> {
        use rcgen::CertificateParams;
        let mut params = CertificateParams::new(vec![subject.to_owned()])
            .map_err(|e| CairnError::new(ErrorKind::Internal, format!("rcgen params: {e}")))?;
        params.is_ca = rcgen::IsCa::NoCa;
        let key = rcgen::KeyPair::generate()
            .map_err(|e| CairnError::new(ErrorKind::Internal, format!("rcgen key: {e}")))?;
        let cert = params
            .self_signed(&key)
            .map_err(|e| CairnError::new(ErrorKind::Internal, format!("rcgen sign: {e}")))?;
        Ok(Self {
            cert_der: cert.der().to_vec(),
            key_der: key.serialize_der(),
        })
    }

    /// BLAKE3 fingerprint peers pin against (exchanged via join-code channel).
    pub fn fingerprint(&self) -> String {
        cairn_core::hash::Hash::of(&self.cert_der).hex()
    }
}

pub(crate) fn transport_config() -> Arc<quinn::TransportConfig> {
    let mut t = quinn::TransportConfig::default();
    // Keep NAT mappings alive through quiet stretches (consumer UDP mappings
    // die in 30-120s); 25s matches the WireGuard PersistentKeepalive rule.
    t.keep_alive_interval(Some(Duration::from_secs(25)));
    t.max_idle_timeout(Some(
        quinn::IdleTimeout::try_from(Duration::from_secs(120)).unwrap(),
    ));
    // Chunk-friendly windows: 8 MiB stream window so a 4 MiB chunk never
    // stalls mid-stream on high-BDP links.
    t.stream_receive_window(8_388_608u32.into());
    t.max_concurrent_bidi_streams(128u32.into());
    Arc::new(t)
}

/// A bound QUIC endpoint (server, client, or both).
#[derive(Clone)]
pub struct QuicEndpoint {
    endpoint: quinn::Endpoint,
}

impl QuicEndpoint {
    /// Bind a server endpoint presenting `identity`.
    pub fn server(bind: SocketAddr, identity: &ServerIdentity) -> Result<Self, CairnError> {
        crate::ensure_crypto_provider();
        use rustls::pki_types::{CertificateDer, PrivateKeyDer};
        let cert = CertificateDer::from(identity.cert_der.clone());
        let key = PrivateKeyDer::Pkcs8(identity.key_der.clone().into());
        let mut server_config = quinn::ServerConfig::with_single_cert(vec![cert], key)
            .map_err(|e| CairnError::new(ErrorKind::Internal, format!("quic server: {e}")))?;
        server_config.transport_config(transport_config());
        let endpoint = quinn::Endpoint::server(server_config, bind)
            .map_err(|e| CairnError::new(ErrorKind::Io, format!("quic bind: {e}")))?;
        Ok(Self { endpoint })
    }

    /// Bind an ephemeral client endpoint (no cert of its own).
    pub fn client() -> Result<Self, CairnError> {
        crate::ensure_crypto_provider();
        let roots = rustls::RootCertStore::empty();
        let rustls_cfg = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let quic_cfg = quinn::crypto::rustls::QuicClientConfig::try_from(Arc::new(rustls_cfg))
            .map_err(|e| CairnError::new(ErrorKind::Internal, format!("quic tls: {e}")))?;
        let mut client_config = quinn::ClientConfig::new(Arc::new(quic_cfg));
        client_config.transport_config(transport_config());
        let mut endpoint = quinn::Endpoint::client("0.0.0.0:0".parse().unwrap())
            .map_err(|e| CairnError::new(ErrorKind::Io, format!("quic client bind: {e}")))?;
        endpoint.set_default_client_config(client_config);
        Ok(Self { endpoint })
    }

    /// Ephemeral client with accept-any TLS (relay hop only — see
    /// `trust` docs). The default config is used for every dial.
    pub fn client_insecure() -> Result<Self, CairnError> {
        crate::ensure_crypto_provider();
        let mut endpoint = quinn::Endpoint::client("0.0.0.0:0".parse().unwrap())
            .map_err(|e| CairnError::new(ErrorKind::Io, format!("quic client bind: {e}")))?;
        endpoint.set_default_client_config(crate::trust::quinn_client_config());
        Ok(Self { endpoint })
    }

    pub fn inner(&self) -> &quinn::Endpoint {
        &self.endpoint
    }

    /// Steal the inner endpoint (shutdown responsibility moves along).
    pub fn into_inner(self) -> quinn::Endpoint {
        self.endpoint
    }
}
