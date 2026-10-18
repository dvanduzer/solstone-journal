// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use super::*;
use hyper::body::Body as _;
use solstone_core_journal_io::journal_root::JournalRoot;
use std::pin::Pin;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

const HOST: &str = "checkpoint.example.test";
const DIR: &str = "https://ca.example.test/directory";
const ORDER: &str = "https://ca.example.test/order/1";

struct ResetFaults;
impl Drop for ResetFaults {
    fn drop(&mut self) {
        FAIL_ISSUANCE_WRITE.set(false);
        FAIL_SCHEDULE_WRITE.set(false);
    }
}

#[derive(Clone, Copy)]
enum Failure {
    None,
    RateLimit,
    OrderCheckpoint,
    HealthyAri,
    FirstOrderFails,
    Lost201,
}
struct Client {
    orders: AtomicUsize,
    payloads: Mutex<Vec<serde_json::Value>>,
    finalizes: AtomicUsize,
    ari_requests: AtomicUsize,
    directory: std::path::PathBuf,
    failure: Failure,
    response_time: i64,
}
struct TestClient(Arc<Client>);
impl HttpClient for TestClient {
    fn request(
        &self,
        request: http::Request<instant_acme::BodyWrapper<bytes::Bytes>>,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<instant_acme::BytesResponse, instant_acme::Error>,
                > + Send,
        >,
    > {
        let client = Arc::clone(&self.0);
        Box::pin(async move {
            let path = request.uri().path().to_owned();
            let mut body = request.into_body();
            let mut request_bytes = Vec::new();
            while let Some(frame) =
                futures::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)).await
            {
                if let Ok(data) = frame.unwrap().into_data() {
                    request_bytes.extend_from_slice(&data);
                }
            }
            let mut status = 200;
            let mut builder = http::Response::builder().header("replay-nonce", "nonce");
            let body = match path.as_str() {
                "/directory" => {
                    let mut directory = serde_json::json!({
                        "newNonce":"https://ca.example.test/nonce", "newAccount":"https://ca.example.test/account",
                        "newOrder":"https://ca.example.test/new-order", "revokeCert":"https://ca.example.test/revoke",
                        "keyChange":"https://ca.example.test/key-change"
                    });
                    if matches!(client.failure, Failure::HealthyAri | Failure::Lost201) {
                        directory["renewalInfo"] = "https://ca.example.test/renewal-info".into();
                    }
                    directory
                }
                _ if path.starts_with("/renewal-info/") => {
                    client.ari_requests.fetch_add(1, Ordering::SeqCst);
                    builder = builder.header("retry-after", "3600");
                    serde_json::json!({"suggestedWindow": {
                        "start":time::OffsetDateTime::from_unix_timestamp(client.response_time+4000).unwrap().format(&time::format_description::well_known::Rfc3339).unwrap(),
                        "end":time::OffsetDateTime::from_unix_timestamp(client.response_time+5000).unwrap().format(&time::format_description::well_known::Rfc3339).unwrap()
                    }})
                }
                "/nonce" => serde_json::Value::Null,
                "/new-order" => {
                    let jws: serde_json::Value = serde_json::from_slice(&request_bytes).unwrap();
                    let payload: serde_json::Value = serde_json::from_slice(
                        &base64::engine::general_purpose::URL_SAFE_NO_PAD
                            .decode(jws["payload"].as_str().unwrap())
                            .unwrap(),
                    )
                    .unwrap();
                    client.payloads.lock().unwrap().push(payload);
                    let attempt = client.orders.fetch_add(1, Ordering::SeqCst);
                    if matches!(client.failure, Failure::Lost201) && attempt == 0 {
                        // The controlled issuer accepted this order but its 201 was lost.
                        return Err(instant_acme::Error::Other(Box::new(std::io::Error::other(
                            "accepted newOrder response lost",
                        ))));
                    }
                    if matches!(client.failure, Failure::FirstOrderFails) && attempt == 0 {
                        return Err(instant_acme::Error::Other(Box::new(std::io::Error::other(
                            "synthetic first-order transport failure",
                        ))));
                    }
                    match client.failure {
                        Failure::RateLimit => {
                            crate::tls::set_test_now_override(Some(client.response_time));
                            FAIL_SCHEDULE_WRITE.set(true);
                            status = 429;
                            builder = builder.header("retry-after", "300");
                            serde_json::json!({"type":"urn:ietf:params:acme:error:rateLimited", "status":429, "detail":"synthetic"})
                        }
                        _ => {
                            if matches!(client.failure, Failure::OrderCheckpoint) {
                                FAIL_SCHEDULE_WRITE.set(true);
                            }
                            status = 201;
                            builder = builder.header("location", ORDER);
                            let mut order = ready_order();
                            if matches!(client.failure, Failure::HealthyAri) {
                                let row: RenewalScheduleRecord = serde_json::from_slice(
                                    &std::fs::read(client.directory.join("renewal.json")).unwrap(),
                                )
                                .unwrap();
                                let aki = base64::engine::general_purpose::STANDARD
                                    .decode(row.leaf_aki)
                                    .unwrap();
                                let serial = base64::engine::general_purpose::STANDARD
                                    .decode(row.leaf_serial)
                                    .unwrap();
                                order["replaces"] = CertificateIdentifier::new(
                                    Der::from_slice(&aki),
                                    Der::from_slice(&serial),
                                )
                                .to_string()
                                .into();
                            }
                            if matches!(client.failure, Failure::Lost201) {
                                // Ordinary reconciliation reuses the accepted Ready order,
                                // including its actual replaces field, as Boulder may do.
                                order["replaces"] =
                                    client.payloads.lock().unwrap()[0]["replaces"].clone();
                            }
                            order
                        }
                    }
                }
                "/order/1" => ready_order(),
                "/finalize" => {
                    client.finalizes.fetch_add(1, Ordering::SeqCst);
                    let row: RenewalScheduleRecord = serde_json::from_slice(
                        &std::fs::read(client.directory.join("renewal.json")).unwrap(),
                    )
                    .unwrap();
                    assert_eq!(row.order_url.as_deref(), Some(ORDER));
                    if matches!(client.failure, Failure::Lost201) {
                        assert_eq!(
                            row.order_replaces.as_deref(),
                            client.payloads.lock().unwrap()[0]["replaces"].as_str()
                        );
                    }
                    let key: IssuanceRecord = serde_json::from_slice(
                        &std::fs::read(client.directory.join("issuance.json")).unwrap(),
                    )
                    .unwrap();
                    assert_eq!(key.order_url, ORDER);
                    assert!(!key.csr_der().unwrap().is_empty());
                    return Err(instant_acme::Error::Other(Box::new(std::io::Error::other(
                        "stop after durable-checkpoint oracle",
                    ))));
                }
                _ => panic!("unexpected request: {}", path),
            };
            Ok(instant_acme::BytesResponse::from(
                builder
                    .status(status)
                    .body(instant_acme::BodyWrapper::from(
                        serde_json::to_vec(&body).unwrap(),
                    ))
                    .unwrap(),
            ))
        })
    }
}
fn ready_order() -> serde_json::Value {
    serde_json::json!({"status":"ready", "identifiers":[{"type":"dns","value":HOST}],
        "authorizations":[], "finalize":"https://ca.example.test/finalize"})
}
fn client(root: &std::path::Path, failure: Failure, response_time: i64) -> Arc<Client> {
    Arc::new(Client {
        orders: AtomicUsize::new(0),
        payloads: Mutex::new(Vec::new()),
        finalizes: AtomicUsize::new(0),
        ari_requests: AtomicUsize::new(0),
        directory: root.join("mcp-endpoint/tls"),
        failure,
        response_time,
    })
}
fn service(root: &std::path::Path) -> McpEndpointTlsService {
    McpEndpointTlsService::for_authorized_hostname(
        Arc::new(JournalRoot::open(root).unwrap()),
        HOST.into(),
        McpEndpointCertificateEnvironment::Staging,
        false,
    )
    .unwrap()
}
fn schedule() -> RenewalScheduleRecord {
    RenewalScheduleRecord {
        environment: McpEndpointCertificateEnvironment::Staging,
        hostname: HOST.into(),
        generation: None,
        leaf_aki: String::new(),
        leaf_serial: String::new(),
        renew_at: 0,
        window_start: None,
        window_end: None,
        ari_retry_at: None,
        ari_unavailable_since: None,
        issue_retry_at: None,
        issue_backoff_seconds: None,
        order_url: Some(ORDER.into()),
        order_replaces: None,
        replaces_submitted: None,
        omit_replaces: None,
    }
}

#[tokio::test(start_paused = true)]
async fn retained_key_must_be_saved_before_retry_finalizes() {
    let _clock = crate::tls::lock_test_clock(Some(1_700_000_000));
    let _faults = ResetFaults;
    let temp = tempfile::tempdir().unwrap();
    let service = service(temp.path());
    let root = JournalRoot::open(temp.path()).unwrap();
    let directory = unix::open_tls_state_directory(&root).unwrap();
    persist_schedule(&directory, &service, &schedule()).unwrap();
    let client = client(temp.path(), Failure::None, 0);
    let account_key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
    let account =
        instant_acme::Account::builder_with_http(Box::new(TestClient(Arc::clone(&client))))
            .from_parts(
                "https://ca.example.test/account/1".into(),
                rustls::pki_types::PrivatePkcs8KeyDer::from(account_key.serialize_der()),
                DIR.into(),
            )
            .await
            .unwrap();
    let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
    let params = rcgen::CertificateParams::new(vec![HOST.into()]).unwrap();
    let csr = params.serialize_request(&key).unwrap();
    let record = IssuanceRecord {
        environment: McpEndpointCertificateEnvironment::Staging,
        hostname: HOST.into(),
        generation: None,
        leaf_aki: String::new(),
        leaf_serial: String::new(),
        order_url: ORDER.into(),
        private_key: base64::engine::general_purpose::STANDARD.encode(key.serialize_der()),
        csr: base64::engine::general_purpose::STANDARD.encode(csr.der()),
    };
    FAIL_ISSUANCE_WRITE.set(true);
    let (_tx, mut rx) = watch::channel(false);
    let outcome = execute_order_completion(
        account.order(ORDER.into()).await.unwrap(),
        &directory,
        McpEndpointCertificateEnvironment::Staging,
        HOST,
        None,
        &[],
        &[],
        Some(record),
        &service,
        &mut rx,
    )
    .await;
    let OrderCompletionOutcome::CheckpointPending(record) = outcome else {
        panic!("unrecorded retained key reached finalization");
    };
    assert_eq!(client.finalizes.load(Ordering::SeqCst), 0);
    assert!(load_saved_issuance(&directory).unwrap().is_none());
    FAIL_ISSUANCE_WRITE.set(false);
    let outcome = execute_order_completion(
        account.order(ORDER.into()).await.unwrap(),
        &directory,
        McpEndpointCertificateEnvironment::Staging,
        HOST,
        None,
        &[],
        &[],
        Some(record),
        &service,
        &mut rx,
    )
    .await;
    assert!(matches!(outcome, OrderCompletionOutcome::Failed(_)));
    assert_eq!(client.finalizes.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn unsaved_rate_limit_survives_repair_and_uses_response_time() {
    let now = 1_700_000_000;
    let _clock = crate::tls::lock_test_clock(Some(now));
    let _faults = ResetFaults;
    let temp = tempfile::tempdir().unwrap();
    let service = service(temp.path());
    service.set_custom_directory_url(DIR.into());
    let root = JournalRoot::open(temp.path()).unwrap();
    let directory = unix::open_tls_state_directory(&root).unwrap();
    let client = client(temp.path(), Failure::RateLimit, now + 120);
    let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
    let (tx, mut rx) = watch::channel(false);
    let future = run_renewal_lifecycle(RenewalLoopParams {
        journal_root: None,
        directory: &directory,
        environment: McpEndpointCertificateEnvironment::Staging,
        hostname: HOST,
        generation: None,
        account_kid: "https://ca.example.test/account/1".into(),
        account_key_pkcs8: key.serialize_der(),
        tls_service: &service,
        shutdown: &mut rx,
        http_client: Arc::new(TestClient(Arc::clone(&client))),
        picker: None,
    });
    tokio::pin!(future);
    assert!(futures::poll!(&mut future).is_pending());
    assert_eq!(client.orders.load(Ordering::SeqCst), 1);
    FAIL_SCHEDULE_WRITE.set(false);
    crate::tls::set_test_now_override(Some(now + 151));
    tokio::time::advance(Duration::from_secs(31)).await;
    assert!(futures::poll!(&mut future).is_pending());
    assert_eq!(
        client.orders.load(Ordering::SeqCst),
        1,
        "storage repair bypassed retained Retry-After"
    );
    let saved = load_saved_schedule(
        &directory,
        McpEndpointCertificateEnvironment::Staging,
        HOST,
        None,
        None,
    )
    .unwrap()
    .unwrap();
    // Whether a failed wait checkpoint has been retried yet is timing dependent;
    // before repair the durable intent must never advertise an earlier retry.
    assert!(saved.issue_retry_at.is_none() || saved.issue_retry_at == Some(now + 420));
    crate::tls::set_test_now_override(Some(now + 419));
    tokio::time::advance(Duration::from_secs(268)).await;
    assert!(futures::poll!(&mut future).is_pending());
    assert_eq!(
        client.orders.load(Ordering::SeqCst),
        1,
        "delta Retry-After used loop-start time"
    );
    tx.send(true).unwrap();
    future.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn accepted_order_stays_single_and_cannot_finalize_before_url_checkpoint() {
    let now = 1_700_000_000;
    let _clock = crate::tls::lock_test_clock(Some(now));
    let _faults = ResetFaults;
    let temp = tempfile::tempdir().unwrap();
    let service = service(temp.path());
    service.set_custom_directory_url(DIR.into());
    let root = JournalRoot::open(temp.path()).unwrap();
    let directory = unix::open_tls_state_directory(&root).unwrap();
    let client = client(temp.path(), Failure::OrderCheckpoint, 0);
    let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
    let (tx, mut rx) = watch::channel(false);
    let future = run_renewal_lifecycle(RenewalLoopParams {
        journal_root: None,
        directory: &directory,
        environment: McpEndpointCertificateEnvironment::Staging,
        hostname: HOST,
        generation: None,
        account_kid: "https://ca.example.test/account/1".into(),
        account_key_pkcs8: key.serialize_der(),
        tls_service: &service,
        shutdown: &mut rx,
        http_client: Arc::new(TestClient(Arc::clone(&client))),
        picker: None,
    });
    tokio::pin!(future);
    assert!(futures::poll!(&mut future).is_pending());
    assert_eq!(client.orders.load(Ordering::SeqCst), 1);
    assert_eq!(client.finalizes.load(Ordering::SeqCst), 0);
    let saved = load_saved_schedule(
        &directory,
        McpEndpointCertificateEnvironment::Staging,
        HOST,
        None,
        None,
    )
    .unwrap()
    .unwrap();
    assert!(saved.order_url.is_none());
    FAIL_SCHEDULE_WRITE.set(false);
    crate::tls::set_test_now_override(Some(now + 30));
    tokio::time::advance(Duration::from_secs(30)).await;
    assert!(futures::poll!(&mut future).is_pending());
    assert_eq!(client.orders.load(Ordering::SeqCst), 1);
    assert_eq!(client.finalizes.load(Ordering::SeqCst), 1);
    tx.send(true).unwrap();
    future.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn healthy_later_window_refreshes_before_renewing_at_its_chosen_instant() {
    let now = 1_700_000_000;
    let _clock = crate::tls::lock_test_clock(Some(now));
    let _faults = ResetFaults;
    let temp = tempfile::tempdir().unwrap();
    let service = service(temp.path());
    service.set_custom_directory_url(DIR.into());
    let root = JournalRoot::open(temp.path()).unwrap();
    let directory = unix::open_tls_state_directory(&root).unwrap();
    let leaf_key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
    let mut leaf_params = rcgen::CertificateParams::new(vec![HOST.into()]).unwrap();
    leaf_params.use_authority_key_identifier_extension = true;
    leaf_params.not_before = time::OffsetDateTime::from_unix_timestamp(now - 3000).unwrap();
    leaf_params.not_after = time::OffsetDateTime::from_unix_timestamp(now + 6000).unwrap();
    let certificate = leaf_params.self_signed(&leaf_key).unwrap();
    service
        .install_ordinary_certificate(
            vec![certificate.der().to_vec()],
            leaf_key.serialize_der(),
            now - 3000,
            now + 6000,
        )
        .unwrap();
    let leaf = extract_leaf_cert_info(certificate.der()).unwrap();
    let mut row = schedule();
    row.leaf_aki = base64::engine::general_purpose::STANDARD.encode(&leaf.aki);
    row.leaf_serial = base64::engine::general_purpose::STANDARD.encode(&leaf.raw_serial);
    row.order_url = None;
    row.renew_at = now + 4200;
    row.window_start = Some(now + 4000);
    row.window_end = Some(now + 5000);
    row.ari_retry_at = Some(now + 3600);
    persist_schedule(&directory, &service, &row).unwrap();
    assert!(matches!(
        decide_renewal_action(
            now + 3600,
            Some(&leaf),
            Some(&row),
            McpEndpointCertificateEnvironment::Staging,
            HOST,
            None,
            true,
            &AriOutcome::None,
            false,
            &|start, _| start
        ),
        RenewalAction::QueryAri { .. }
    ));
    let client = client(temp.path(), Failure::HealthyAri, now);
    let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
    let (tx, mut rx) = watch::channel(false);
    let future = run_renewal_lifecycle(RenewalLoopParams {
        journal_root: None,
        directory: &directory,
        environment: McpEndpointCertificateEnvironment::Staging,
        hostname: HOST,
        generation: None,
        account_kid: "https://ca.example.test/account/1".into(),
        account_key_pkcs8: key.serialize_der(),
        tls_service: &service,
        shutdown: &mut rx,
        http_client: Arc::new(TestClient(Arc::clone(&client))),
        picker: None,
    });
    tokio::pin!(future);
    assert!(futures::poll!(&mut future).is_pending());
    crate::tls::set_test_now_override(Some(now + 3000));
    tokio::time::advance(Duration::from_secs(3000)).await;
    assert!(futures::poll!(&mut future).is_pending());
    assert_eq!(client.orders.load(Ordering::SeqCst), 0);
    crate::tls::set_test_now_override(Some(now + 3600));
    tokio::time::advance(Duration::from_secs(600)).await;
    assert!(futures::poll!(&mut future).is_pending());
    assert_eq!(client.ari_requests.load(Ordering::SeqCst), 1);
    assert_eq!(client.orders.load(Ordering::SeqCst), 0);
    crate::tls::set_test_now_override(Some(now + 4200));
    tokio::time::advance(Duration::from_secs(600)).await;
    assert!(futures::poll!(&mut future).is_pending());
    assert_eq!(client.orders.load(Ordering::SeqCst), 1);
    assert_eq!(client.finalizes.load(Ordering::SeqCst), 1);
    tx.send(true).unwrap();
    future.await.unwrap();
}

#[test]
fn forced_staging_restart_resumes_the_known_order() {
    let row = schedule();
    assert!(matches!(decide_renewal_action(100,None,Some(&row),
        McpEndpointCertificateEnvironment::Staging,HOST,None,false,&AriOutcome::None,true,&|start,_|start),
        RenewalAction::ResumeOrder{order_url,..} if order_url == ORDER));
}

#[tokio::test(start_paused = true)]
async fn forced_staging_resume_does_not_create_an_order_after_retry() {
    let now = 1_700_000_000;
    let _clock = crate::tls::lock_test_clock(Some(now));
    let _faults = ResetFaults;
    let temp = tempfile::tempdir().unwrap();
    let service = McpEndpointTlsService::for_authorized_hostname(
        Arc::new(JournalRoot::open(temp.path()).unwrap()),
        HOST.into(),
        McpEndpointCertificateEnvironment::Staging,
        true,
    )
    .unwrap();
    service.set_custom_directory_url(DIR.into());
    let root = JournalRoot::open(temp.path()).unwrap();
    let directory = unix::open_tls_state_directory(&root).unwrap();
    persist_schedule(&directory, &service, &schedule()).unwrap();
    let client = client(temp.path(), Failure::None, 0);
    let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
    let (tx, mut rx) = watch::channel(false);
    let future = run_renewal_lifecycle(RenewalLoopParams {
        journal_root: None,
        directory: &directory,
        environment: McpEndpointCertificateEnvironment::Staging,
        hostname: HOST,
        generation: None,
        account_kid: "https://ca.example.test/account/1".into(),
        account_key_pkcs8: key.serialize_der(),
        tls_service: &service,
        shutdown: &mut rx,
        http_client: Arc::new(TestClient(Arc::clone(&client))),
        picker: None,
    });
    tokio::pin!(future);
    assert!(futures::poll!(&mut future).is_pending());
    assert_eq!(client.orders.load(Ordering::SeqCst), 0);
    assert_eq!(client.finalizes.load(Ordering::SeqCst), 1);
    crate::tls::set_test_now_override(Some(now + 30));
    tokio::time::advance(Duration::from_secs(30)).await;
    assert!(futures::poll!(&mut future).is_pending());
    assert_eq!(client.orders.load(Ordering::SeqCst), 0);
    assert_eq!(client.finalizes.load(Ordering::SeqCst), 2);
    tx.send(true).unwrap();
    future.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn forced_cached_leaf_retries_a_failed_order_before_normal_backstop() {
    let now = 1_700_000_000;
    let _clock = crate::tls::lock_test_clock(Some(now));
    let _faults = ResetFaults;
    let temp = tempfile::tempdir().unwrap();
    let service = McpEndpointTlsService::for_authorized_hostname(
        Arc::new(JournalRoot::open(temp.path()).unwrap()),
        HOST.into(),
        McpEndpointCertificateEnvironment::Staging,
        true,
    )
    .unwrap();
    service.set_custom_directory_url(DIR.into());
    let root = JournalRoot::open(temp.path()).unwrap();
    let directory = unix::open_tls_state_directory(&root).unwrap();
    let leaf_key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
    let mut params = rcgen::CertificateParams::new(vec![HOST.into()]).unwrap();
    params.use_authority_key_identifier_extension = true;
    params.not_before = time::OffsetDateTime::from_unix_timestamp(now).unwrap();
    params.not_after = time::OffsetDateTime::from_unix_timestamp(now + 9000).unwrap();
    let certificate = params.self_signed(&leaf_key).unwrap();
    service
        .install_ordinary_certificate(
            vec![certificate.der().to_vec()],
            leaf_key.serialize_der(),
            now,
            now + 9000,
        )
        .unwrap();
    let client = client(temp.path(), Failure::FirstOrderFails, 0);
    let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
    let (tx, mut rx) = watch::channel(false);
    let future = run_renewal_lifecycle(RenewalLoopParams {
        journal_root: None,
        directory: &directory,
        environment: McpEndpointCertificateEnvironment::Staging,
        hostname: HOST,
        generation: None,
        account_kid: "https://ca.example.test/account/1".into(),
        account_key_pkcs8: key.serialize_der(),
        tls_service: &service,
        shutdown: &mut rx,
        http_client: Arc::new(TestClient(Arc::clone(&client))),
        picker: None,
    });
    tokio::pin!(future);
    assert!(futures::poll!(&mut future).is_pending());
    assert_eq!(client.orders.load(Ordering::SeqCst), 1);
    crate::tls::set_test_now_override(Some(now + 30));
    tokio::time::advance(Duration::from_secs(30)).await;
    assert!(futures::poll!(&mut future).is_pending());
    assert_eq!(
        client.orders.load(Ordering::SeqCst),
        2,
        "forced renewal was abandoned until the normal backstop"
    );
    assert_eq!(client.finalizes.load(Ordering::SeqCst), 1);
    tx.send(true).unwrap();
    future.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn lost_replacement_response_survives_restart_and_reconciles_once() {
    let now = 1_700_000_000;
    let _clock = crate::tls::lock_test_clock(Some(now));
    let _faults = ResetFaults;
    let temp = tempfile::tempdir().unwrap();
    let service1 = service(temp.path());
    service1.set_custom_directory_url(DIR.into());
    let root = JournalRoot::open(temp.path()).unwrap();
    let directory = unix::open_tls_state_directory(&root).unwrap();
    let leaf_key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
    let mut params = rcgen::CertificateParams::new(vec![HOST.into()]).unwrap();
    params.use_authority_key_identifier_extension = true;
    params.not_before = time::OffsetDateTime::from_unix_timestamp(now - 9000).unwrap();
    params.not_after = time::OffsetDateTime::from_unix_timestamp(now + 9000).unwrap();
    let certificate = params.self_signed(&leaf_key).unwrap();
    service1
        .install_ordinary_certificate(
            vec![certificate.der().to_vec()],
            leaf_key.serialize_der(),
            now - 9000,
            now + 9000,
        )
        .unwrap();
    let leaf = extract_leaf_cert_info(certificate.der()).unwrap();
    let mut row = schedule();
    row.order_url = None;
    row.renew_at = now - 8;
    row.leaf_aki = base64::engine::general_purpose::STANDARD.encode(&leaf.aki);
    row.leaf_serial = base64::engine::general_purpose::STANDARD.encode(&leaf.raw_serial);
    row.window_start = Some(now - 10);
    row.window_end = Some(now - 5);
    row.ari_retry_at = Some(now + 3600);
    persist_schedule(&directory, &service1, &row).unwrap();
    let client = client(temp.path(), Failure::Lost201, now);
    let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)
        .unwrap()
        .serialize_der();
    unix::persist_tls_acme_account_bytes(&directory, false, &key).unwrap();
    let (tx, mut rx) = watch::channel(false);
    let future = run_renewal_lifecycle(RenewalLoopParams {
        journal_root: None,
        directory: &directory,
        environment: McpEndpointCertificateEnvironment::Staging,
        hostname: HOST,
        generation: None,
        account_kid: "https://ca.example.test/account/1".into(),
        account_key_pkcs8: key.clone(),
        tls_service: &service1,
        shutdown: &mut rx,
        http_client: Arc::new(TestClient(Arc::clone(&client))),
        picker: None,
    });
    tokio::pin!(future);
    assert!(futures::poll!(&mut future).is_pending());
    assert_eq!(client.orders.load(Ordering::SeqCst), 1);
    let saved = load_saved_schedule(
        &directory,
        McpEndpointCertificateEnvironment::Staging,
        HOST,
        None,
        Some(&leaf),
    )
    .unwrap()
    .unwrap();
    assert_eq!(saved.replaces_submitted, Some(true));
    assert!(saved.order_url.is_none());
    assert_eq!(saved.issue_retry_at, Some(now + 30));
    tx.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(1), future)
        .await
        .unwrap()
        .unwrap();
    let service2 = service(temp.path());
    service2.set_custom_directory_url(DIR.into());
    crate::tls::set_test_now_override(Some(now + 29));
    tokio::time::advance(Duration::from_secs(29)).await;
    let (tx, mut rx) = watch::channel(false);
    let future = run_renewal_lifecycle(RenewalLoopParams {
        journal_root: None,
        directory: &directory,
        environment: McpEndpointCertificateEnvironment::Staging,
        hostname: HOST,
        generation: None,
        account_kid: "https://ca.example.test/account/1".into(),
        account_key_pkcs8: key.clone(),
        tls_service: &service2,
        shutdown: &mut rx,
        http_client: Arc::new(TestClient(Arc::clone(&client))),
        picker: None,
    });
    tokio::pin!(future);
    assert!(futures::poll!(&mut future).is_pending());
    assert_eq!(client.orders.load(Ordering::SeqCst), 1);
    crate::tls::set_test_now_override(Some(now + 30));
    tokio::time::advance(Duration::from_secs(1)).await;
    assert!(futures::poll!(&mut future).is_pending());
    assert_eq!(client.orders.load(Ordering::SeqCst), 2);
    assert_eq!(client.finalizes.load(Ordering::SeqCst), 1);
    {
        let payloads = client.payloads.lock().unwrap();
        assert_eq!(payloads.len(), 2);
        assert_eq!(
            payloads[0]["replaces"],
            cert_id_from_leaf(&leaf).to_string()
        );
        assert!(payloads[1].get("replaces").is_none());
    }

    assert_eq!(
        unix::read_tls_acme_account_bytes(&directory, false)
            .unwrap()
            .unwrap(),
        key
    );
    tx.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(1), future)
        .await
        .unwrap()
        .unwrap();
}
