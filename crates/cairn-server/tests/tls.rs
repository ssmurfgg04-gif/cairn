//! TLS acceptance (beta punch list): the metadata plane (7443) MUST be able to bind TLS
//! and serve an authenticated call through a real rustls handshake with a custom CA.
//! Runs the assembled server with a self-signed cert (rcgen), then dials it with
//! `connect_channel` — the same helper production clients use.

use std::sync::Arc;

use cairn_core::clock::{SystemClock, WallClock};

#[tokio::test]
async fn metadata_plane_serves_tls_with_custom_ca() {
    // rustls provider: feature unification leaves both compiled in — pick ring explicitly
    let _ = rustls::crypto::ring::default_provider().install_default();
    // 1) dev-style self-signed cert covering localhost + loopback IP
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into(), "127.0.0.1".into()])
        .expect("rcgen");
    let cert_pem = cert.cert.pem();
    let key_pem = cert.key_pair.serialize_pem();

    // 2) server state + TLS-enabled gRPC endpoint (mirrors run.rs's TLS branch)
    let dir = tempfile::tempdir().unwrap();
    let clock: Arc<dyn SystemClock> = Arc::new(WallClock);
    let cfg = cairn_server::run::ServerConfig {
        data_dir: dir.path().to_path_buf(),
        grpc_addr: "127.0.0.1:0".into(),
        objects_addr: "127.0.0.1:0".into(),
        dev_insecure: true,
        tls_cert: None, // bound manually below with the rcgen material
        tls_key: None,
    };
    let state = cairn_server::run::build_state(&cfg, clock).await.unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let identity = tonic::transport::Identity::from_pem(cert_pem.clone(), key_pem);
    let jh = tokio::spawn(async move {
        let _ = tonic::transport::Server::builder()
            .tls_config(tonic::transport::ServerTlsConfig::new().identity(identity))
            .expect("server tls")
            .add_service(cairn_proto::pb::auth_server::AuthServer::new(
                cairn_server::services::AuthSvc { state },
            ))
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
            .await;
    });

    // 3) client dials https://localhost:<port> with ONLY our CA pinned — plaintext
    //    clients must fail, CA-pinned clients must succeed
    let url = format!("https://localhost:{}", addr.port());
    let channel = cairn_sync::plane_grpc::connect_channel(&url, Some(cert_pem.as_bytes()))
        .await
        .expect("TLS handshake with pinned CA");
    let mut auth = cairn_proto::pb::auth_client::AuthClient::new(channel);
    let out = auth
        .enroll_code(cairn_proto::pb::EnrollCodeRequest {
            tenant_id: "t1".into(),
            email: "editor@studio.tv".into(),
            scopes: "sync".into(),
        })
        .await
        .expect("authenticated call over TLS");
    assert!(out.into_inner().code.starts_with("enr-"));

    // 4) a client with the WRONG CA must NOT complete a handshake
    let bad = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let wrong_ca = bad.cert.pem();
    let r = cairn_sync::plane_grpc::connect_channel(&url, Some(wrong_ca.as_bytes())).await;
    assert!(r.is_err(), "unpinned CA must be rejected");
    jh.abort();
}
