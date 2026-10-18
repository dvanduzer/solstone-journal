// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use super::*;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tokio::sync::Notify;

const HOSTNAME: &str = "renewal.example.test";
const DIRECTORY: &str = "https://ca.example.test/directory";

#[derive(Default)]
struct RequestState {
    ari_started: Notify,
    ari_cancelled: AtomicBool,
    order_seen: Notify,
    orders: AtomicUsize,
}

struct HeldAriClient(Arc<RequestState>);

struct CancelAri(Arc<RequestState>);

impl Drop for CancelAri {
    fn drop(&mut self) {
        self.0.ari_cancelled.store(true, Ordering::SeqCst);
    }
}

impl HttpClient for HeldAriClient {
    fn request(
        &self,
        req: http::Request<instant_acme::BodyWrapper<bytes::Bytes>>,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<instant_acme::BytesResponse, instant_acme::Error>,
                > + Send,
        >,
    > {
        let state = Arc::clone(&self.0);
        crate::acme_http::with_request_deadline(async move {
            let path = req.uri().path();
            let body = if path == "/directory" {
                serde_json::json!({
                    "newNonce":"https://ca.example.test/nonce",
                    "newAccount":"https://ca.example.test/account",
                    "newOrder":"https://ca.example.test/new-order",
                    "revokeCert":"https://ca.example.test/revoke",
                    "keyChange":"https://ca.example.test/key-change",
                    "renewalInfo":"https://ca.example.test/renewal-info"
                })
                .to_string()
            } else if path == "/nonce" {
                String::new()
            } else if path.starts_with("/renewal-info/") {
                let _cancel = CancelAri(Arc::clone(&state));
                state.ari_started.notify_one();
                return std::future::pending().await;
            } else if path == "/new-order" {
                state.orders.fetch_add(1, Ordering::SeqCst);
                state.order_seen.notify_one();
                return Err(instant_acme::Error::Other(Box::new(std::io::Error::other(
                    "synthetic issuance failure",
                ))));
            } else {
                panic!("unexpected synthetic request: {}", req.uri());
            };
            Ok(instant_acme::BytesResponse::from(
                http::Response::builder()
                    .status(http::StatusCode::OK)
                    .header("replay-nonce", "synthetic-nonce")
                    .body(instant_acme::BodyWrapper::from(body.into_bytes()))
                    .unwrap(),
            ))
        })
    }
}

async fn stalled_transport_response(headers: bool) -> bool {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0_u8; 4096];
        let count = stream.read(&mut request).await.unwrap();
        assert!(count > 0);
        assert!(request[..count].starts_with(b"GET /renewal-info HTTP/1.1"));
        if !headers {
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\n")
                .await
                .unwrap();
        }
        let _ = stop_rx.await;
        drop(stream);
    });
    let client = crate::acme_http::McpEndpointHttpClient::new();
    let request = http::Request::builder()
        .uri(format!("http://{address}/renewal-info"))
        .body(instant_acme::BodyWrapper::default())
        .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(45), async {
        let mut response = client.request(request).await?;
        response
            .body
            .into_bytes()
            .await
            .map_err(instant_acme::Error::Other)
    })
    .await;
    let _ = stop_tx.send(());
    server.await.unwrap();
    matches!(result, Ok(Err(instant_acme::Error::Other(error)))
        if error.is::<tokio::time::error::Elapsed>())
}

#[tokio::test]
#[ignore = "explicit loopback request-deadline fixture"]
async fn real_transport_deadline_cancels_stalled_headers_and_body() {
    let (headers, body) = tokio::join!(
        stalled_transport_response(true),
        stalled_transport_response(false),
    );
    assert!(
        headers,
        "production transport did not bound stalled response headers"
    );
    assert!(
        body,
        "production transport did not bound a stalled response body"
    );
}

#[tokio::test(start_paused = true)]
#[ignore = "explicitly selected lifecycle timing fixture"]
async fn stalled_ari_reaches_due_fallback_and_keeps_serving_cached_leaf() {
    use rustls::pki_types::{CertificateDer, ServerName};
    use solstone_core_journal_io::journal_root::JournalRoot;

    let temp = tempfile::tempdir().unwrap();
    let root = Arc::new(JournalRoot::open(temp.path()).unwrap());
    let service = McpEndpointTlsService::for_authorized_hostname(
        Arc::clone(&root),
        HOSTNAME.into(),
        McpEndpointCertificateEnvironment::Staging,
        false,
    )
    .unwrap();
    service.set_custom_directory_url(DIRECTORY.into());
    let directory = unix::open_tls_state_directory(&root).unwrap();
    let now = current_epoch_seconds();
    let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
    let mut params = rcgen::CertificateParams::new(vec![HOSTNAME.into()]).unwrap();
    params.use_authority_key_identifier_extension = true;
    params.not_before = time::OffsetDateTime::from_unix_timestamp(now - 180).unwrap();
    params.not_after = time::OffsetDateTime::from_unix_timestamp(now + 60).unwrap();
    let certificate = params.self_signed(&key).unwrap();
    let der = certificate.der().to_vec();
    service
        .install_ordinary_certificate(vec![der.clone()], key.serialize_der(), now - 180, now + 60)
        .unwrap();
    let before = unix::read_tls_state_bytes(&directory).unwrap().unwrap();
    let state = Arc::new(RequestState::default());
    let client: Arc<dyn HttpClient> = Arc::new(HeldAriClient(Arc::clone(&state)));
    let account = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
    let lifecycle = run_renewal_lifecycle(RenewalLoopParams {
        journal_root: None,
        directory: &directory,
        environment: McpEndpointCertificateEnvironment::Staging,
        hostname: HOSTNAME,
        generation: None,
        account_kid: "https://ca.example.test/account/1".into(),
        account_key_pkcs8: account.serialize_der(),
        tls_service: &service,
        shutdown: &mut shutdown_rx,
        http_client: client,
        picker: None,
    });
    tokio::pin!(lifecycle);
    tokio::select! {
        _ = state.ari_started.notified() => {},
        result = &mut lifecycle => panic!("lifecycle stopped before ARI: {result:?}"),
        _ = tokio::time::sleep(Duration::from_secs(60)) => panic!("ARI request was not reached"),
    }
    assert_eq!(state.orders.load(Ordering::SeqCst), 0);
    tokio::time::advance(Duration::from_secs(29)).await;
    assert!(futures::poll!(&mut lifecycle).is_pending());
    assert_eq!(state.orders.load(Ordering::SeqCst), 0);
    tokio::time::advance(Duration::from_secs(1)).await;
    tokio::select! {
        _ = state.order_seen.notified() => {},
        result = &mut lifecycle => panic!("lifecycle stopped before fallback: {result:?}"),
        _ = tokio::time::sleep(Duration::from_secs(1)) => panic!("stalled ARI did not reach due fallback"),
    }
    assert!(state.ari_cancelled.load(Ordering::SeqCst));
    assert_eq!(state.orders.load(Ordering::SeqCst), 1);
    shutdown_tx.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(1), &mut lifecycle)
        .await
        .unwrap()
        .unwrap();
    assert!(before == unix::read_tls_state_bytes(&directory).unwrap().unwrap());

    let mut roots = rustls::RootCertStore::empty();
    roots.add(CertificateDer::from(der.clone())).unwrap();
    let mut config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    let mut client =
        rustls::ClientConnection::new(Arc::new(config), ServerName::try_from(HOSTNAME).unwrap())
            .unwrap();
    let mut server =
        rustls::ServerConnection::new(crate::tls::mcp_endpoint_server_config(&service)).unwrap();
    for _ in 0..10 {
        let mut bytes = Vec::new();
        client.write_tls(&mut bytes).unwrap();
        server.read_tls(&mut std::io::Cursor::new(bytes)).unwrap();
        server.process_new_packets().unwrap();
        let mut bytes = Vec::new();
        server.write_tls(&mut bytes).unwrap();
        client.read_tls(&mut std::io::Cursor::new(bytes)).unwrap();
        client.process_new_packets().unwrap();
        if !client.is_handshaking() && !server.is_handshaking() {
            break;
        }
    }
    assert!(!client.is_handshaking() && !server.is_handshaking());
    assert_eq!(
        client.peer_certificates().unwrap()[0].as_ref(),
        der.as_slice()
    );
}
