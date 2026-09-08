//! TLS trust for swarm QUIC links.
//!
//! The relay is an OPAQUE pass-through: every datagram it carries is
//! end-to-end XChaCha20-Poly1305 sealed between the two peers (session.rs),
//! so the relay learns routing headers and nothing else. TLS on the
//! peer-relay QUIC hop therefore buys transport privacy only — and the
//! relay's cert is self-signed, distributed out of band (same channel as
//! the join code). `AcceptAny` skips chain validation while KEEPING the
//! TLS 1.3 encryption + handshake; peer authentication stays where it
//! belongs (the sealed frames + join-code-gated signal admission).
//!
//! If your threat model needs relay authentication too, pin
//! `ServerIdentity::fingerprint` here instead (one-line change).

use std::sync::Arc;

#[derive(Debug)]
struct AcceptAny;

impl rustls::client::danger::ServerCertVerifier for AcceptAny {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// QUIC client config with transport tuning + accept-any verification.
/// Shared by peer dialers and tests so the policy lives in one place.
pub fn quinn_client_config() -> quinn::ClientConfig {
    let rustls_cfg = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAny))
        .with_no_client_auth();
    let quic_cfg = quinn::crypto::rustls::QuicClientConfig::try_from(Arc::new(rustls_cfg))
        .expect("tls config builds");
    let mut client = quinn::ClientConfig::new(Arc::new(quic_cfg));
    client.transport_config(crate::endpoint::transport_config());
    client
}
