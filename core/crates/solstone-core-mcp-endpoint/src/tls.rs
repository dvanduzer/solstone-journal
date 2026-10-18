// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Owner-held, hot-swappable TLS state for the MCP listener.
//!
//! Lane B owns the listener. This module owns the exact TLS 1.3 resolver it
//! receives, the ordinary certificate state below the journal root, and the
//! short-lived in-memory TLS-ALPN challenge certificate. No caller can supply
//! a certificate path or recover a private key from the opaque service.

use std::fmt;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration as StdDuration, Instant};

use arc_swap::ArcSwapOption;
use base64::Engine as _;
use rcgen::{
    CertificateParams, CustomExtension, DistinguishedName, KeyPair, KeyUsagePurpose,
    PKCS_ECDSA_P256_SHA256,
};
use ring::rand::SystemRandom;
use ring::signature::{
    ECDSA_P256_SHA256_ASN1_SIGNING, ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair as _,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use solstone_core_journal_config::McpEndpointCertificateEnvironment;
use solstone_core_journal_io::journal_root::JournalRoot;
use time::{Duration, OffsetDateTime};
use x509_parser::prelude::{GeneralName, parse_x509_certificate};

use crate::unix::{self, TlsStateDirectory};

const ACME_TLS_ALPN: &[u8] = b"acme-tls/1";
const HTTP11_ALPN: &[u8] = b"http/1.1";
const ACME_IDENTIFIER_OID: &[u64] = &[1, 3, 6, 1, 5, 5, 7, 1, 31];

/// Opaque, owner-held certificate state for one account-authorized MCP name.
///
/// The construction, persistent store, certificate installation, and challenge
/// routes are crate-private. Consumers can only hand this handle to
/// [`mcp_endpoint_server_config`].
pub struct McpEndpointTlsService {
    resolver: Arc<McpEndpointCertificateResolver>,
    store: Option<Arc<CertificateStateStore>>,
    force_staging_renewal: bool,
    #[cfg(test)]
    pub(crate) last_publication_error: Arc<std::sync::Mutex<Option<McpEndpointTlsError>>>,
    #[cfg(test)]
    test_install_shutdown: Arc<std::sync::Mutex<Option<tokio::sync::watch::Sender<bool>>>>,
    #[cfg(test)]
    pub(crate) custom_http_client: Arc<std::sync::Mutex<Option<Arc<dyn instant_acme::HttpClient>>>>,
    #[cfg(test)]
    #[allow(clippy::type_complexity)]
    pub(crate) custom_picker:
        Arc<std::sync::Mutex<Option<Arc<dyn Fn(i64, i64) -> i64 + Send + Sync>>>>,
    #[cfg(any(test, feature = "test-hooks"))]
    pub(crate) custom_directory_url: Arc<std::sync::Mutex<Option<String>>>,
    #[cfg(any(test, feature = "test-hooks"))]
    pub(crate) extra_trust_roots:
        Arc<std::sync::Mutex<Option<Vec<rustls::pki_types::CertificateDer<'static>>>>>,
    #[cfg(test)]
    pub(crate) custom_order_key: Arc<std::sync::Mutex<Option<Vec<u8>>>>,
}

pub(crate) struct CertificateStateStore {
    pub(crate) directory: TlsStateDirectory,
    pub(crate) environment: McpEndpointCertificateEnvironment,
    pub(crate) generation: Option<u64>,
}

struct McpEndpointCertificateResolver {
    hostname: String,
    ordinary: ArcSwapOption<ActiveOrdinaryCertificate>,
    challenge: ArcSwapOption<ActiveChallengeCertificate>,
    next_challenge_generation: AtomicU64,
}

struct ActiveOrdinaryCertificate {
    key: Arc<CertifiedKey>,
    expires_at: Instant,
    not_after: i64,
}

#[cfg(test)]
pub static TEST_NOW_OVERRIDE: std::sync::RwLock<Option<time::OffsetDateTime>> =
    std::sync::RwLock::new(None);

#[cfg(test)]
pub(crate) static TEST_CLOCK_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
pub(crate) struct TestClockGuard {
    _guard: std::sync::MutexGuard<'static, ()>,
}

#[cfg(test)]
impl Drop for TestClockGuard {
    fn drop(&mut self) {
        set_test_now_override(None);
    }
}

#[cfg(test)]
pub(crate) fn lock_test_clock(timestamp: Option<i64>) -> TestClockGuard {
    let guard = TEST_CLOCK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    set_test_now_override(timestamp);
    TestClockGuard { _guard: guard }
}

#[cfg(test)]
pub(crate) fn set_test_now_override(timestamp: Option<i64>) {
    if let Ok(mut guard) = TEST_NOW_OVERRIDE.write() {
        *guard = timestamp.and_then(|ts| time::OffsetDateTime::from_unix_timestamp(ts).ok());
    }
}

pub(crate) fn current_epoch_seconds() -> i64 {
    #[cfg(test)]
    {
        if let Ok(guard) = TEST_NOW_OVERRIDE.read()
            && let Some(now) = *guard
        {
            return now.unix_timestamp();
        }
    }
    time::OffsetDateTime::now_utc().unix_timestamp()
}

struct ActiveChallengeCertificate {
    key: Arc<CertifiedKey>,
    generation: u64,
}

/// A scoped handle for an active TLS-ALPN-01 challenge certificate.
///
/// It clears only its own generation. Dropping an old guard therefore cannot
/// withdraw a challenge installed by a later issuance attempt.
pub(crate) struct McpEndpointChallengeGuard {
    resolver: Arc<McpEndpointCertificateResolver>,
    generation: u64,
}

/// Payload-free result from the unattended ACME certificate lifecycle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum McpEndpointCertificateLifecycleError {
    /// Owner-held TLS state could not be opened or validated.
    State,
}

impl fmt::Display for McpEndpointCertificateLifecycleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("MCP endpoint certificate lifecycle could not start")
    }
}

impl std::error::Error for McpEndpointCertificateLifecycleError {}

/// Payload-free failure from local TLS state validation or activation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum McpEndpointTlsError {
    State,
}

impl fmt::Debug for McpEndpointCertificateResolver {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("McpEndpointCertificateResolver")
            .finish_non_exhaustive()
    }
}

impl McpEndpointTlsService {
    pub(crate) fn force_staging_renewal(&self) -> bool {
        self.force_staging_renewal
    }

    pub(crate) fn effective_http_client(&self) -> Arc<dyn instant_acme::HttpClient> {
        #[cfg(test)]
        if let Ok(guard) = self.custom_http_client.lock()
            && let Some(client) = guard.as_ref()
        {
            return Arc::clone(client);
        }
        #[cfg(any(test, feature = "test-hooks"))]
        if let Ok(guard) = self.extra_trust_roots.lock()
            && let Some(roots) = guard.as_ref()
        {
            return Arc::new(crate::acme_http::McpEndpointHttpClient::with_extra_roots(
                roots.clone(),
            ));
        }
        Arc::new(crate::acme_http::McpEndpointHttpClient::default())
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn set_custom_http_client(&self, client: Arc<dyn instant_acme::HttpClient>) {
        if let Ok(mut guard) = self.custom_http_client.lock() {
            *guard = Some(client);
        }
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn set_custom_picker(&self, picker: Arc<dyn Fn(i64, i64) -> i64 + Send + Sync>) {
        if let Ok(mut guard) = self.custom_picker.lock() {
            *guard = Some(picker);
        }
    }

    #[cfg(any(test, feature = "test-hooks"))]
    #[allow(dead_code)]
    pub(crate) fn set_custom_directory_url(&self, url: String) {
        if let Ok(mut guard) = self.custom_directory_url.lock() {
            *guard = Some(url);
        }
    }

    #[cfg(any(test, feature = "test-hooks"))]
    #[allow(dead_code)]
    pub(crate) fn set_extra_trust_roots(
        &self,
        roots: Vec<rustls::pki_types::CertificateDer<'static>>,
    ) {
        if let Ok(mut guard) = self.extra_trust_roots.lock() {
            *guard = Some(roots);
        }
    }

    #[cfg(any(test, feature = "test-hooks"))]
    #[allow(dead_code)]
    pub(crate) fn effective_directory_url(&self) -> Option<String> {
        self.custom_directory_url
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn effective_picker(&self) -> Option<Arc<dyn Fn(i64, i64) -> i64 + Send + Sync>> {
        self.custom_picker
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
    }

    pub(crate) fn ordinary_certificate_is_active(&self) -> bool {
        let now_ts = current_epoch_seconds();
        self.resolver
            .ordinary
            .load_full()
            .as_deref()
            .is_some_and(|ordinary| {
                now_ts < ordinary.not_after && Instant::now() < ordinary.expires_at
            })
    }

    pub(crate) fn active_ordinary_leaf_der(&self) -> Option<Vec<u8>> {
        let now_ts = current_epoch_seconds();
        let ordinary = self.resolver.ordinary.load_full()?;
        if now_ts < ordinary.not_after && Instant::now() < ordinary.expires_at {
            ordinary.key.cert.first().map(|c| c.as_ref().to_vec())
        } else {
            None
        }
    }

    pub(crate) fn authorized_hostname(&self) -> &str {
        &self.resolver.hostname
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn set_custom_order_key(&self, key: Vec<u8>) {
        if let Ok(mut guard) = self.custom_order_key.lock() {
            *guard = Some(key);
        }
    }

    #[allow(dead_code)]
    pub(crate) fn lifecycle_copy(&self) -> Self {
        Self {
            resolver: Arc::clone(&self.resolver),
            store: self.store.as_ref().map(Arc::clone),
            force_staging_renewal: self.force_staging_renewal,
            #[cfg(test)]
            last_publication_error: Arc::clone(&self.last_publication_error),
            #[cfg(test)]
            test_install_shutdown: Arc::clone(&self.test_install_shutdown),
            #[cfg(test)]
            custom_http_client: Arc::clone(&self.custom_http_client),
            #[cfg(test)]
            custom_picker: Arc::clone(&self.custom_picker),
            #[cfg(any(test, feature = "test-hooks"))]
            custom_directory_url: Arc::clone(&self.custom_directory_url),
            #[cfg(any(test, feature = "test-hooks"))]
            extra_trust_roots: Arc::clone(&self.extra_trust_roots),
            #[cfg(test)]
            custom_order_key: Arc::clone(&self.custom_order_key),
        }
    }

    /// Construct TLS state for an account-authorized hostname and activate a
    /// valid persisted ordinary certificate, if one exists. Invalid state is
    /// never repaired or replaced during load.
    pub(crate) fn for_authorized_hostname(
        journal_root: Arc<JournalRoot>,
        hostname: String,
        environment: McpEndpointCertificateEnvironment,
        force_staging_renewal: bool,
    ) -> Result<Self, McpEndpointTlsError> {
        if !is_exact_authorized_hostname(&hostname, &hostname)
            || (force_staging_renewal
                && matches!(environment, McpEndpointCertificateEnvironment::Production))
        {
            return Err(McpEndpointTlsError::State);
        }
        let store = Arc::new(CertificateStateStore {
            directory: unix::open_tls_state_directory(&journal_root)
                .map_err(|_| McpEndpointTlsError::State)?,
            environment,
            generation: None,
        });
        let service = Self::empty(hostname, Some(Arc::clone(&store)), force_staging_renewal);
        if let Some(bytes) =
            unix::read_tls_state_bytes(&store.directory).map_err(|_| McpEndpointTlsError::State)?
        {
            match classify_stored_state(&bytes, &service.resolver.hostname, environment)? {
                StoredCertificateClassification::Same(decoded) => {
                    if certificate_is_current(&decoded)? {
                        let active = activate_decoded_stored_state(decoded)?;
                        service.resolver.ordinary.store(Some(Arc::new(active)));
                    }
                }
                StoredCertificateClassification::Foreign => {}
            }
        }
        Ok(service)
    }

    pub(crate) fn for_byo_cert_directory(
        cert_directory: unix::TlsStateDirectory,
        hostname: String,
        generation: u64,
    ) -> Result<Self, McpEndpointTlsError> {
        Self::for_byo_cert_directory_in_environment(
            cert_directory,
            hostname,
            generation,
            McpEndpointCertificateEnvironment::Production,
        )
    }

    fn for_byo_cert_directory_in_environment(
        cert_directory: unix::TlsStateDirectory,
        hostname: String,
        generation: u64,
        environment: McpEndpointCertificateEnvironment,
    ) -> Result<Self, McpEndpointTlsError> {
        if !is_exact_authorized_hostname(&hostname, &hostname) {
            return Err(McpEndpointTlsError::State);
        }
        let store = Arc::new(CertificateStateStore {
            directory: cert_directory,
            environment,
            generation: Some(generation),
        });
        let service = Self::empty(hostname, Some(Arc::clone(&store)), false);
        if let Some(bytes) =
            unix::read_tls_state_bytes(&store.directory).map_err(|_| McpEndpointTlsError::State)?
        {
            match classify_stored_state(&bytes, &service.resolver.hostname, environment)? {
                StoredCertificateClassification::Same(decoded) => {
                    if certificate_is_current(&decoded)? {
                        let active = activate_decoded_stored_state(decoded)?;
                        service.resolver.ordinary.store(Some(Arc::new(active)));
                    }
                }
                StoredCertificateClassification::Foreign => {}
            }
        }
        Ok(service)
    }

    /// Whether a newly authenticated bridge authority names this service's
    /// exact certificate hostname. This stays crate-private so callers cannot
    /// inspect or select the hostname themselves.
    pub(crate) fn matches_authorized_hostname(&self, hostname: &str) -> bool {
        is_exact_authorized_hostname(hostname, &self.resolver.hostname)
    }

    fn empty(
        hostname: String,
        store: Option<Arc<CertificateStateStore>>,
        force_staging_renewal: bool,
    ) -> Self {
        Self {
            resolver: Arc::new(McpEndpointCertificateResolver {
                hostname,
                ordinary: ArcSwapOption::empty(),
                challenge: ArcSwapOption::empty(),
                next_challenge_generation: AtomicU64::new(0),
            }),
            store,
            force_staging_renewal,
            #[cfg(test)]
            last_publication_error: Arc::new(std::sync::Mutex::new(None)),
            #[cfg(test)]
            test_install_shutdown: Arc::new(std::sync::Mutex::new(None)),
            #[cfg(test)]
            custom_http_client: Arc::new(std::sync::Mutex::new(None)),
            #[cfg(test)]
            custom_picker: Arc::new(std::sync::Mutex::new(None)),
            #[cfg(any(test, feature = "test-hooks"))]
            custom_directory_url: Arc::new(std::sync::Mutex::new(None)),
            #[cfg(any(test, feature = "test-hooks"))]
            extra_trust_roots: Arc::new(std::sync::Mutex::new(None)),
            #[cfg(test)]
            custom_order_key: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    /// Validate, persist, reopen, revalidate, then activate a newly issued
    /// ordinary certificate. A persistence or reopen failure leaves the prior
    /// still-valid key active.
    pub(crate) fn install_ordinary_certificate(
        &self,
        certificate_chain: Vec<Vec<u8>>,
        private_key: Vec<u8>,
        not_before: i64,
        not_after: i64,
    ) -> Result<(), McpEndpointTlsError> {
        let store = self.store.as_ref().ok_or(McpEndpointTlsError::State)?;
        let state = StoredCertificateState {
            environment: environment_name(store.environment).to_owned(),
            hostname: self.resolver.hostname.clone(),
            certificate_chain: certificate_chain
                .into_iter()
                .map(|value| base64::engine::general_purpose::STANDARD.encode(value))
                .collect(),
            private_key: base64::engine::general_purpose::STANDARD.encode(private_key),
            not_before,
            not_after,
        };
        let serialized = serde_json::to_vec(&state).map_err(|_| McpEndpointTlsError::State)?;
        let _ = validate_stored_state(&serialized, &self.resolver.hostname, store.environment)?;
        unix::persist_tls_state_bytes(&store.directory, &serialized)
            .map_err(|_| McpEndpointTlsError::State)?;
        let reopened = unix::read_tls_state_bytes(&store.directory)
            .map_err(|_| McpEndpointTlsError::State)?
            .ok_or(McpEndpointTlsError::State)?;
        let active = validate_stored_state(&reopened, &self.resolver.hostname, store.environment)?;
        self.resolver.ordinary.store(Some(Arc::new(active)));
        #[cfg(test)]
        if let Some(shutdown) = self
            .test_install_shutdown
            .lock()
            .expect("test install hook")
            .as_ref()
        {
            shutdown.send_replace(true);
        }
        Ok(())
    }

    /// Install a short-lived RFC 8737 certificate for one ACME key-authorization
    /// digest. The certificate and private key exist only in this process.
    pub(crate) fn install_acme_challenge(
        &self,
        key_authorization_digest: [u8; 32],
    ) -> Result<McpEndpointChallengeGuard, McpEndpointTlsError> {
        let key = build_acme_challenge_key(&self.resolver.hostname, &key_authorization_digest)?;
        let generation = self
            .resolver
            .next_challenge_generation
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        let generation = if generation == 0 { 1 } else { generation };
        self.resolver
            .challenge
            .store(Some(Arc::new(ActiveChallengeCertificate {
                key,
                generation,
            })));
        Ok(McpEndpointChallengeGuard {
            resolver: Arc::clone(&self.resolver),
            generation,
        })
    }

    /// Keep the journal-owned ACME account and certificate current until the
    /// supplied listener shutdown. The caller must already be serving this
    /// service's [`mcp_endpoint_server_config`]: TLS-ALPN-01 validation is
    /// deliberately answered by that same listener, not by a second port.
    ///
    /// A cached but expired certificate is structurally validated and passed
    /// to the ACME scheduler without being activated for ordinary handshakes.
    /// Consequently a journal returning after an offline renewal window
    /// immediately retries issuance while never serving an expired key.
    pub async fn run_acme_renewal(
        &self,
        shutdown: &mut tokio::sync::watch::Receiver<bool>,
    ) -> Result<(), McpEndpointCertificateLifecycleError> {
        self.run_acme_renewal_inner(None, shutdown).await
    }

    pub(crate) async fn run_acme_renewal_with_owner_state(
        &self,
        journal_root: &Path,
        shutdown: &mut tokio::sync::watch::Receiver<bool>,
    ) -> Result<(), McpEndpointCertificateLifecycleError> {
        self.run_acme_renewal_inner(Some(journal_root), shutdown)
            .await
    }

    async fn run_acme_renewal_inner(
        &self,
        journal_root: Option<&Path>,
        shutdown: &mut tokio::sync::watch::Receiver<bool>,
    ) -> Result<(), McpEndpointCertificateLifecycleError> {
        if *shutdown.borrow() || shutdown.has_changed().is_err() {
            return Ok(());
        }
        let store = self
            .store
            .as_ref()
            .ok_or(McpEndpointCertificateLifecycleError::State)?;
        let production = matches!(
            store.environment,
            McpEndpointCertificateEnvironment::Production
        );

        let client = self.effective_http_client();
        let picker = {
            #[cfg(test)]
            {
                self.effective_picker()
            }
            #[cfg(not(test))]
            {
                None
            }
        };

        let reg_default_dir = if production {
            instant_acme::LetsEncrypt::Production.url()
        } else {
            instant_acme::LetsEncrypt::Staging.url()
        };
        #[cfg(any(test, feature = "test-hooks"))]
        let reg_dir = self
            .effective_directory_url()
            .unwrap_or_else(|| reg_default_dir.to_string());
        #[cfg(not(any(test, feature = "test-hooks")))]
        let reg_dir = reg_default_dir.to_string();

        let account_kid = loop {
            let reg_dir = reg_dir.clone();
            let register_client = Arc::clone(&client);
            let establish_fut =
                crate::sme_account::establish_account_uri(&store.directory, production, |key| {
                    let register_client = Arc::clone(&register_client);
                    let reg_dir = reg_dir.clone();
                    async move {
                        let pkcs8_der = rustls::pki_types::PrivatePkcs8KeyDer::from(key);
                        let instant_key = instant_acme::Key::from_pkcs8_der(pkcs8_der.clone_key())
                            .map_err(|e| e.to_string())?;
                        let pki_key = rustls::pki_types::PrivateKeyDer::Pkcs8(pkcs8_der);
                        let (account, _creds) = instant_acme::Account::builder_with_http(Box::new(
                            crate::acme_http::ArcHttpClient(register_client),
                        ))
                        .create_from_key((instant_key, pki_key), reg_dir)
                        .await
                        .map_err(|e| e.to_string())?;
                        Ok(account.id().to_owned())
                    }
                });

            let establish_res = tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        return Ok(());
                    }
                    continue;
                }
                res = establish_fut => res,
            };

            match establish_res {
                Ok(uri) => break uri,
                Err(_e) => {
                    log::error!("failed to establish ACME account URI");
                    tokio::select! {
                        changed = shutdown.changed() => {
                            if changed.is_err() || *shutdown.borrow() {
                                return Ok(());
                            }
                        }
                        _ = tokio::time::sleep(std::time::Duration::from_secs(
                            crate::acme_lifecycle::INITIAL_ISSUE_BACKOFF_SECONDS,
                        )) => {}
                    }
                }
            }
        };

        let account_bytes = unix::read_tls_acme_account_bytes(&store.directory, production)
            .map_err(|_| McpEndpointCertificateLifecycleError::State)?
            .ok_or(McpEndpointCertificateLifecycleError::State)?;

        let params = crate::acme_lifecycle::RenewalLoopParams {
            journal_root,
            directory: &store.directory,
            environment: store.environment,
            hostname: &self.resolver.hostname,
            generation: store.generation,
            account_kid,
            account_key_pkcs8: account_bytes,
            tls_service: self,
            shutdown,
            http_client: client,
            picker,
        };

        crate::acme_lifecycle::run_renewal_lifecycle(params).await
    }

    pub(crate) async fn run_byo_acme_renewal(
        &self,
        account_dir: &unix::TlsStateDirectory,
        shutdown: &mut tokio::sync::watch::Receiver<bool>,
    ) -> Result<(), McpEndpointCertificateLifecycleError> {
        if *shutdown.borrow() || shutdown.has_changed().is_err() {
            return Ok(());
        }
        let store = self
            .store
            .as_ref()
            .ok_or(McpEndpointCertificateLifecycleError::State)?;

        let uri = unix::read_byo_account_uri(account_dir)
            .map_err(|_| McpEndpointCertificateLifecycleError::State)?
            .ok_or(McpEndpointCertificateLifecycleError::State)?;

        let account_bytes = unix::read_byo_account_key(account_dir)
            .map_err(|_| McpEndpointCertificateLifecycleError::State)?
            .ok_or(McpEndpointCertificateLifecycleError::State)?;

        validate_acme_account_key(&account_bytes)
            .map_err(|_| McpEndpointCertificateLifecycleError::State)?;

        let client = self.effective_http_client();
        let picker = {
            #[cfg(test)]
            {
                self.effective_picker()
            }
            #[cfg(not(test))]
            {
                None
            }
        };

        let params = crate::acme_lifecycle::RenewalLoopParams {
            journal_root: None,
            directory: &store.directory,
            environment: store.environment,
            hostname: &self.resolver.hostname,
            generation: store.generation,
            account_kid: uri,
            account_key_pkcs8: account_bytes,
            tls_service: self,
            shutdown,
            http_client: client,
            picker,
        };

        crate::acme_lifecycle::run_renewal_lifecycle(params).await
    }

    #[cfg(all(test, not(feature = "full-tests")))]
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) fn empty_for_test(hostname: String) -> Self {
        Self::empty(hostname, None, false)
    }

    #[cfg(all(test, not(feature = "full-tests")))]
    #[cfg_attr(not(unix), allow(dead_code))]
    fn install_ordinary_for_test(&self, key: Arc<CertifiedKey>) {
        self.resolver
            .ordinary
            .store(Some(Arc::new(ActiveOrdinaryCertificate {
                key,
                expires_at: Instant::now() + StdDuration::from_secs(3600),
                not_after: time::OffsetDateTime::now_utc().unix_timestamp() + 3600,
            })));
    }
}

#[cfg(all(test, feature = "full-tests"))]
#[cfg(unix)]
#[path = "acme_process_tests.rs"]
mod acme_test_driver;

impl Drop for McpEndpointChallengeGuard {
    fn drop(&mut self) {
        let current = self.resolver.challenge.load_full();
        if current
            .as_ref()
            .is_some_and(|challenge| challenge.generation == self.generation)
        {
            let _ = self.resolver.challenge.compare_and_swap(&current, None);
        }
    }
}

/// Build the one TLS configuration that Lane B's dedicated listener consumes.
///
/// The resolver stays shared with the service: a persisted ordinary-certificate
/// install or temporary ACME challenge affects only new handshakes, never an
/// established TLS session or the listener configuration identity.
pub fn mcp_endpoint_server_config(service: &McpEndpointTlsService) -> Arc<rustls::ServerConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let resolver: Arc<dyn ResolvesServerCert> = service.resolver.clone();
    let mut config = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .expect("ring provider supports TLS 1.3")
        .with_no_client_auth()
        .with_cert_resolver(resolver);
    config.alpn_protocols = vec![ACME_TLS_ALPN.to_vec(), HTTP11_ALPN.to_vec()];
    Arc::new(config)
}

impl ResolvesServerCert for McpEndpointCertificateResolver {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let server_name = hello.server_name()?;
        if !is_exact_authorized_hostname(server_name, &self.hostname) {
            return None;
        }
        let alpn = hello
            .alpn()
            .map(|protocols| protocols.collect::<Vec<_>>())
            .unwrap_or_default();
        if is_acme_attempt(&alpn) {
            if !is_only_acme_alpn(&alpn) {
                return None;
            }
            return self
                .challenge
                .load_full()
                .as_deref()
                .filter(|challenge| challenge.generation != 0)
                .map(|challenge| Arc::clone(&challenge.key));
        }
        let now_ts = current_epoch_seconds();
        self.ordinary
            .load_full()
            .as_deref()
            .filter(|ordinary| now_ts < ordinary.not_after && Instant::now() < ordinary.expires_at)
            .map(|ordinary| Arc::clone(&ordinary.key))
    }
}

#[derive(Serialize)]
struct StoredCertificateState {
    environment: String,
    hostname: String,
    certificate_chain: Vec<String>,
    private_key: String,
    not_before: i64,
    not_after: i64,
}

impl<'de> Deserialize<'de> for StoredCertificateState {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct StateVisitor;

        impl<'de> Visitor<'de> for StateVisitor {
            type Value = StoredCertificateState;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a strict MCP endpoint certificate state object")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut environment = None;
                let mut hostname = None;
                let mut certificate_chain = None;
                let mut private_key = None;
                let mut not_before = None;
                let mut not_after = None;
                while let Some(field) = map.next_key::<String>()? {
                    match field.as_str() {
                        "environment" => assign_field(&mut environment, &field, &mut map)?,
                        "hostname" => assign_field(&mut hostname, &field, &mut map)?,
                        "certificate_chain" => {
                            assign_field(&mut certificate_chain, &field, &mut map)?
                        }
                        "private_key" => assign_field(&mut private_key, &field, &mut map)?,
                        "not_before" => assign_field(&mut not_before, &field, &mut map)?,
                        "not_after" => assign_field(&mut not_after, &field, &mut map)?,
                        _ => return Err(de::Error::unknown_field(&field, STATE_FIELDS)),
                    }
                }
                Ok(StoredCertificateState {
                    environment: environment
                        .ok_or_else(|| de::Error::missing_field("environment"))?,
                    hostname: hostname.ok_or_else(|| de::Error::missing_field("hostname"))?,
                    certificate_chain: certificate_chain
                        .ok_or_else(|| de::Error::missing_field("certificate_chain"))?,
                    private_key: private_key
                        .ok_or_else(|| de::Error::missing_field("private_key"))?,
                    not_before: not_before.ok_or_else(|| de::Error::missing_field("not_before"))?,
                    not_after: not_after.ok_or_else(|| de::Error::missing_field("not_after"))?,
                })
            }
        }

        deserializer.deserialize_map(StateVisitor)
    }
}

const STATE_FIELDS: &[&str] = &[
    "environment",
    "hostname",
    "certificate_chain",
    "private_key",
    "not_before",
    "not_after",
];

fn assign_field<'de, A, T>(slot: &mut Option<T>, _field: &str, map: &mut A) -> Result<(), A::Error>
where
    A: MapAccess<'de>,
    T: Deserialize<'de>,
{
    if slot.is_some() {
        return Err(de::Error::custom("duplicate certificate state field"));
    }
    *slot = Some(map.next_value()?);
    Ok(())
}

pub(crate) struct DecodedStoredCertificateState {
    pub(crate) certificate_chain: Vec<Vec<u8>>,
    pub(crate) private_key: Vec<u8>,
    pub(crate) not_before: i64,
    pub(crate) not_after: i64,
}

fn validate_stored_state(
    bytes: &[u8],
    expected_hostname: &str,
    expected_environment: McpEndpointCertificateEnvironment,
) -> Result<ActiveOrdinaryCertificate, McpEndpointTlsError> {
    let decoded = decode_stored_state(bytes, expected_hostname, expected_environment)?;
    if !certificate_is_current(&decoded)? {
        return Err(McpEndpointTlsError::State);
    }
    activate_decoded_stored_state(decoded)
}

pub(crate) enum StoredCertificateClassification {
    Same(DecodedStoredCertificateState),
    Foreign,
}

pub(crate) fn classify_stored_state(
    bytes: &[u8],
    expected_hostname: &str,
    expected_environment: McpEndpointCertificateEnvironment,
) -> Result<StoredCertificateClassification, McpEndpointTlsError> {
    if bytes.len() > unix::MAX_TLS_STATE_BYTES {
        return Err(McpEndpointTlsError::State);
    }
    let state = serde_json::from_slice::<StoredCertificateState>(bytes)
        .map_err(|_| McpEndpointTlsError::State)?;
    let stored_environment = match state.environment.as_str() {
        "staging" => McpEndpointCertificateEnvironment::Staging,
        "production" => McpEndpointCertificateEnvironment::Production,
        _ => return Err(McpEndpointTlsError::State),
    };
    if state.hostname != expected_hostname
        || !is_exact_authorized_hostname(&state.hostname, expected_hostname)
        || state.not_before >= state.not_after
    {
        return Err(McpEndpointTlsError::State);
    }
    let certificate_chain = state
        .certificate_chain
        .iter()
        .map(|value| decode_base64_exact(value))
        .collect::<Result<Vec<_>, _>>()?;
    if certificate_chain.is_empty() {
        return Err(McpEndpointTlsError::State);
    }
    let private_key = decode_base64_exact(&state.private_key)?;
    let parsed = certificate_chain
        .iter()
        .map(|certificate| {
            let (remainder, parsed) =
                parse_x509_certificate(certificate).map_err(|_| McpEndpointTlsError::State)?;
            if !remainder.is_empty() {
                return Err(McpEndpointTlsError::State);
            }
            Ok(parsed)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let leaf = parsed.first().ok_or(McpEndpointTlsError::State)?;
    let names = leaf
        .subject_alternative_name()
        .map_err(|_| McpEndpointTlsError::State)?
        .ok_or(McpEndpointTlsError::State)?;
    if names.value.general_names.len() != 1
        || !matches!(
            names.value.general_names.first(),
            Some(GeneralName::DNSName(name)) if *name == expected_hostname
        )
        || leaf.validity().not_before.timestamp() != state.not_before
        || leaf.validity().not_after.timestamp() != state.not_after
    {
        return Err(McpEndpointTlsError::State);
    }
    for pair in parsed.windows(2) {
        if pair[0].issuer() != pair[1].subject()
            || pair[0]
                .verify_signature(Some(pair[1].public_key()))
                .is_err()
        {
            return Err(McpEndpointTlsError::State);
        }
    }
    let ecdsa_key = EcdsaKeyPair::from_pkcs8(
        &ECDSA_P256_SHA256_FIXED_SIGNING,
        &private_key,
        &SystemRandom::new(),
    )
    .or_else(|_| {
        EcdsaKeyPair::from_pkcs8(
            &ECDSA_P256_SHA256_ASN1_SIGNING,
            &private_key,
            &SystemRandom::new(),
        )
    })
    .map_err(|_| McpEndpointTlsError::State)?;
    if leaf.public_key().subject_public_key.data != ecdsa_key.public_key().as_ref() {
        return Err(McpEndpointTlsError::State);
    }
    if stored_environment == expected_environment {
        Ok(StoredCertificateClassification::Same(
            DecodedStoredCertificateState {
                certificate_chain,
                private_key,
                not_before: state.not_before,
                not_after: state.not_after,
            },
        ))
    } else {
        Ok(StoredCertificateClassification::Foreign)
    }
}

fn decode_stored_state(
    bytes: &[u8],
    expected_hostname: &str,
    expected_environment: McpEndpointCertificateEnvironment,
) -> Result<DecodedStoredCertificateState, McpEndpointTlsError> {
    match classify_stored_state(bytes, expected_hostname, expected_environment)? {
        StoredCertificateClassification::Same(decoded) => Ok(decoded),
        StoredCertificateClassification::Foreign => Err(McpEndpointTlsError::State),
    }
}

fn certificate_is_current(
    state: &DecodedStoredCertificateState,
) -> Result<bool, McpEndpointTlsError> {
    let now = now_unix_seconds()?;
    Ok(now >= state.not_before && now < state.not_after)
}

fn activate_decoded_stored_state(
    state: DecodedStoredCertificateState,
) -> Result<ActiveOrdinaryCertificate, McpEndpointTlsError> {
    let DecodedStoredCertificateState {
        certificate_chain,
        private_key,
        not_before: _,
        not_after,
    } = state;
    let now = now_unix_seconds()?;
    let private_key_der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(private_key));
    let signing_key = rustls::crypto::ring::sign::any_supported_type(&private_key_der)
        .map_err(|_| McpEndpointTlsError::State)?;
    let certificate_chain = certificate_chain
        .into_iter()
        .map(CertificateDer::from)
        .collect();
    let seconds_until_expiry = not_after
        .checked_sub(now)
        .and_then(|seconds| u64::try_from(seconds).ok())
        .filter(|seconds| *seconds > 0)
        .ok_or(McpEndpointTlsError::State)?;
    let expires_at = Instant::now()
        .checked_add(StdDuration::from_secs(seconds_until_expiry))
        .ok_or(McpEndpointTlsError::State)?;
    Ok(ActiveOrdinaryCertificate {
        key: Arc::new(CertifiedKey::new(certificate_chain, signing_key)),
        expires_at,
        not_after,
    })
}

pub(crate) fn validate_acme_account_key(bytes: &[u8]) -> Result<(), McpEndpointTlsError> {
    if bytes.is_empty() || bytes.len() > unix::MAX_TLS_ACME_ACCOUNT_BYTES {
        return Err(McpEndpointTlsError::State);
    }
    EcdsaKeyPair::from_pkcs8(
        &ECDSA_P256_SHA256_FIXED_SIGNING,
        bytes,
        &SystemRandom::new(),
    )
    .or_else(|_| {
        EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, bytes, &SystemRandom::new())
    })
    .map(|_| ())
    .map_err(|_| McpEndpointTlsError::State)
}

fn build_acme_challenge_key(
    hostname: &str,
    key_authorization_digest: &[u8; 32],
) -> Result<Arc<CertifiedKey>, McpEndpointTlsError> {
    let now = OffsetDateTime::now_utc();
    let key_pair =
        KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).map_err(|_| McpEndpointTlsError::State)?;
    let mut params = CertificateParams::new(vec![hostname.to_owned()])
        .map_err(|_| McpEndpointTlsError::State)?;
    params.distinguished_name = DistinguishedName::new();
    params.not_before = now - Duration::minutes(1);
    params.not_after = now + Duration::minutes(10);
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    let mut extension = CustomExtension::from_oid_content(
        ACME_IDENTIFIER_OID,
        der_octet_string(key_authorization_digest),
    );
    extension.set_criticality(true);
    params.custom_extensions.push(extension);
    let certificate = params
        .self_signed(&key_pair)
        .map_err(|_| McpEndpointTlsError::State)?;
    let private_key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_pair.serialize_der()));
    let signing_key = rustls::crypto::ring::sign::any_supported_type(&private_key)
        .map_err(|_| McpEndpointTlsError::State)?;
    Ok(Arc::new(CertifiedKey::new(
        vec![CertificateDer::from(certificate.der().to_vec())],
        signing_key,
    )))
}

fn der_octet_string(value: &[u8; 32]) -> Vec<u8> {
    let mut encoded = Vec::with_capacity(34);
    encoded.extend_from_slice(&[0x04, 0x20]);
    encoded.extend_from_slice(value);
    encoded
}

fn decode_base64_exact(value: &str) -> Result<Vec<u8>, McpEndpointTlsError> {
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(value)
        .map_err(|_| McpEndpointTlsError::State)?;
    if base64::engine::general_purpose::STANDARD.encode(&decoded) != value {
        return Err(McpEndpointTlsError::State);
    }
    Ok(decoded)
}

fn now_unix_seconds() -> Result<i64, McpEndpointTlsError> {
    Ok(current_epoch_seconds())
}

fn environment_name(environment: McpEndpointCertificateEnvironment) -> &'static str {
    match environment {
        McpEndpointCertificateEnvironment::Staging => "staging",
        McpEndpointCertificateEnvironment::Production => "production",
    }
}

fn is_exact_authorized_hostname(candidate: &str, expected: &str) -> bool {
    !expected.is_empty()
        && candidate == expected
        && candidate.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'.' || byte == b'-'
        })
}

fn is_acme_attempt(protocols: &[&[u8]]) -> bool {
    protocols.contains(&ACME_TLS_ALPN)
}

fn is_only_acme_alpn(protocols: &[&[u8]]) -> bool {
    protocols == [ACME_TLS_ALPN]
}

#[cfg(test)]
#[cfg(unix)]
pub(crate) mod test_helpers {
    use std::io::Cursor;
    use std::sync::Arc;

    use base64::Engine;
    use rustls::pki_types::{CertificateDer, ServerName};
    use rustls::{ClientConfig, ClientConnection, RootCertStore, ServerConnection};

    pub(crate) const HOSTNAME: &str = "ab12cd34.solstone.me";

    #[cfg(feature = "full-tests")]
    pub(crate) fn mint_test_leaf(
        not_before: i64,
        not_after: i64,
    ) -> (Vec<u8>, Vec<u8>, CertificateDer<'static>) {
        use rcgen::{CertificateParams, KeyPair, PKCS_ECDSA_P256_SHA256};
        use time::OffsetDateTime;

        let keypair = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("leaf key");
        let mut params = CertificateParams::new(vec![HOSTNAME.to_owned()]).expect("params");
        params.use_authority_key_identifier_extension = true;
        params.not_before = OffsetDateTime::from_unix_timestamp(not_before).expect("not_before");
        params.not_after = OffsetDateTime::from_unix_timestamp(not_after).expect("not_after");
        let certificate = params.self_signed(&keypair).expect("certificate");
        let der = certificate.der().to_vec();
        let key_der = keypair.serialize_der();
        (der.clone(), key_der, CertificateDer::from(der))
    }

    #[cfg_attr(not(feature = "full-tests"), allow(dead_code))]
    pub(crate) fn pem_from_der(der: &[u8]) -> String {
        format!(
            "-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\n",
            base64::engine::general_purpose::STANDARD.encode(der)
        )
    }

    pub(crate) fn trusted_client_config(
        certificate: CertificateDer<'static>,
        alpn: Vec<Vec<u8>>,
    ) -> Arc<ClientConfig> {
        let mut roots = RootCertStore::empty();
        roots.add(certificate).expect("fixture root");
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut config = ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .expect("ring provider supports TLS 1.3")
            .with_root_certificates(roots)
            .with_no_client_auth();
        config.alpn_protocols = alpn;
        Arc::new(config)
    }

    #[derive(Debug)]
    pub(crate) struct AcmeTestVerifier;

    impl rustls::client::danger::ServerCertVerifier for AcmeTestVerifier {
        fn verify_server_cert(
            &self,
            _: &CertificateDer<'_>,
            _: &[CertificateDer<'_>],
            _: &ServerName<'_>,
            _: &[u8],
            _: rustls::pki_types::UnixTime,
        ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            _: &[u8],
            _: &CertificateDer<'_>,
            _: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            Err(rustls::Error::General(
                "test client permits TLS 1.3 only".to_owned(),
            ))
        }

        fn verify_tls13_signature(
            &self,
            _: &[u8],
            _: &CertificateDer<'_>,
            _: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }

        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            rustls::crypto::ring::default_provider()
                .signature_verification_algorithms
                .supported_schemes()
        }
    }

    pub(crate) fn acme_test_client_config(alpn: Vec<Vec<u8>>) -> Arc<ClientConfig> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut config = ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .expect("ring provider supports TLS 1.3")
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcmeTestVerifier))
            .with_no_client_auth();
        config.alpn_protocols = alpn;
        Arc::new(config)
    }

    pub(crate) fn complete_handshake(
        client: &mut ClientConnection,
        server: &mut ServerConnection,
    ) -> Result<(), Box<dyn std::error::Error>> {
        for _ in 0..16 {
            let mut client_bytes = Vec::new();
            client.write_tls(&mut client_bytes)?;
            if !client_bytes.is_empty() {
                server.read_tls(&mut Cursor::new(client_bytes))?;
                server.process_new_packets()?;
            }
            let mut server_bytes = Vec::new();
            server.write_tls(&mut server_bytes)?;
            if !server_bytes.is_empty() {
                client.read_tls(&mut Cursor::new(server_bytes))?;
                client.process_new_packets()?;
            }
            if !client.is_handshaking() && !server.is_handshaking() {
                return Ok(());
            }
        }
        Err("fixture handshake did not converge".into())
    }

    #[allow(clippy::type_complexity)]
    pub(crate) enum ScriptedHandler {
        Sync(
            Arc<
                dyn Fn(&str, &str, &[u8]) -> axum::http::Response<axum::body::Body>
                    + Send
                    + Sync
                    + 'static,
            >,
        ),
        #[cfg(feature = "full-tests")]
        Async(
            Arc<
                dyn Fn(
                        String,
                        String,
                        Vec<u8>,
                    ) -> std::pin::Pin<
                        Box<
                            dyn std::future::Future<Output = axum::http::Response<axum::body::Body>>
                                + Send,
                        >,
                    > + Send
                    + Sync
                    + 'static,
            >,
        ),
    }

    #[cfg_attr(not(feature = "full-tests"), allow(dead_code))]
    pub(crate) struct ScriptedHttpClient {
        #[allow(clippy::type_complexity)]
        pub(crate) requests: Arc<std::sync::Mutex<Vec<(String, String, Vec<u8>)>>>,
        pub(crate) handler: ScriptedHandler,
    }

    #[cfg_attr(not(feature = "full-tests"), allow(dead_code))]
    impl ScriptedHttpClient {
        #[allow(clippy::type_complexity)]
        pub(crate) fn new<F>(
            handler: F,
        ) -> (
            Arc<Self>,
            Arc<std::sync::Mutex<Vec<(String, String, Vec<u8>)>>>,
        )
        where
            F: Fn(&str, &str, &[u8]) -> axum::http::Response<axum::body::Body>
                + Send
                + Sync
                + 'static,
        {
            let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
            (
                Arc::new(Self {
                    requests: Arc::clone(&requests),
                    handler: ScriptedHandler::Sync(Arc::new(handler)),
                }),
                requests,
            )
        }

        #[cfg(feature = "full-tests")]
        #[allow(clippy::type_complexity)]
        pub(crate) fn new_async<F, Fut>(
            handler: F,
        ) -> (
            Arc<Self>,
            Arc<std::sync::Mutex<Vec<(String, String, Vec<u8>)>>>,
        )
        where
            F: Fn(String, String, Vec<u8>) -> Fut + Send + Sync + 'static,
            Fut: std::future::Future<Output = axum::http::Response<axum::body::Body>>
                + Send
                + 'static,
        {
            let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
            let wrapped = Arc::new(move |m: String, u: String, b: Vec<u8>| {
                Box::pin(handler(m, u, b))
                    as std::pin::Pin<
                        Box<
                            dyn std::future::Future<Output = axum::http::Response<axum::body::Body>>
                                + Send,
                        >,
                    >
            });
            (
                Arc::new(Self {
                    requests: Arc::clone(&requests),
                    handler: ScriptedHandler::Async(wrapped),
                }),
                requests,
            )
        }
    }

    #[cfg_attr(not(feature = "full-tests"), allow(dead_code))]
    impl instant_acme::HttpClient for ScriptedHttpClient {
        fn request(
            &self,
            req: axum::http::Request<instant_acme::BodyWrapper<bytes::Bytes>>,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<instant_acme::BytesResponse, instant_acme::Error>,
                    > + Send,
            >,
        > {
            let (parts, mut body) = req.into_parts();
            let method = parts.method.to_string();
            let uri = parts.uri.to_string();
            let requests = Arc::clone(&self.requests);
            let handler = match &self.handler {
                ScriptedHandler::Sync(h) => ScriptedHandler::Sync(Arc::clone(h)),
                #[cfg(feature = "full-tests")]
                ScriptedHandler::Async(h) => ScriptedHandler::Async(Arc::clone(h)),
            };
            Box::pin(async move {
                use hyper::body::Body;
                let frame =
                    futures::future::poll_fn(|cx| std::pin::Pin::new(&mut body).poll_frame(cx))
                        .await;
                let body_bytes = frame
                    .and_then(|f| f.ok())
                    .and_then(|f| f.into_data().ok())
                    .unwrap_or_default();
                if let Ok(mut reqs) = requests.lock() {
                    reqs.push((method.clone(), uri.clone(), body_bytes.to_vec()));
                }
                let response = match handler {
                    ScriptedHandler::Sync(h) => h(&method, &uri, &body_bytes),
                    #[cfg(feature = "full-tests")]
                    ScriptedHandler::Async(h) => h(method, uri, body_bytes.to_vec()).await,
                };
                Ok(instant_acme::BytesResponse::from(response))
            })
        }
    }

    #[cfg_attr(not(feature = "full-tests"), allow(dead_code))]
    #[derive(Default, Clone)]
    pub(crate) struct AcmeServerGates {
        #[cfg(feature = "full-tests")]
        pub hold_authz: Option<Arc<tokio::sync::Notify>>,
        #[cfg(feature = "full-tests")]
        pub hold_cert: Option<Arc<tokio::sync::Notify>>,
        pub reject_replaces_once: Option<Arc<std::sync::atomic::AtomicBool>>,
        pub resume_problem: Option<(u16, String)>,
        pub resume_already_valid: bool,
        pub before_new_order_response: Option<Arc<dyn Fn() + Send + Sync>>,
    }

    struct MockAcmeState {
        effective_acct_string: String,
        ari_status_cell: std::sync::atomic::AtomicU16,
        finalized: std::sync::atomic::AtomicBool,
        directory_url: String,
        has_ari: bool,
        ari_window: (i64, i64),
        cert_pem: String,
        expected_replaces: Option<String>,
        on_challenge: Option<Arc<dyn Fn() + Send + Sync>>,
    }

    #[cfg_attr(not(feature = "full-tests"), allow(dead_code))]
    #[allow(clippy::collapsible_if)]
    fn handle_mock_acme_sync(
        method: &str,
        uri: &str,
        body: &[u8],
        state: &MockAcmeState,
        gates: &AcmeServerGates,
    ) -> axum::http::Response<axum::body::Body> {
        if method == "GET" && uri == state.directory_url {
            let json = if state.has_ari {
                serde_json::json!({
                    "newNonce": "https://acme.example/new-nonce",
                    "newAccount": "https://acme.example/new-account",
                    "newOrder": "https://acme.example/new-order",
                    "renewalInfo": "https://acme.example/ari"
                })
            } else {
                serde_json::json!({
                    "newNonce": "https://acme.example/new-nonce",
                    "newAccount": "https://acme.example/new-account",
                    "newOrder": "https://acme.example/new-order"
                })
            };
            return axum::http::Response::builder()
                .status(200)
                .header("content-type", "application/json")
                .body(axum::body::Body::from(json.to_string()))
                .unwrap();
        }
        if (method == "HEAD" || method == "GET") && uri.ends_with("/new-nonce") {
            return axum::http::Response::builder()
                .status(200)
                .header("replay-nonce", "test-nonce-1")
                .header("content-type", "text/plain")
                .body(axum::body::Body::empty())
                .unwrap();
        }
        if method == "POST" && uri.ends_with("/new-account") {
            let json = serde_json::json!({
                "status": "valid",
                "orders": format!("{}/orders", state.effective_acct_string)
            });
            return axum::http::Response::builder()
                .status(201)
                .header("location", state.effective_acct_string.as_str())
                .header("replay-nonce", "test-nonce-account")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(json.to_string()))
                .unwrap();
        }
        if method == "GET" && uri.starts_with("https://acme.example/ari/") {
            let status = state
                .ari_status_cell
                .load(std::sync::atomic::Ordering::SeqCst);
            if status == 503 {
                state
                    .ari_status_cell
                    .store(200, std::sync::atomic::Ordering::SeqCst);
                return axum::http::Response::builder()
                    .status(503)
                    .header("retry-after", "120")
                    .header("content-type", "application/problem+json")
                    .body(axum::body::Body::from(
                        r#"{"type":"urn:ietf:params:acme:error:serverInternal"}"#,
                    ))
                    .unwrap();
            }
            let window = if let Some(ref exp_rep) = state.expected_replaces {
                if uri.ends_with(exp_rep) {
                    state.ari_window
                } else {
                    (state.ari_window.0 + 100_000, state.ari_window.1 + 100_000)
                }
            } else {
                state.ari_window
            };
            let start_dt = time::OffsetDateTime::from_unix_timestamp(window.0)
                .unwrap()
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap();
            let end_dt = time::OffsetDateTime::from_unix_timestamp(window.1)
                .unwrap()
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap();
            let json = serde_json::json!({
                "suggestedWindow": {
                    "start": start_dt,
                    "end": end_dt
                }
            });
            return axum::http::Response::builder()
                .status(200)
                .header("retry-after", "3600")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(json.to_string()))
                .unwrap();
        }
        if method == "POST" && uri.ends_with("/new-order") {
            if let Some(ref hook) = gates.before_new_order_response {
                hook();
            }
            if let Ok(jws) = serde_json::from_slice::<serde_json::Value>(body) {
                if let Some(payload_b64) = jws.get("payload").and_then(|v| v.as_str()) {
                    if let Ok(payload_bytes) =
                        base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload_b64)
                    {
                        if let Ok(order_payload) =
                            serde_json::from_slice::<serde_json::Value>(&payload_bytes)
                        {
                            if let Some(ref flag) = gates.reject_replaces_once {
                                if order_payload.get("replaces").is_some()
                                    && flag.swap(false, std::sync::atomic::Ordering::SeqCst)
                                {
                                    let problem = serde_json::json!({
                                        "type": "urn:ietf:params:acme:error:alreadyReplaced",
                                        "detail": "already replaced",
                                        "status": 409
                                    });
                                    return axum::http::Response::builder()
                                        .status(409)
                                        .header("replay-nonce", "test-nonce-conflict")
                                        .header("content-type", "application/problem+json")
                                        .body(axum::body::Body::from(problem.to_string()))
                                        .unwrap();
                                }
                            }
                            if let Some(ref exp_rep) = state.expected_replaces {
                                let actual_rep =
                                    order_payload.get("replaces").and_then(|v| v.as_str());
                                assert_eq!(
                                    actual_rep,
                                    Some(exp_rep.as_str()),
                                    "replaces mismatch in new-order"
                                );
                            } else if gates.reject_replaces_once.is_none() {
                                assert!(
                                    order_payload.get("replaces").is_none(),
                                    "unexpected replaces in new-order"
                                );
                            }
                        }
                    }
                }
            }
            let mut json = serde_json::json!({
                "status": "pending",
                "expires": "2030-01-01T00:00:00Z",
                "identifiers": [{ "type": "dns", "value": "ab12cd34.solstone.me" }],
                "authorizations": ["https://acme.example/authz/1"],
                "finalize": "https://acme.example/order/1/finalize",
                "error": null,
                "certificate": null
            });
            if let Some(ref exp_rep) = state.expected_replaces {
                json["replaces"] = serde_json::Value::String(exp_rep.clone());
            }
            return axum::http::Response::builder()
                .status(201)
                .header("location", "https://acme.example/order/1")
                .header("replay-nonce", "test-nonce-order")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(json.to_string()))
                .unwrap();
        }
        if method == "POST" && uri == "https://acme.example/authz/1" {
            let json = serde_json::json!({
                "status": "pending",
                "identifier": { "type": "dns", "value": "ab12cd34.solstone.me" },
                "challenges": [
                    {
                        "type": "tls-alpn-01",
                        "url": "https://acme.example/chall/1",
                        "token": "dGVzdC10b2tlbg",
                        "status": "pending",
                        "error": null
                    }
                ]
            });
            return axum::http::Response::builder()
                .status(200)
                .header("replay-nonce", "test-nonce-authz")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(json.to_string()))
                .unwrap();
        }
        if method == "POST" && uri == "https://acme.example/chall/1" {
            if let Some(ref hook) = state.on_challenge {
                hook();
            }
            let json = serde_json::json!({
                "type": "tls-alpn-01",
                "url": "https://acme.example/chall/1",
                "status": "valid",
                "token": "dGVzdC10b2tlbg",
                "error": null
            });
            return axum::http::Response::builder()
                .status(200)
                .header("replay-nonce", "test-nonce-chall")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(json.to_string()))
                .unwrap();
        }
        if method == "POST"
            && (uri == "https://acme.example/order/1" || uri.ends_with("/order/existing"))
        {
            if let Some((status, ref err_type)) = gates.resume_problem {
                let problem = serde_json::json!({
                    "type": err_type,
                    "detail": "resume",
                    "status": status
                });
                return axum::http::Response::builder()
                    .status(status)
                    .header("replay-nonce", "test-nonce-order-problem")
                    .header("content-type", "application/problem+json")
                    .body(axum::body::Body::from(problem.to_string()))
                    .unwrap();
            }
            if gates.resume_already_valid {
                let mut json = serde_json::json!({
                    "status": "valid",
                    "expires": "2030-01-01T00:00:00Z",
                    "identifiers": [{ "type": "dns", "value": "ab12cd34.solstone.me" }],
                    "authorizations": ["https://acme.example/authz/1"],
                    "finalize": "https://acme.example/order/1/finalize",
                    "error": null,
                    "certificate": "https://acme.example/cert/1"
                });
                if let Some(ref exp_rep) = state.expected_replaces {
                    json["replaces"] = serde_json::Value::String(exp_rep.clone());
                }
                return axum::http::Response::builder()
                    .status(200)
                    .header("replay-nonce", "test-nonce-refresh")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(json.to_string()))
                    .unwrap();
            }
            let is_fin = state.finalized.load(std::sync::atomic::Ordering::SeqCst);
            let mut json = if is_fin {
                serde_json::json!({
                    "status": "valid",
                    "expires": "2030-01-01T00:00:00Z",
                    "identifiers": [{ "type": "dns", "value": "ab12cd34.solstone.me" }],
                    "authorizations": ["https://acme.example/authz/1"],
                    "finalize": "https://acme.example/order/1/finalize",
                    "error": null,
                    "certificate": "https://acme.example/cert/1"
                })
            } else {
                serde_json::json!({
                    "status": "ready",
                    "expires": "2030-01-01T00:00:00Z",
                    "identifiers": [{ "type": "dns", "value": "ab12cd34.solstone.me" }],
                    "authorizations": ["https://acme.example/authz/1"],
                    "finalize": "https://acme.example/order/1/finalize",
                    "error": null,
                    "certificate": null
                })
            };
            if let Some(ref exp_rep) = state.expected_replaces {
                json["replaces"] = serde_json::Value::String(exp_rep.clone());
            }
            return axum::http::Response::builder()
                .status(200)
                .header("replay-nonce", "test-nonce-refresh")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(json.to_string()))
                .unwrap();
        }
        if method == "POST" && uri.ends_with("/finalize") {
            state
                .finalized
                .store(true, std::sync::atomic::Ordering::SeqCst);
            let mut json = serde_json::json!({
                "status": "valid",
                "expires": "2030-01-01T00:00:00Z",
                "identifiers": [{ "type": "dns", "value": "ab12cd34.solstone.me" }],
                "authorizations": ["https://acme.example/authz/1"],
                "finalize": "https://acme.example/order/1/finalize",
                "error": null,
                "certificate": "https://acme.example/cert/1"
            });
            if let Some(ref exp_rep) = state.expected_replaces {
                json["replaces"] = serde_json::Value::String(exp_rep.clone());
            }
            return axum::http::Response::builder()
                .status(200)
                .header("replay-nonce", "test-nonce-fin")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(json.to_string()))
                .unwrap();
        }
        if method == "POST" && uri.ends_with("/cert/1") {
            return axum::http::Response::builder()
                .status(200)
                .header("replay-nonce", "test-nonce-cert")
                .header("content-type", "application/pem-certificate-chain")
                .body(axum::body::Body::from(state.cert_pem.clone()))
                .unwrap();
        }
        panic!("unexpected mock ACME request: {method} {uri}");
    }

    #[cfg_attr(not(feature = "full-tests"), allow(dead_code))]
    #[allow(clippy::type_complexity, clippy::too_many_arguments)]
    pub(crate) fn mock_acme_server(
        directory_url: String,
        has_ari: bool,
        ari_status: u16,
        ari_window: (i64, i64),
        cert_pem: String,
        expected_replaces: Option<String>,
        on_challenge: Option<Arc<dyn Fn() + Send + Sync>>,
        account_url: Option<&str>,
    ) -> (
        Arc<ScriptedHttpClient>,
        Arc<std::sync::Mutex<Vec<(String, String, Vec<u8>)>>>,
    ) {
        let state = Arc::new(MockAcmeState {
            effective_acct_string: account_url
                .unwrap_or("https://acme-staging-v02.api.letsencrypt.org/acme/acct/1")
                .to_string(),
            ari_status_cell: std::sync::atomic::AtomicU16::new(ari_status),
            finalized: std::sync::atomic::AtomicBool::new(false),
            directory_url,
            has_ari,
            ari_window,
            cert_pem,
            expected_replaces,
            on_challenge,
        });
        let state_clone = Arc::clone(&state);
        let gates = AcmeServerGates::default();
        let (client, requests) = ScriptedHttpClient::new(move |method, uri, body| {
            handle_mock_acme_sync(method, uri, body, &state_clone, &gates)
        });
        (client, requests)
    }

    #[cfg(feature = "full-tests")]
    #[allow(clippy::type_complexity, clippy::too_many_arguments)]
    pub(crate) fn mock_acme_server_gated(
        directory_url: String,
        has_ari: bool,
        ari_status: u16,
        ari_window: (i64, i64),
        cert_pem: String,
        expected_replaces: Option<String>,
        on_challenge: Option<Arc<dyn Fn() + Send + Sync>>,
        account_url: Option<&str>,
        gates: AcmeServerGates,
    ) -> (
        Arc<ScriptedHttpClient>,
        Arc<std::sync::Mutex<Vec<(String, String, Vec<u8>)>>>,
    ) {
        let state = Arc::new(MockAcmeState {
            effective_acct_string: account_url
                .unwrap_or("https://acme-staging-v02.api.letsencrypt.org/acme/acct/1")
                .to_string(),
            ari_status_cell: std::sync::atomic::AtomicU16::new(ari_status),
            finalized: std::sync::atomic::AtomicBool::new(false),
            directory_url,
            has_ari,
            ari_window,
            cert_pem,
            expected_replaces,
            on_challenge,
        });
        let state_clone = Arc::clone(&state);
        let (client, requests) = ScriptedHttpClient::new_async(move |method, uri, body| {
            let state = Arc::clone(&state_clone);
            let gates = gates.clone();
            let body_vec = body.to_vec();
            let method_str = method.to_string();
            let uri_str = uri.to_string();
            Box::pin(async move {
                if method_str == "POST"
                    && uri_str == "https://acme.example/authz/1"
                    && let Some(ref notify) = gates.hold_authz
                {
                    notify.notified().await;
                }
                if method_str == "POST"
                    && uri_str.ends_with("/cert/1")
                    && let Some(ref notify) = gates.hold_cert
                {
                    notify.notified().await;
                }
                handle_mock_acme_sync(&method_str, &uri_str, &body_vec, &state, &gates)
            })
        });
        (client, requests)
    }
}

#[cfg(all(test, not(feature = "full-tests")))]
// These exercise the owner state layer through Unix file modes.
#[cfg(unix)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;

    use rcgen::{CertificateParams, KeyPair, PKCS_ECDSA_P256_SHA256};
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
    use rustls::{ClientConnection, ServerConnection};
    use solstone_core_journal_io::journal_root::JournalRoot;
    use tempfile::TempDir;

    use super::test_helpers::*;
    use super::*;

    fn fixture_key() -> (Arc<CertifiedKey>, CertificateDer<'static>) {
        let key_pair = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("fixture key");
        let certificate = CertificateParams::new(vec![HOSTNAME.to_owned()])
            .expect("fixture params")
            .self_signed(&key_pair)
            .expect("fixture certificate");
        let cert = CertificateDer::from(certificate.der().to_vec());
        let private_key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_pair.serialize_der()));
        let signing_key = rustls::crypto::ring::sign::any_supported_type(&private_key)
            .expect("fixture signing key");
        (
            Arc::new(CertifiedKey::new(vec![cert.clone()], signing_key)),
            cert,
        )
    }

    fn state_fixture() -> (Vec<Vec<u8>>, Vec<u8>, i64, i64, CertificateDer<'static>) {
        let key_pair = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("state key");
        let now = OffsetDateTime::from_unix_timestamp(current_epoch_seconds()).unwrap();
        let mut params = CertificateParams::new(vec![HOSTNAME.to_owned()]).expect("state params");
        params.use_authority_key_identifier_extension = true;
        params.not_before = now - Duration::minutes(1);
        params.not_after = now + Duration::hours(1);
        let not_before = params.not_before.unix_timestamp();
        let not_after = params.not_after.unix_timestamp();
        let certificate = params.self_signed(&key_pair).expect("state certificate");
        let der = certificate.der().to_vec();
        (
            vec![der.clone()],
            key_pair.serialize_der(),
            not_before,
            not_after,
            CertificateDer::from(der),
        )
    }

    fn expired_state_fixture() -> (Vec<Vec<u8>>, Vec<u8>, i64, i64) {
        let key_pair = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("expired state key");
        let now = OffsetDateTime::now_utc();
        let mut params =
            CertificateParams::new(vec![HOSTNAME.to_owned()]).expect("expired state params");
        params.use_authority_key_identifier_extension = true;
        params.not_before = now - Duration::hours(2);
        params.not_after = now - Duration::hours(1);
        let not_before = params.not_before.unix_timestamp();
        let not_after = params.not_after.unix_timestamp();
        let certificate = params
            .self_signed(&key_pair)
            .expect("expired state certificate");
        (
            vec![certificate.der().to_vec()],
            key_pair.serialize_der(),
            not_before,
            not_after,
        )
    }

    #[test]
    fn config_is_tls13_and_advertises_the_exact_two_protocols() {
        let _clock = lock_test_clock(None);
        let service = McpEndpointTlsService::empty_for_test(HOSTNAME.to_owned());
        let config = mcp_endpoint_server_config(&service);
        assert_eq!(
            config.alpn_protocols,
            vec![ACME_TLS_ALPN.to_vec(), HTTP11_ALPN.to_vec()]
        );
    }

    #[test]
    fn reconnect_authority_must_match_the_existing_tls_hostname() {
        let _clock = lock_test_clock(None);
        let service = McpEndpointTlsService::empty_for_test(HOSTNAME.to_owned());
        assert!(service.matches_authorized_hostname(HOSTNAME));
        assert!(!service.matches_authorized_hostname("other.solstone.me"));
        assert!(!service.matches_authorized_hostname("AB12CD34.solstone.me"));
    }

    #[test]
    fn resolver_never_has_a_default_certificate_without_exact_sni() {
        let _clock = lock_test_clock(None);
        let service = McpEndpointTlsService::empty_for_test(HOSTNAME.to_owned());
        service.install_ordinary_for_test(fixture_key().0);
        assert!(is_exact_authorized_hostname(HOSTNAME, HOSTNAME));
        assert!(is_exact_authorized_hostname(
            "mcp.my-domain.org",
            "mcp.my-domain.org"
        ));
        assert!(!is_exact_authorized_hostname(
            "AB12CD34.solstone.me",
            HOSTNAME
        ));
        assert!(!is_exact_authorized_hostname("other.solstone.me", HOSTNAME));
        assert!(!is_exact_authorized_hostname(
            "x.ab12cd34.solstone.me",
            HOSTNAME
        ));
    }

    #[test]
    fn acme_attempt_requires_one_exact_protocol_and_an_active_generation() {
        let _clock = lock_test_clock(None);
        let service = McpEndpointTlsService::empty_for_test(HOSTNAME.to_owned());
        let first = service
            .install_acme_challenge([1; 32])
            .expect("challenge installs");
        let second = service
            .install_acme_challenge([2; 32])
            .expect("replacement installs");
        drop(first);
        assert!(service.resolver.challenge.load_full().is_some());
        drop(second);
        assert!(service.resolver.challenge.load_full().is_none());
        assert!(is_acme_attempt(&[ACME_TLS_ALPN]));
        assert!(is_only_acme_alpn(&[ACME_TLS_ALPN]));
        assert!(!is_only_acme_alpn(&[ACME_TLS_ALPN, HTTP11_ALPN]));
        assert!(!is_only_acme_alpn(&[HTTP11_ALPN, ACME_TLS_ALPN]));
    }

    #[test]
    fn ordinary_install_is_owner_only_durable_and_reloads_before_activation() {
        let _clock = lock_test_clock(None);
        let root = TempDir::new().expect("state root");
        let (chain, private_key, not_before, not_after, certificate) = state_fixture();
        let service = McpEndpointTlsService::for_authorized_hostname(
            Arc::new(JournalRoot::open(root.path()).expect("journal root")),
            HOSTNAME.to_owned(),
            McpEndpointCertificateEnvironment::Staging,
            false,
        )
        .expect("empty state service");
        service
            .install_ordinary_certificate(chain, private_key, not_before, not_after)
            .expect("validated state installs");
        assert_eq!(
            fs::metadata(root.path().join("mcp-endpoint/tls"))
                .expect("tls directory")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(root.path().join("mcp-endpoint/tls/state.json"))
                .expect("tls state")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let reloaded = McpEndpointTlsService::for_authorized_hostname(
            Arc::new(JournalRoot::open(root.path()).expect("reopened journal root")),
            HOSTNAME.to_owned(),
            McpEndpointCertificateEnvironment::Staging,
            false,
        )
        .expect("reloaded valid state");
        let mut client = ClientConnection::new(
            trusted_client_config(certificate, vec![HTTP11_ALPN.to_vec()]),
            ServerName::try_from(HOSTNAME.to_owned()).expect("fixture hostname"),
        )
        .expect("ordinary client");
        let mut server =
            ServerConnection::new(mcp_endpoint_server_config(&reloaded)).expect("reloaded server");
        complete_handshake(&mut client, &mut server).expect("reloaded certificate handshakes");
    }

    #[test]
    fn expired_state_is_not_served_but_is_retained_for_acme_recovery() {
        let _clock = lock_test_clock(None);
        let root = TempDir::new().expect("state root");
        let (chain, private_key, not_before, not_after) = expired_state_fixture();
        let service = McpEndpointTlsService::for_authorized_hostname(
            Arc::new(JournalRoot::open(root.path()).expect("journal root")),
            HOSTNAME.to_owned(),
            McpEndpointCertificateEnvironment::Staging,
            false,
        )
        .expect("empty service");
        let state = StoredCertificateState {
            environment: "staging".to_owned(),
            hostname: HOSTNAME.to_owned(),
            certificate_chain: chain
                .into_iter()
                .map(|value| base64::engine::general_purpose::STANDARD.encode(value))
                .collect(),
            private_key: base64::engine::general_purpose::STANDARD.encode(private_key),
            not_before,
            not_after,
        };
        let encoded = serde_json::to_vec(&state).expect("state JSON");
        let store = service.store.as_ref().expect("state store");
        unix::persist_tls_state_bytes(&store.directory, &encoded).expect("persist expired state");

        let recovered = McpEndpointTlsService::for_authorized_hostname(
            Arc::new(JournalRoot::open(root.path()).expect("reopened journal root")),
            HOSTNAME.to_owned(),
            McpEndpointCertificateEnvironment::Staging,
            false,
        )
        .expect("expired state is structurally valid");
        assert!(recovered.resolver.ordinary.load_full().is_none());

        let store = recovered.store.as_ref().expect("state store");
        let bytes = unix::read_tls_state_bytes(&store.directory)
            .expect("read state")
            .expect("expired certificate state remains on disk");
        assert!(matches!(
            classify_stored_state(&bytes, HOSTNAME, McpEndpointCertificateEnvironment::Staging),
            Ok(StoredCertificateClassification::Same(_))
        ));
    }

    #[test]
    fn forced_staging_renewal_withholds_an_otherwise_current_cached_certificate() {
        let _clock = lock_test_clock(None);
        let root = TempDir::new().expect("state root");
        let (chain, private_key, not_before, not_after, _) = state_fixture();
        let ordinary = McpEndpointTlsService::for_authorized_hostname(
            Arc::new(JournalRoot::open(root.path()).expect("journal root")),
            HOSTNAME.to_owned(),
            McpEndpointCertificateEnvironment::Staging,
            false,
        )
        .expect("empty service");
        ordinary
            .install_ordinary_certificate(chain, private_key, not_before, not_after)
            .expect("current state installs");

        let forced = McpEndpointTlsService::for_authorized_hostname(
            Arc::new(JournalRoot::open(root.path()).expect("reopened journal root")),
            HOSTNAME.to_owned(),
            McpEndpointCertificateEnvironment::Staging,
            true,
        )
        .expect("forced service opens current state");
        assert!(forced.force_staging_renewal());
        assert!(
            forced.resolver.ordinary.load_full().is_some(),
            "the last current certificate remains available during reissuance"
        );
        assert!(matches!(
            McpEndpointTlsService::for_authorized_hostname(
                Arc::new(JournalRoot::open(root.path()).expect("production journal root")),
                HOSTNAME.to_owned(),
                McpEndpointCertificateEnvironment::Production,
                true,
            ),
            Err(McpEndpointTlsError::State)
        ));
    }

    #[test]
    fn acme_account_state_is_environment_scoped_and_owner_only() {
        let _clock = lock_test_clock(None);
        let root = TempDir::new().expect("state root");
        let service = McpEndpointTlsService::for_authorized_hostname(
            Arc::new(JournalRoot::open(root.path()).expect("journal root")),
            HOSTNAME.to_owned(),
            McpEndpointCertificateEnvironment::Staging,
            false,
        )
        .expect("empty service");
        let account =
            EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &SystemRandom::new())
                .expect("account key");
        let store = service.store.as_ref().expect("state store");
        unix::persist_tls_acme_account_bytes(&store.directory, false, account.as_ref())
            .expect("persist staging account");
        assert!(
            unix::read_tls_acme_account_bytes(&store.directory, true)
                .expect("read production account")
                .is_none()
        );
        assert_eq!(
            fs::metadata(root.path().join("mcp-endpoint/tls/account-staging.pk8"))
                .expect("staging account")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn renewal_honors_a_preexisting_shutdown_without_acme_work() {
        let _clock = lock_test_clock(None);
        let root = TempDir::new().expect("state root");
        let service = McpEndpointTlsService::for_authorized_hostname(
            Arc::new(JournalRoot::open(root.path()).expect("journal root")),
            HOSTNAME.to_owned(),
            McpEndpointCertificateEnvironment::Staging,
            false,
        )
        .expect("empty service");
        let (_sender, mut shutdown) = tokio::sync::watch::channel(true);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime
            .block_on(service.run_acme_renewal(&mut shutdown))
            .expect("preexisting shutdown is clean");
        assert!(
            !root
                .path()
                .join("mcp-endpoint/tls/account-staging.pk8")
                .exists()
        );
        assert!(!root.path().join("mcp-endpoint/tls/state.json").exists());
    }

    #[test]
    fn state_parser_rejects_duplicate_unknown_and_environment_mismatch() {
        let _clock = lock_test_clock(None);
        let duplicate = br#"{
            "environment":"staging","environment":"staging",
            "hostname":"ab12cd34.solstone.me","certificate_chain":[],
            "private_key":"","not_before":1,"not_after":2
        }"#;
        assert!(matches!(
            validate_stored_state(
                duplicate,
                HOSTNAME,
                McpEndpointCertificateEnvironment::Staging
            ),
            Err(McpEndpointTlsError::State)
        ));
        let unknown = br#"{
            "environment":"staging","hostname":"ab12cd34.solstone.me",
            "certificate_chain":[],"private_key":"","not_before":1,
            "not_after":2,"other":true
        }"#;
        assert!(matches!(
            validate_stored_state(
                unknown,
                HOSTNAME,
                McpEndpointCertificateEnvironment::Staging
            ),
            Err(McpEndpointTlsError::State)
        ));
    }

    #[test]
    fn staging_state_is_foreign_under_production() {
        let _clock = lock_test_clock(None);
        let root = TempDir::new().expect("state root");
        let (chain, private_key, not_before, not_after, _) = state_fixture();
        let staging_service = McpEndpointTlsService::for_authorized_hostname(
            Arc::new(JournalRoot::open(root.path()).expect("staging root")),
            HOSTNAME.to_owned(),
            McpEndpointCertificateEnvironment::Staging,
            false,
        )
        .expect("staging service");
        staging_service
            .install_ordinary_certificate(chain, private_key, not_before, not_after)
            .expect("install staging state");

        let state_path = root.path().join("mcp-endpoint/tls/state.json");
        let before_bytes = fs::read(&state_path).expect("read staging state");

        let prod_service = McpEndpointTlsService::for_authorized_hostname(
            Arc::new(JournalRoot::open(root.path()).expect("production root")),
            HOSTNAME.to_owned(),
            McpEndpointCertificateEnvironment::Production,
            false,
        )
        .expect("production service opens foreign staging state cleanly");
        assert!(
            prod_service.resolver.ordinary.load_full().is_none(),
            "foreign staging state must not be served under production"
        );

        let after_bytes = fs::read(&state_path).expect("read state after prod open");
        assert_eq!(
            before_bytes, after_bytes,
            "foreign state file must not be rewritten on open or miss"
        );
        assert!(matches!(
            classify_stored_state(
                &after_bytes,
                HOSTNAME,
                McpEndpointCertificateEnvironment::Production
            ),
            Ok(StoredCertificateClassification::Foreign)
        ));
    }

    #[test]
    fn production_install_activates_and_reuses_under_production() {
        let _clock = lock_test_clock(None);
        let root = TempDir::new().expect("state root");
        let (chain, private_key, not_before, not_after, certificate) = state_fixture();
        let service = McpEndpointTlsService::for_authorized_hostname(
            Arc::new(JournalRoot::open(root.path()).expect("journal root")),
            HOSTNAME.to_owned(),
            McpEndpointCertificateEnvironment::Production,
            false,
        )
        .expect("empty production service");
        service
            .install_ordinary_certificate(chain, private_key, not_before, not_after)
            .expect("validated production state installs");

        let reloaded = McpEndpointTlsService::for_authorized_hostname(
            Arc::new(JournalRoot::open(root.path()).expect("reopened journal root")),
            HOSTNAME.to_owned(),
            McpEndpointCertificateEnvironment::Production,
            false,
        )
        .expect("reloaded valid production state");
        assert!(reloaded.resolver.ordinary.load_full().is_some());

        let mut client = ClientConnection::new(
            trusted_client_config(certificate, vec![HTTP11_ALPN.to_vec()]),
            ServerName::try_from(HOSTNAME.to_owned()).expect("fixture hostname"),
        )
        .expect("ordinary client");
        let mut server =
            ServerConnection::new(mcp_endpoint_server_config(&reloaded)).expect("reloaded server");
        complete_handshake(&mut client, &mut server)
            .expect("reloaded production certificate handshakes");

        let second_reload = McpEndpointTlsService::for_authorized_hostname(
            Arc::new(JournalRoot::open(root.path()).expect("second reopen")),
            HOSTNAME.to_owned(),
            McpEndpointCertificateEnvironment::Production,
            false,
        )
        .expect("second production reopen");
        assert!(second_reload.resolver.ordinary.load_full().is_some());
    }

    #[test]
    fn production_state_is_foreign_under_explicit_staging() {
        let _clock = lock_test_clock(None);
        let root = TempDir::new().expect("state root");
        let (chain, private_key, not_before, not_after, _) = state_fixture();
        let prod_service = McpEndpointTlsService::for_authorized_hostname(
            Arc::new(JournalRoot::open(root.path()).expect("prod root")),
            HOSTNAME.to_owned(),
            McpEndpointCertificateEnvironment::Production,
            false,
        )
        .expect("prod service");
        prod_service
            .install_ordinary_certificate(chain, private_key, not_before, not_after)
            .expect("install production state");

        let state_path = root.path().join("mcp-endpoint/tls/state.json");
        let before_bytes = fs::read(&state_path).expect("read production state");

        let staging_service = McpEndpointTlsService::for_authorized_hostname(
            Arc::new(JournalRoot::open(root.path()).expect("staging root")),
            HOSTNAME.to_owned(),
            McpEndpointCertificateEnvironment::Staging,
            false,
        )
        .expect("staging service opens foreign production state cleanly");
        assert!(
            staging_service.resolver.ordinary.load_full().is_none(),
            "foreign production state must not be served under staging"
        );

        let after_bytes = fs::read(&state_path).expect("read state after staging open");
        assert_eq!(before_bytes, after_bytes);
        assert!(matches!(
            classify_stored_state(
                &after_bytes,
                HOSTNAME,
                McpEndpointCertificateEnvironment::Staging
            ),
            Ok(StoredCertificateClassification::Foreign)
        ));
    }

    #[test]
    fn corrupt_state_is_integrity_failure_under_production() {
        let _clock = lock_test_clock(None);
        let root = TempDir::new().expect("state root");
        let service = McpEndpointTlsService::for_authorized_hostname(
            Arc::new(JournalRoot::open(root.path()).expect("journal root")),
            HOSTNAME.to_owned(),
            McpEndpointCertificateEnvironment::Production,
            false,
        )
        .expect("empty service");
        let store = service.store.as_ref().expect("state store");
        unix::persist_tls_state_bytes(&store.directory, b"{\"corrupt\": true}")
            .expect("persist corrupt state");

        assert!(matches!(
            McpEndpointTlsService::for_authorized_hostname(
                Arc::new(JournalRoot::open(root.path()).expect("reopened journal root")),
                HOSTNAME.to_owned(),
                McpEndpointCertificateEnvironment::Production,
                false,
            ),
            Err(McpEndpointTlsError::State)
        ));
    }

    #[test]
    fn hostname_mismatched_state_is_integrity_failure_under_production() {
        let _clock = lock_test_clock(None);
        let root = TempDir::new().expect("state root");
        let (chain, private_key, not_before, not_after, _) = state_fixture();
        let service = McpEndpointTlsService::for_authorized_hostname(
            Arc::new(JournalRoot::open(root.path()).expect("journal root")),
            HOSTNAME.to_owned(),
            McpEndpointCertificateEnvironment::Production,
            false,
        )
        .expect("empty service");
        let other_hostname = "cd56ef78.solstone.me";
        let state = StoredCertificateState {
            environment: "production".to_owned(),
            hostname: other_hostname.to_owned(),
            certificate_chain: chain
                .into_iter()
                .map(|value| base64::engine::general_purpose::STANDARD.encode(value))
                .collect(),
            private_key: base64::engine::general_purpose::STANDARD.encode(private_key),
            not_before,
            not_after,
        };
        let encoded = serde_json::to_vec(&state).expect("state JSON");
        let store = service.store.as_ref().expect("state store");
        unix::persist_tls_state_bytes(&store.directory, &encoded)
            .expect("persist mismatched state");

        assert!(matches!(
            McpEndpointTlsService::for_authorized_hostname(
                Arc::new(JournalRoot::open(root.path()).expect("reopened journal root")),
                HOSTNAME.to_owned(),
                McpEndpointCertificateEnvironment::Production,
                false,
            ),
            Err(McpEndpointTlsError::State)
        ));
    }

    #[test]
    fn tls_permissions_failure_is_integrity_failure_under_production() {
        let _clock = lock_test_clock(None);
        let root = TempDir::new().expect("state root");
        let (chain, private_key, not_before, not_after, _) = state_fixture();
        let service = McpEndpointTlsService::for_authorized_hostname(
            Arc::new(JournalRoot::open(root.path()).expect("journal root")),
            HOSTNAME.to_owned(),
            McpEndpointCertificateEnvironment::Production,
            false,
        )
        .expect("service");
        service
            .install_ordinary_certificate(chain, private_key, not_before, not_after)
            .expect("install state");

        let state_path = root.path().join("mcp-endpoint/tls/state.json");
        fs::set_permissions(&state_path, fs::Permissions::from_mode(0o644))
            .expect("chmod state.json to invalid 0o644");

        assert!(matches!(
            McpEndpointTlsService::for_authorized_hostname(
                Arc::new(JournalRoot::open(root.path()).expect("reopened journal root")),
                HOSTNAME.to_owned(),
                McpEndpointCertificateEnvironment::Production,
                false,
            ),
            Err(McpEndpointTlsError::State)
        ));
    }

    #[test]
    fn expired_foreign_state_is_not_served_and_yields_cache_miss_under_production() {
        let _clock = lock_test_clock(None);
        let root = TempDir::new().expect("state root");
        let (chain, private_key, not_before, not_after) = expired_state_fixture();
        let service = McpEndpointTlsService::for_authorized_hostname(
            Arc::new(JournalRoot::open(root.path()).expect("journal root")),
            HOSTNAME.to_owned(),
            McpEndpointCertificateEnvironment::Staging,
            false,
        )
        .expect("empty service");
        let state = StoredCertificateState {
            environment: "staging".to_owned(),
            hostname: HOSTNAME.to_owned(),
            certificate_chain: chain
                .into_iter()
                .map(|value| base64::engine::general_purpose::STANDARD.encode(value))
                .collect(),
            private_key: base64::engine::general_purpose::STANDARD.encode(private_key),
            not_before,
            not_after,
        };
        let encoded = serde_json::to_vec(&state).expect("state JSON");
        let store = service.store.as_ref().expect("state store");
        unix::persist_tls_state_bytes(&store.directory, &encoded)
            .expect("persist expired staging state");

        let prod_service = McpEndpointTlsService::for_authorized_hostname(
            Arc::new(JournalRoot::open(root.path()).expect("reopened journal root")),
            HOSTNAME.to_owned(),
            McpEndpointCertificateEnvironment::Production,
            false,
        )
        .expect("expired foreign state is structurally valid and opens cleanly");
        assert!(prod_service.resolver.ordinary.load_full().is_none());

        assert!(matches!(
            classify_stored_state(
                &encoded,
                HOSTNAME,
                McpEndpointCertificateEnvironment::Production
            ),
            Ok(StoredCertificateClassification::Foreign)
        ));
    }

    #[test]
    fn unknown_environment_string_is_integrity_failure() {
        let _clock = lock_test_clock(None);
        let root = TempDir::new().expect("state root");
        let (chain, private_key, not_before, not_after, _) = state_fixture();
        let service = McpEndpointTlsService::for_authorized_hostname(
            Arc::new(JournalRoot::open(root.path()).expect("journal root")),
            HOSTNAME.to_owned(),
            McpEndpointCertificateEnvironment::Production,
            false,
        )
        .expect("empty service");
        let state = StoredCertificateState {
            environment: "test".to_owned(),
            hostname: HOSTNAME.to_owned(),
            certificate_chain: chain
                .into_iter()
                .map(|value| base64::engine::general_purpose::STANDARD.encode(value))
                .collect(),
            private_key: base64::engine::general_purpose::STANDARD.encode(private_key),
            not_before,
            not_after,
        };
        let encoded = serde_json::to_vec(&state).expect("state JSON");
        let store = service.store.as_ref().expect("state store");
        unix::persist_tls_state_bytes(&store.directory, &encoded)
            .expect("persist unknown env state");

        assert!(matches!(
            McpEndpointTlsService::for_authorized_hostname(
                Arc::new(JournalRoot::open(root.path()).expect("reopened journal root")),
                HOSTNAME.to_owned(),
                McpEndpointCertificateEnvironment::Production,
                false,
            ),
            Err(McpEndpointTlsError::State)
        ));
    }

    #[test]
    fn challenge_certificate_has_exact_rfc8737_identifier_and_only_negotiates_acme() {
        let _clock = lock_test_clock(None);
        let service = McpEndpointTlsService::empty_for_test(HOSTNAME.to_owned());
        let digest = [0x5a; 32];
        let guard = service
            .install_acme_challenge(digest)
            .expect("challenge installs");
        let challenge = service
            .resolver
            .challenge
            .load_full()
            .expect("challenge active");
        let certificate = challenge.key.cert.first().expect("challenge leaf").clone();
        let (_, parsed) = parse_x509_certificate(certificate.as_ref()).expect("parse challenge");
        let names = parsed
            .subject_alternative_name()
            .expect("parse SAN")
            .expect("challenge SAN");
        assert_eq!(names.value.general_names.len(), 1);
        assert!(matches!(
            names.value.general_names.first(),
            Some(GeneralName::DNSName(name)) if *name == HOSTNAME
        ));
        let identifier = parsed
            .extensions()
            .iter()
            .find(|extension| extension.oid.to_id_string() == "1.3.6.1.5.5.7.1.31")
            .expect("acme identifier extension");
        assert!(identifier.critical);
        assert_eq!(identifier.value, der_octet_string(&digest));

        let mut client = ClientConnection::new(
            acme_test_client_config(vec![ACME_TLS_ALPN.to_vec()]),
            ServerName::try_from(HOSTNAME.to_owned()).expect("acme hostname"),
        )
        .expect("acme client");
        let mut server =
            ServerConnection::new(mcp_endpoint_server_config(&service)).expect("acme server");
        complete_handshake(&mut client, &mut server).expect("acme handshake");
        assert_eq!(client.alpn_protocol(), Some(ACME_TLS_ALPN));
        drop(guard);
    }

    #[test]
    fn ordinary_http11_handshake_succeeds_but_h2_only_fails() {
        let _clock = lock_test_clock(None);
        let service = McpEndpointTlsService::empty_for_test(HOSTNAME.to_owned());
        let (key, certificate) = fixture_key();
        service.install_ordinary_for_test(key);
        let server_config = mcp_endpoint_server_config(&service);

        let mut ordinary_client = ClientConnection::new(
            trusted_client_config(certificate.clone(), vec![HTTP11_ALPN.to_vec()]),
            ServerName::try_from(HOSTNAME.to_owned()).expect("fixture hostname"),
        )
        .expect("ordinary client");
        let mut ordinary_server =
            ServerConnection::new(Arc::clone(&server_config)).expect("ordinary server");
        complete_handshake(&mut ordinary_client, &mut ordinary_server).expect("http1 handshake");
        assert_eq!(ordinary_client.alpn_protocol(), Some(HTTP11_ALPN));

        let mut h2_client = ClientConnection::new(
            trusted_client_config(certificate, vec![b"h2".to_vec()]),
            ServerName::try_from(HOSTNAME.to_owned()).expect("h2 hostname"),
        )
        .expect("h2 client");
        let mut h2_server = ServerConnection::new(server_config).expect("h2 server");
        assert!(complete_handshake(&mut h2_client, &mut h2_server).is_err());
    }

    #[test]
    fn hot_swap_changes_the_per_handshake_key_without_rebuilding_the_resolver() {
        let _clock = lock_test_clock(None);
        let service = McpEndpointTlsService::empty_for_test(HOSTNAME.to_owned());
        service.install_ordinary_for_test(fixture_key().0);
        let first = service
            .resolver
            .ordinary
            .load_full()
            .expect("first active ordinary key");
        service.install_ordinary_for_test(fixture_key().0);
        let second = service
            .resolver
            .ordinary
            .load_full()
            .expect("second active ordinary key");
        assert!(!Arc::ptr_eq(&first, &second));
    }

    #[tokio::test]
    async fn publication_failure_records_error_and_backs_off() {
        let _clock = lock_test_clock(None);
        let root = TempDir::new().expect("state root");
        let journal_root = Arc::new(JournalRoot::open(root.path()).expect("journal root"));
        let service = McpEndpointTlsService::for_authorized_hostname(
            Arc::clone(&journal_root),
            HOSTNAME.to_owned(),
            McpEndpointCertificateEnvironment::Staging,
            false,
        )
        .expect("service");

        let (chain, private_key, not_before, not_after, _) = state_fixture();
        service
            .install_ordinary_certificate(chain, private_key, not_before, not_after)
            .expect("install initial cert");

        let store = service.store.as_ref().unwrap();
        let sched = crate::acme_lifecycle::RenewalScheduleRecord {
            environment: McpEndpointCertificateEnvironment::Staging,
            hostname: HOSTNAME.to_string(),
            generation: None,
            leaf_aki: "YWtp".to_string(),
            leaf_serial: "c2VyaWFs".to_string(),
            renew_at: 1_700_000_000,
            window_start: None,
            window_end: None,
            ari_retry_at: None,
            ari_unavailable_since: None,
            issue_retry_at: None,
            issue_backoff_seconds: None,
            order_url: None,
            order_replaces: None,
            omit_replaces: None,
            replaces_submitted: None,
        };

        // Make directory read-only so persist fails
        let tls_dir = root.path().join("mcp-endpoint/tls");
        fs::set_permissions(&tls_dir, fs::Permissions::from_mode(0o500)).unwrap();

        let persist_res =
            crate::acme_lifecycle::persist_schedule(&store.directory, &service, &sched);
        assert_eq!(persist_res, Err(McpEndpointTlsError::State));

        let err = service.last_publication_error.lock().unwrap().take();
        assert_eq!(err, Some(McpEndpointTlsError::State));

        // Restore permissions for clean tempdir drop
        fs::set_permissions(&tls_dir, fs::Permissions::from_mode(0o700)).unwrap();
    }
}

#[cfg(all(test, feature = "full-tests"))]
#[cfg(unix)]
mod full_tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use tempfile::TempDir;

    use rcgen::{KeyPair, PKCS_ECDSA_P256_SHA256};
    use rustls::pki_types::ServerName;
    use rustls::{ClientConnection, ServerConnection};
    use solstone_core_journal_io::journal_root::JournalRoot;

    use super::test_helpers::*;
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn hand_built_bytes_reload_and_preserve() {
        let _clock = lock_test_clock(None);
        let now = current_epoch_seconds();

        let root = TempDir::new().expect("root");
        let journal_root = Arc::new(JournalRoot::open(root.path()).expect("journal root"));

        // Call for_authorized_hostname once on empty journal to admit and create mcp-endpoint/tls
        let initial_service = McpEndpointTlsService::for_authorized_hostname(
            Arc::clone(&journal_root),
            HOSTNAME.to_owned(),
            McpEndpointCertificateEnvironment::Staging,
            false,
        )
        .expect("initial service");
        drop(initial_service);

        let tls_dir = root.path().join("mcp-endpoint/tls");
        let acc_keypair = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let raw_pkcs8 = acc_keypair.serialize_der();
        let account_file = tls_dir.join("account-staging.pk8");
        fs::write(&account_file, &raw_pkcs8).unwrap();
        fs::set_permissions(&account_file, fs::Permissions::from_mode(0o600)).unwrap();

        let (leaf_der, leaf_key, cert_der) = mint_test_leaf(now - 1000, now + 10_000);
        let state_json = format!(
            r#"{{"environment":"staging","hostname":"ab12cd34.solstone.me","certificate_chain":["{}"],"private_key":"{}","not_before":{},"not_after":{}}}"#,
            base64::engine::general_purpose::STANDARD.encode(&leaf_der),
            base64::engine::general_purpose::STANDARD.encode(&leaf_key),
            now - 1000,
            now + 10_000
        );
        let state_file = tls_dir.join("state.json");
        fs::write(&state_file, state_json).unwrap();
        fs::set_permissions(&state_file, fs::Permissions::from_mode(0o600)).unwrap();

        let service = McpEndpointTlsService::for_authorized_hostname(
            Arc::clone(&journal_root),
            HOSTNAME.to_owned(),
            McpEndpointCertificateEnvironment::Staging,
            false,
        )
        .expect("reopen");

        let mut client = ClientConnection::new(
            trusted_client_config(cert_der, vec![HTTP11_ALPN.to_vec()]),
            ServerName::try_from(HOSTNAME.to_owned()).expect("server name"),
        )
        .unwrap();
        let mut server = ServerConnection::new(mcp_endpoint_server_config(&service)).unwrap();
        complete_handshake(&mut client, &mut server).expect("handshake");

        let pk8_after = fs::read(&account_file).unwrap();
        assert_eq!(pk8_after, raw_pkcs8);

        // Separately, write a BYO account.uri and assert read_byo_account_uri returns those exact bytes
        let byo_dir = unix::open_byo_directory(&journal_root).unwrap();
        let acc_dir = unix::open_byo_account_directory(&byo_dir, HOSTNAME).unwrap();
        let uri_file = root
            .path()
            .join("mcp-endpoint/byo/accounts")
            .join(HOSTNAME)
            .join("account.uri");
        let raw_uri = "https://acme.example/byo-custom-uri";
        fs::write(&uri_file, raw_uri).unwrap();
        fs::set_permissions(&uri_file, fs::Permissions::from_mode(0o600)).unwrap();

        let read_uri = unix::read_byo_account_uri(&acc_dir).unwrap();
        assert_eq!(read_uri.as_deref(), Some(raw_uri));
    }

    fn new_order_has_replaces(body: &[u8]) -> Option<bool> {
        let jws: serde_json::Value = serde_json::from_slice(body).ok()?;
        let payload_b64 = jws.get("payload")?.as_str()?;
        let payload_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(payload_b64)
            .ok()?;
        let order_payload: serde_json::Value = serde_json::from_slice(&payload_bytes).ok()?;
        Some(order_payload.get("replaces").is_some())
    }

    #[tokio::test(start_paused = true)]
    async fn operated_lane_ari_renewal_timing_and_replacement() {
        let now = 1_700_000_000_i64;
        let _clock = lock_test_clock(Some(now));

        let root = TempDir::new().expect("state root");
        let journal_root = Arc::new(JournalRoot::open(root.path()).expect("journal root"));
        let service = McpEndpointTlsService::for_authorized_hostname(
            Arc::clone(&journal_root),
            HOSTNAME.to_owned(),
            McpEndpointCertificateEnvironment::Staging,
            false,
        )
        .expect("service");

        let (pred_der, pred_key, pred_cert) = mint_test_leaf(now - 1000, now + 10_000);
        service
            .install_ordinary_certificate(
                vec![pred_der.clone()],
                pred_key,
                now - 1000,
                now + 10_000,
            )
            .expect("install initial cert");

        let leaf_info =
            crate::acme_lifecycle::extract_leaf_cert_info(&pred_der).expect("leaf info");
        let expected_replaces = format!(
            "{}.{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&leaf_info.aki),
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&leaf_info.raw_serial)
        );

        let (succ_der, succ_key, succ_cert) = mint_test_leaf(now + 1200, now + 20_000);
        let new_cert_pem = pem_from_der(&succ_der);
        service.set_custom_order_key(succ_key);

        let service_ref = service.lifecycle_copy();
        let challenge_observed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let challenge_observed_clone = Arc::clone(&challenge_observed);
        let challenge_hook = Arc::new(move || {
            assert!(
                service_ref.resolver.challenge.load_full().is_some(),
                "challenge must be active during validation"
            );
            challenge_observed_clone.store(true, std::sync::atomic::Ordering::SeqCst);
        });

        let (mock_client, requests) = mock_acme_server(
            "https://acme.example/dir".to_string(),
            true,
            200,
            (now + 1000, now + 2000),
            new_cert_pem,
            Some(expected_replaces),
            Some(challenge_hook),
            None,
        );

        service.set_custom_http_client(mock_client);
        service.set_custom_directory_url("https://acme.example/dir".to_string());
        service.set_custom_picker(Arc::new(|start, _| start + 200)); // picks now + 1200

        let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
        let service_task = service.lifecycle_copy();
        let root_path = root.path().to_path_buf();
        let handle = tokio::spawn(async move {
            service_task
                .run_acme_renewal_with_owner_state(&root_path, &mut shutdown_rx)
                .await
        });

        // Advance 50ms to allow initial ARI check and schedule persistence
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        let schedule_path = root.path().join("mcp-endpoint/tls/renewal.json");
        assert!(schedule_path.exists(), "renewal.json was persisted");
        let sched_bytes = fs::read(&schedule_path).expect("read renewal.json");
        let sched: crate::acme_lifecycle::RenewalScheduleRecord =
            serde_json::from_slice(&sched_bytes).expect("parse schedule");
        assert_eq!(sched.renew_at, now + 1200);

        // Advance virtual time to a moment before the picked instant: now + 1199
        set_test_now_override(Some(now + 1199));
        tokio::time::advance(tokio::time::Duration::from_secs(1199)).await;
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // No new-order before the picked instant
        {
            let reqs = requests.lock().unwrap();
            assert!(!reqs.iter().any(|(_, uri, _)| uri.ends_with("/new-order")));
        }
        let active_before = service.resolver.ordinary.load_full().expect("active cert");
        assert_eq!(active_before.key.cert[0], pred_cert);

        // Advance virtual time to the picked instant: now + 1200
        set_test_now_override(Some(now + 1200));
        tokio::time::advance(tokio::time::Duration::from_secs(1)).await;

        for _ in 0..50 {
            let found = requests
                .lock()
                .unwrap()
                .iter()
                .any(|(_, uri, _)| uri.ends_with("/cert/1"));
            if found {
                break;
            }
            tokio::time::advance(tokio::time::Duration::from_millis(250)).await;
        }

        // Assert exactly one new-order whose payload still has replaces
        {
            let reqs = requests.lock().unwrap();
            let new_order_count = reqs
                .iter()
                .filter(|(_, uri, _)| uri.ends_with("/new-order"))
                .count();
            assert_eq!(new_order_count, 1, "expected exactly one new-order request");
            assert!(
                reqs.iter().any(|(_, uri, _)| uri.ends_with("/cert/1")),
                "cert was downloaded"
            );
        }
        assert!(
            challenge_observed.load(std::sync::atomic::Ordering::SeqCst),
            "challenge was observed during validation"
        );
        assert!(
            service.resolver.challenge.load_full().is_none(),
            "challenge is cleared after task completes"
        );

        // Verify new certificate is now active in ordinary slot
        let active = service.resolver.ordinary.load_full().expect("active cert");
        assert_eq!(active.key.cert[0], succ_cert);

        // Verify in-memory handshake
        let mut client = ClientConnection::new(
            acme_test_client_config(vec![HTTP11_ALPN.to_vec()]),
            ServerName::try_from(HOSTNAME.to_owned()).expect("fixture hostname"),
        )
        .expect("client");
        let mut server =
            ServerConnection::new(mcp_endpoint_server_config(&service)).expect("server");
        complete_handshake(&mut client, &mut server).expect("handshake succeeds");
        assert_eq!(client.alpn_protocol(), Some(HTTP11_ALPN));

        // Stop renewal
        shutdown_tx.send(true).unwrap();
        let res = handle.await.expect("join handle");
        assert!(res.is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn operated_lane_directory_without_ari_renews_at_backstop() {
        let now = 1_700_000_000_i64;
        let _clock = lock_test_clock(Some(now));

        let root = TempDir::new().expect("state root");
        let journal_root = Arc::new(JournalRoot::open(root.path()).expect("journal root"));
        let service = McpEndpointTlsService::for_authorized_hostname(
            Arc::clone(&journal_root),
            HOSTNAME.to_owned(),
            McpEndpointCertificateEnvironment::Staging,
            false,
        )
        .expect("service");

        // Lifetime 90_000s, not_before = now, not_after = now + 90_000
        // Backstop = not_after - lifetime / 3 = now + 60_000
        let (pred_der, pred_key, pred_cert) = mint_test_leaf(now, now + 90_000);
        service
            .install_ordinary_certificate(vec![pred_der], pred_key, now, now + 90_000)
            .expect("install cert");

        let (succ_der, succ_key, succ_cert) = mint_test_leaf(now + 60_000, now + 150_000);
        let new_cert_pem = pem_from_der(&succ_der);
        service.set_custom_order_key(succ_key);

        let (mock_client, requests) = mock_acme_server(
            "https://acme.example/dir".to_string(),
            false, // no ARI
            200,
            (0, 0),
            new_cert_pem,
            None, // no replaces
            None,
            None,
        );

        service.set_custom_http_client(mock_client);
        service.set_custom_directory_url("https://acme.example/dir".to_string());

        let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
        let service_task = service.lifecycle_copy();
        let root_path = root.path().to_path_buf();
        let handle = tokio::spawn(async move {
            service_task
                .run_acme_renewal_with_owner_state(&root_path, &mut shutdown_rx)
                .await
        });

        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        let schedule_path = root.path().join("mcp-endpoint/tls/renewal.json");
        assert!(schedule_path.exists());
        let sched_bytes = fs::read(&schedule_path).expect("read renewal.json");
        let sched: crate::acme_lifecycle::RenewalScheduleRecord =
            serde_json::from_slice(&sched_bytes).expect("parse schedule");
        assert_eq!(sched.renew_at, now + 60_000);

        // Before backstop: now + 59_999
        set_test_now_override(Some(now + 59_999));
        tokio::time::advance(tokio::time::Duration::from_secs(59_999)).await;
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        {
            let reqs = requests.lock().unwrap();
            assert!(!reqs.iter().any(|(_, uri, _)| uri.ends_with("/new-order")));
        }
        let active_before = service.resolver.ordinary.load_full().expect("active cert");
        assert_eq!(active_before.key.cert[0], pred_cert);

        // At backstop: now + 60_000
        set_test_now_override(Some(now + 60_000));
        tokio::time::advance(tokio::time::Duration::from_secs(1)).await;

        for _ in 0..50 {
            let found = requests
                .lock()
                .unwrap()
                .iter()
                .any(|(_, uri, _)| uri.ends_with("/cert/1"));
            if found {
                break;
            }
            tokio::time::advance(tokio::time::Duration::from_millis(250)).await;
        }

        let active = service.resolver.ordinary.load_full().expect("active cert");
        assert_eq!(active.key.cert[0], succ_cert);

        {
            let reqs = requests.lock().unwrap();
            let new_orders = reqs
                .iter()
                .filter(|(_, uri, _)| uri.ends_with("/new-order"))
                .count();
            assert_eq!(new_orders, 1);
        }

        shutdown_tx.send(true).unwrap();
        let res = handle.await.expect("join");
        assert!(res.is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn ari_503_temporary_failure_applies_retry_after() {
        let now = 1_700_000_000_i64;
        let _clock = lock_test_clock(Some(now));

        let root = TempDir::new().expect("state root");
        let journal_root = Arc::new(JournalRoot::open(root.path()).expect("journal root"));
        let service = McpEndpointTlsService::for_authorized_hostname(
            Arc::clone(&journal_root),
            HOSTNAME.to_owned(),
            McpEndpointCertificateEnvironment::Staging,
            false,
        )
        .expect("service");

        let (pred_der, pred_key, _) = mint_test_leaf(now - 1000, now + 10_000);
        service
            .install_ordinary_certificate(vec![pred_der], pred_key, now - 1000, now + 10_000)
            .expect("install initial cert");

        let (succ_der, _, _) = mint_test_leaf(now + 2000, now + 10_000);
        let new_cert_pem = pem_from_der(&succ_der);

        let (mock_client, _) = mock_acme_server(
            "https://acme.example/dir".to_string(),
            true,
            503, // first attempt fails with 503
            (now + 2000, now + 4000),
            new_cert_pem,
            None,
            None,
            None,
        );

        service.set_custom_http_client(mock_client);
        service.set_custom_directory_url("https://acme.example/dir".to_string());
        service.set_custom_picker(Arc::new(|start, _| start + 100));

        let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
        let service_task = service.lifecycle_copy();
        let root_path = root.path().to_path_buf();
        let handle = tokio::spawn(async move {
            service_task
                .run_acme_renewal_with_owner_state(&root_path, &mut shutdown_rx)
                .await
        });

        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        let schedule_path = root.path().join("mcp-endpoint/tls/renewal.json");
        assert!(schedule_path.exists());
        let sched_bytes = fs::read(&schedule_path).expect("read renewal.json");
        let sched: crate::acme_lifecycle::RenewalScheduleRecord =
            serde_json::from_slice(&sched_bytes).expect("parse schedule");
        assert_eq!(sched.ari_retry_at, Some(now + 120));

        // Advance 120s past 503 retry_after
        set_test_now_override(Some(now + 120));
        tokio::time::advance(tokio::time::Duration::from_secs(120)).await;
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        let sched_bytes2 = fs::read(&schedule_path).expect("read renewal.json after retry");
        let sched2: crate::acme_lifecycle::RenewalScheduleRecord =
            serde_json::from_slice(&sched_bytes2).expect("parse schedule");
        assert_eq!(sched2.renew_at, now + 2100);

        shutdown_tx.send(true).unwrap();
        let res = handle.await.expect("join");
        assert!(res.is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn renewal_json_reload_with_order_url_resumes() {
        let now = 1_700_000_000_i64;
        let _clock = lock_test_clock(Some(now));

        let root = TempDir::new().expect("state root");
        let journal_root = Arc::new(JournalRoot::open(root.path()).expect("journal root"));
        let service = McpEndpointTlsService::for_authorized_hostname(
            Arc::clone(&journal_root),
            HOSTNAME.to_owned(),
            McpEndpointCertificateEnvironment::Staging,
            false,
        )
        .expect("service");

        let (pred_der, pred_key, _) = mint_test_leaf(now - 1000, now + 10_000);
        service
            .install_ordinary_certificate(
                vec![pred_der.clone()],
                pred_key,
                now - 1000,
                now + 10_000,
            )
            .expect("install initial cert");

        let (succ_der, succ_key, succ_cert) = mint_test_leaf(now - 50, now + 10_000);
        let new_cert_pem = pem_from_der(&succ_der);
        service.set_custom_order_key(succ_key);

        let leaf_info =
            crate::acme_lifecycle::extract_leaf_cert_info(&pred_der).expect("leaf info");

        // Pre-create renewal.json with existing order_url
        let rec = crate::acme_lifecycle::RenewalScheduleRecord {
            environment: McpEndpointCertificateEnvironment::Staging,
            hostname: HOSTNAME.to_string(),
            generation: None,
            leaf_aki: base64::engine::general_purpose::STANDARD.encode(&leaf_info.aki),
            leaf_serial: base64::engine::general_purpose::STANDARD.encode(&leaf_info.raw_serial),
            renew_at: now - 50,
            window_start: Some(now - 100),
            window_end: Some(now + 100),
            ari_retry_at: None,
            ari_unavailable_since: None,
            issue_retry_at: None,
            issue_backoff_seconds: None,
            order_url: Some("https://acme.example/order/existing".to_string()),
            order_replaces: None,
            omit_replaces: None,
            replaces_submitted: None,
        };
        let sched_bytes = serde_json::to_vec(&rec).unwrap();
        let store = service.store.as_ref().unwrap();
        unix::persist_tls_renewal_bytes(&store.directory, &sched_bytes).unwrap();

        let (mock_client, requests) = mock_acme_server(
            "https://acme.example/dir".to_string(),
            true,
            200,
            (now + 1000, now + 2000),
            new_cert_pem,
            None,
            None,
            None,
        );

        service.set_custom_http_client(mock_client);
        service.set_custom_directory_url("https://acme.example/dir".to_string());

        let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
        let service_task = service.lifecycle_copy();
        let root_path = root.path().to_path_buf();
        let handle = tokio::spawn(async move {
            service_task
                .run_acme_renewal_with_owner_state(&root_path, &mut shutdown_rx)
                .await
        });

        for _ in 0..50 {
            let found = requests
                .lock()
                .unwrap()
                .iter()
                .any(|(_, uri, _)| uri.ends_with("/cert/1"));
            if found {
                break;
            }
            tokio::time::advance(tokio::time::Duration::from_millis(250)).await;
        }

        // Verify order/existing was polled directly and new-order was never called
        {
            let reqs = requests.lock().unwrap();
            assert!(
                reqs.iter()
                    .any(|(_, uri, _)| uri.contains("/order/existing"))
            );
            assert!(!reqs.iter().any(|(_, uri, _)| uri.ends_with("/new-order")));
        }

        let active = service.resolver.ordinary.load_full().expect("active cert");
        assert_eq!(active.key.cert[0], succ_cert);

        shutdown_tx.send(true).unwrap();
        let res = handle.await.expect("join");
        assert!(res.is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn byo_lane_renewal_with_account_key_and_uri() {
        let now = 1_700_000_000_i64;
        let _clock = lock_test_clock(Some(now));

        let root = TempDir::new().expect("byo root");
        let journal_root = JournalRoot::open(root.path()).expect("journal root");
        let byo_dir = unix::open_byo_directory(&journal_root).unwrap();
        let acc_dir = unix::open_byo_account_directory(&byo_dir, HOSTNAME).unwrap();
        let cert_dir = unix::open_byo_cert_directory(&byo_dir, HOSTNAME, 1).unwrap();

        let acc_keypair = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let account_pk8_path = root
            .path()
            .join("mcp-endpoint/byo/accounts")
            .join(HOSTNAME)
            .join("account.pk8");
        fs::write(&account_pk8_path, acc_keypair.serialize_der()).unwrap();
        fs::set_permissions(&account_pk8_path, fs::Permissions::from_mode(0o600)).unwrap();

        let account_uri_path = root
            .path()
            .join("mcp-endpoint/byo/accounts")
            .join(HOSTNAME)
            .join("account.uri");
        fs::write(&account_uri_path, "https://acme.example/account/byo-1").unwrap();
        fs::set_permissions(&account_uri_path, fs::Permissions::from_mode(0o600)).unwrap();

        let service =
            McpEndpointTlsService::for_byo_cert_directory(cert_dir, HOSTNAME.to_string(), 1)
                .expect("service");

        let (pred_der, pred_key, pred_cert) = mint_test_leaf(now - 1000, now + 10_000);
        service
            .install_ordinary_certificate(
                vec![pred_der.clone()],
                pred_key,
                now - 1000,
                now + 10_000,
            )
            .expect("install initial cert");

        let leaf_info =
            crate::acme_lifecycle::extract_leaf_cert_info(&pred_der).expect("leaf info");
        let expected_replaces = format!(
            "{}.{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&leaf_info.aki),
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&leaf_info.raw_serial)
        );

        let (succ_der, succ_key, succ_cert) = mint_test_leaf(now + 1200, now + 20_000);
        let new_cert_pem = pem_from_der(&succ_der);
        service.set_custom_order_key(succ_key);

        let (mock_client, requests) = mock_acme_server(
            "https://acme.example/dir".to_string(),
            true,
            200,
            (now + 1000, now + 2000),
            new_cert_pem,
            Some(expected_replaces),
            None,
            None,
        );

        service.set_custom_http_client(mock_client);
        service.set_custom_directory_url("https://acme.example/dir".to_string());
        service.set_custom_picker(Arc::new(|start, _| start + 200)); // renew_at = now + 1200

        let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
        let service_task = service.lifecycle_copy();
        let handle = tokio::spawn(async move {
            service_task
                .run_byo_acme_renewal(&acc_dir, &mut shutdown_rx)
                .await
        });

        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        let active = service.resolver.ordinary.load_full().expect("active cert");
        assert_eq!(active.key.cert[0], pred_cert);
        let sched_path = root
            .path()
            .join("mcp-endpoint/byo/certs")
            .join(HOSTNAME)
            .join("1/renewal.json");
        assert!(sched_path.exists());
        let sched_bytes = fs::read(&sched_path).expect("read renewal.json");
        let sched: crate::acme_lifecycle::RenewalScheduleRecord =
            serde_json::from_slice(&sched_bytes).expect("parse schedule");
        assert_eq!(sched.renew_at, now + 1200);

        // Advance to now + 1200
        set_test_now_override(Some(now + 1200));
        tokio::time::advance(tokio::time::Duration::from_secs(1200)).await;

        for _ in 0..50 {
            let found = requests
                .lock()
                .unwrap()
                .iter()
                .any(|(_, uri, _)| uri.ends_with("/cert/1"));
            if found {
                break;
            }
            tokio::time::advance(tokio::time::Duration::from_millis(250)).await;
        }

        let active_after = service
            .resolver
            .ordinary
            .load_full()
            .expect("active cert after renewal");
        assert_eq!(active_after.key.cert[0], succ_cert);

        shutdown_tx.send(true).unwrap();
        let res = handle.await.expect("join");
        assert!(res.is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn saved_window_later_next_ari_earlier_shifts_renew_at() {
        let now = 1_700_000_000_i64;
        let _clock = lock_test_clock(Some(now));

        let root = TempDir::new().expect("state root");
        let journal_root = Arc::new(JournalRoot::open(root.path()).expect("journal root"));
        let service = McpEndpointTlsService::for_authorized_hostname(
            Arc::clone(&journal_root),
            HOSTNAME.to_owned(),
            McpEndpointCertificateEnvironment::Staging,
            false,
        )
        .expect("service");

        let (pred_der, pred_key, pred_cert) = mint_test_leaf(now - 1000, now + 10_000);
        service
            .install_ordinary_certificate(
                vec![pred_der.clone()],
                pred_key,
                now - 1000,
                now + 10_000,
            )
            .expect("install initial cert");

        let leaf_info =
            crate::acme_lifecycle::extract_leaf_cert_info(&pred_der).expect("leaf info");
        let expected_replaces = format!(
            "{}.{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&leaf_info.aki),
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&leaf_info.raw_serial)
        );

        let (succ_der, succ_key, succ_cert) = mint_test_leaf(now + 600, now + 20_000);
        let new_cert_pem = pem_from_der(&succ_der);
        service.set_custom_order_key(succ_key);

        // Pre-create renewal.json with a later saved window: [now + 3000, now + 5000], renew_at = now + 4000
        // and ari_retry_at = Some(now + 100)
        let rec = crate::acme_lifecycle::RenewalScheduleRecord {
            environment: McpEndpointCertificateEnvironment::Staging,
            hostname: HOSTNAME.to_string(),
            generation: None,
            leaf_aki: base64::engine::general_purpose::STANDARD.encode(&leaf_info.aki),
            leaf_serial: base64::engine::general_purpose::STANDARD.encode(&leaf_info.raw_serial),
            renew_at: now + 4000,
            window_start: Some(now + 3000),
            window_end: Some(now + 5000),
            ari_retry_at: Some(now + 100),
            ari_unavailable_since: None,
            issue_retry_at: None,
            issue_backoff_seconds: None,
            order_url: None,
            order_replaces: None,
            omit_replaces: None,
            replaces_submitted: None,
        };
        let sched_bytes = serde_json::to_vec(&rec).unwrap();
        let store = service.store.as_ref().unwrap();
        unix::persist_tls_renewal_bytes(&store.directory, &sched_bytes).unwrap();

        // The next ARI response returns an earlier window: [now + 500, now + 1000]
        let (mock_client, requests) = mock_acme_server(
            "https://acme.example/dir".to_string(),
            true,
            200,
            (now + 500, now + 1000),
            new_cert_pem,
            Some(expected_replaces),
            None,
            None,
        );

        service.set_custom_http_client(mock_client);
        service.set_custom_directory_url("https://acme.example/dir".to_string());
        service.set_custom_picker(Arc::new(|start, _| start + 100)); // picks now + 600

        let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
        let service_task = service.lifecycle_copy();
        let root_path = root.path().to_path_buf();
        let handle = tokio::spawn(async move {
            service_task
                .run_acme_renewal_with_owner_state(&root_path, &mut shutdown_rx)
                .await
        });

        // Advance 100s to let it wake up, query ARI, and update schedule to earlier window
        set_test_now_override(Some(now + 100));
        tokio::time::advance(tokio::time::Duration::from_secs(100)).await;
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        let schedule_path = root.path().join("mcp-endpoint/tls/renewal.json");
        let sched_bytes2 = fs::read(&schedule_path).expect("read renewal.json after ari");
        let sched2: crate::acme_lifecycle::RenewalScheduleRecord =
            serde_json::from_slice(&sched_bytes2).expect("parse schedule");
        assert_eq!(sched2.renew_at, now + 600);

        // Advance to a moment before the new instant: now + 599
        set_test_now_override(Some(now + 599));
        tokio::time::advance(tokio::time::Duration::from_secs(499)).await;
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        let active = service.resolver.ordinary.load_full().expect("active cert");
        assert_eq!(active.key.cert[0], pred_cert);
        {
            let reqs = requests.lock().unwrap();
            assert!(!reqs.iter().any(|(_, uri, _)| uri.ends_with("/new-order")));
        }

        // Advance 1 second to now + 600
        set_test_now_override(Some(now + 600));
        tokio::time::advance(tokio::time::Duration::from_secs(1)).await;

        for _ in 0..50 {
            let found = requests
                .lock()
                .unwrap()
                .iter()
                .any(|(_, uri, _)| uri.ends_with("/cert/1"));
            if found {
                break;
            }
            tokio::time::advance(tokio::time::Duration::from_millis(250)).await;
        }

        let active_after = service.resolver.ordinary.load_full().expect("active cert");
        assert_eq!(active_after.key.cert[0], succ_cert);
        {
            let reqs = requests.lock().unwrap();
            assert!(reqs.iter().any(|(_, uri, _)| uri.ends_with("/new-order")));
        }

        shutdown_tx.send(true).unwrap();
        let res = handle.await.expect("join");
        assert!(res.is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn ari_window_later_than_backstop_waits_for_picked_instant() {
        let now = 1_700_000_000_i64;
        let _clock = lock_test_clock(Some(now));

        let root = TempDir::new().expect("state root");
        let journal_root = Arc::new(JournalRoot::open(root.path()).expect("journal root"));
        let service = McpEndpointTlsService::for_authorized_hostname(
            Arc::clone(&journal_root),
            HOSTNAME.to_owned(),
            McpEndpointCertificateEnvironment::Staging,
            false,
        )
        .expect("service");

        // Lifetime = 9000, not_after = now + 6000, not_before = now - 3000
        // Backstop = not_after - lifetime/3 = (now + 6000) - 3000 = now + 3000
        let not_before = now - 3000;
        let not_after = now + 6000;
        let (pred_der, pred_key, pred_cert) = mint_test_leaf(not_before, not_after);

        service
            .install_ordinary_certificate(vec![pred_der.clone()], pred_key, not_before, not_after)
            .expect("install initial cert");

        let leaf_info =
            crate::acme_lifecycle::extract_leaf_cert_info(&pred_der).expect("leaf info");
        let expected_replaces = format!(
            "{}.{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&leaf_info.aki),
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&leaf_info.raw_serial)
        );

        let (succ_der, succ_key, succ_cert) = mint_test_leaf(now + 4000, now + 20_000);
        let new_cert_pem = pem_from_der(&succ_der);
        service.set_custom_order_key(succ_key);

        // Suggested window is [now + 4000, now + 5000], which is later than backstop (now + 3000)
        let (mock_client, requests) = mock_acme_server(
            "https://acme.example/dir".to_string(),
            true,
            200,
            (now + 4000, now + 5000),
            new_cert_pem,
            Some(expected_replaces),
            None,
            None,
        );

        service.set_custom_http_client(mock_client);
        service.set_custom_directory_url("https://acme.example/dir".to_string());
        service.set_custom_picker(Arc::new(|start, _| start + 200)); // picks now + 4200

        let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
        let service_task = service.lifecycle_copy();
        let root_path = root.path().to_path_buf();
        let handle = tokio::spawn(async move {
            service_task
                .run_acme_renewal_with_owner_state(&root_path, &mut shutdown_rx)
                .await
        });

        // Let initial ARI exchange run and save schedule
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        let schedule_path = root.path().join("mcp-endpoint/tls/renewal.json");
        assert!(schedule_path.exists());
        let sched_bytes = fs::read(&schedule_path).expect("read renewal.json");
        let sched: crate::acme_lifecycle::RenewalScheduleRecord =
            serde_json::from_slice(&sched_bytes).expect("parse schedule");
        assert_eq!(sched.renew_at, now + 4200);

        // Advance only to the backstop: now + 3000
        set_test_now_override(Some(now + 3000));
        tokio::time::advance(tokio::time::Duration::from_secs(3000)).await;
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // Assert no new-order has been sent
        {
            let reqs = requests.lock().unwrap();
            assert!(!reqs.iter().any(|(_, uri, _)| uri.ends_with("/new-order")));
        }
        let active = service.resolver.ordinary.load_full().expect("active cert");
        assert_eq!(active.key.cert[0], pred_cert);

        // Advance to the picked instant: now + 4200
        set_test_now_override(Some(now + 4200));
        tokio::time::advance(tokio::time::Duration::from_secs(1200)).await;

        for _ in 0..50 {
            let found = requests
                .lock()
                .unwrap()
                .iter()
                .any(|(_, uri, _)| uri.ends_with("/cert/1"));
            if found {
                break;
            }
            tokio::time::advance(tokio::time::Duration::from_millis(250)).await;
        }

        // Assert exactly one new-order whose payload still has replaces
        {
            let reqs = requests.lock().unwrap();
            let new_order_count = reqs
                .iter()
                .filter(|(_, uri, _)| uri.ends_with("/new-order"))
                .count();
            assert_eq!(new_order_count, 1, "expected exactly one new-order request");
        }
        let active_after = service.resolver.ordinary.load_full().expect("active cert");
        assert_eq!(active_after.key.cert[0], succ_cert);

        shutdown_tx.send(true).unwrap();
        let res = handle.await.expect("join");
        assert!(res.is_ok());
    }

    #[tokio::test]
    async fn shutdown_cancels_renewal_loop_cleanly() {
        let _clock = lock_test_clock(None);
        let root = TempDir::new().expect("state root");
        let journal_root = Arc::new(JournalRoot::open(root.path()).expect("journal root"));
        let service = McpEndpointTlsService::for_authorized_hostname(
            Arc::clone(&journal_root),
            HOSTNAME.to_owned(),
            McpEndpointCertificateEnvironment::Staging,
            false,
        )
        .expect("service");

        let (_shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(true);
        let res = service.run_acme_renewal(&mut shutdown_rx).await;
        assert!(res.is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn lifecycle_driver_registers_on_its_directory_and_persists_issuance_before_activation() {
        let now = 1_700_000_000_i64;
        let _clock = lock_test_clock(Some(now));

        struct CustomHostGuard;
        impl Drop for CustomHostGuard {
            fn drop(&mut self) {
                crate::sme_account::set_custom_directory_host(None);
            }
        }
        let _guard = CustomHostGuard;
        crate::sme_account::set_custom_directory_host(Some("acme.example"));

        assert!(crate::sme_account::account_uri_has_expected_shape(
            "https://acme-v02.api.letsencrypt.org/acme/acct/1",
            true
        ));
        assert!(crate::sme_account::account_uri_has_expected_shape(
            "https://acme-staging-v02.api.letsencrypt.org/acme/acct/1",
            false
        ));
        assert!(!crate::sme_account::account_uri_has_expected_shape(
            "https://acme.example/acme/acct/7",
            true
        ));
        assert!(!crate::sme_account::account_uri_has_expected_shape(
            "https://acme.example/acme/acct/7",
            false
        ));

        let root = TempDir::new().expect("state root");
        let journal_root = Arc::new(JournalRoot::open(root.path()).expect("journal root"));
        let service = McpEndpointTlsService::for_authorized_hostname(
            Arc::clone(&journal_root),
            HOSTNAME.to_owned(),
            McpEndpointCertificateEnvironment::Staging,
            false,
        )
        .expect("service");

        let (pred_der, pred_key, pred_cert) = mint_test_leaf(now, now + 90_000);
        service
            .install_ordinary_certificate(vec![pred_der], pred_key, now, now + 90_000)
            .expect("install initial cert");

        set_test_now_override(Some(now + 60_000));
        tokio::time::advance(tokio::time::Duration::from_secs(60_000)).await;

        let (succ_der, succ_key, succ_cert) = mint_test_leaf(now + 60_000, now + 150_000);
        let new_cert_pem = pem_from_der(&succ_der);
        service.set_custom_order_key(succ_key);

        let hold_cert = Arc::new(tokio::sync::Notify::new());
        let gates = AcmeServerGates {
            hold_cert: Some(Arc::clone(&hold_cert)),
            ..Default::default()
        };

        let (mock_client, requests) = mock_acme_server_gated(
            "https://acme.example/dir".to_string(),
            false,
            200,
            (0, 0),
            new_cert_pem,
            None,
            None,
            Some("https://acme.example/acme/acct/7"),
            gates,
        );

        service.set_custom_http_client(mock_client);
        service.set_custom_directory_url("https://acme.example/dir".to_string());

        let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
        let service_task = service.lifecycle_copy();
        let root_path = root.path().to_path_buf();
        let handle = tokio::spawn(async move {
            service_task
                .run_acme_renewal_with_owner_state(&root_path, &mut shutdown_rx)
                .await
        });

        for _ in 0..50 {
            let found = requests
                .lock()
                .unwrap()
                .iter()
                .any(|(_, uri, _)| uri.ends_with("/cert/1"));
            if found {
                break;
            }
            tokio::time::advance(tokio::time::Duration::from_millis(50)).await;
        }

        let tls_dir = root.path().join("mcp-endpoint/tls");
        let issuance_path = tls_dir.join("issuance.json");
        assert!(issuance_path.exists());
        let issuance_bytes = fs::read(&issuance_path).expect("read issuance.json");
        let issuance_json: serde_json::Value =
            serde_json::from_slice(&issuance_bytes).expect("parse issuance.json");
        assert_eq!(
            issuance_json["order_url"].as_str(),
            Some("https://acme.example/order/1")
        );
        let saved_pkcs8 = issuance_json["private_key"].as_str().unwrap().to_string();

        let active_during = service.resolver.ordinary.load_full().expect("active cert");
        assert_eq!(active_during.key.cert[0], pred_cert);

        hold_cert.notify_one();

        for _ in 0..50 {
            tokio::task::yield_now().await;
            let current = service.resolver.ordinary.load_full().unwrap();
            if current.key.cert[0] == succ_cert {
                break;
            }
            tokio::time::advance(tokio::time::Duration::from_millis(50)).await;
        }

        let active_after = service.resolver.ordinary.load_full().expect("active cert");
        assert_eq!(active_after.key.cert[0], succ_cert);

        let state_bytes = fs::read(tls_dir.join("state.json")).expect("read state.json");
        let state_json: serde_json::Value =
            serde_json::from_slice(&state_bytes).expect("parse state.json");
        assert_eq!(
            state_json["private_key"].as_str().unwrap(),
            saved_pkcs8.as_str()
        );
        assert!(!issuance_path.exists());

        shutdown_tx.send(true).unwrap();
        let res = handle.await.expect("join");
        assert!(res.is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn lifecycle_driver_resumes_valid_order_after_restart() {
        let now = 1_700_000_000_i64;
        let _clock = lock_test_clock(Some(now));

        let root = TempDir::new().expect("state root");
        let journal_root = Arc::new(JournalRoot::open(root.path()).expect("journal root"));
        let service = McpEndpointTlsService::for_authorized_hostname(
            Arc::clone(&journal_root),
            HOSTNAME.to_owned(),
            McpEndpointCertificateEnvironment::Staging,
            false,
        )
        .expect("service");

        let (pred_der, pred_key, pred_cert) = mint_test_leaf(now, now + 90_000);
        service
            .install_ordinary_certificate(vec![pred_der], pred_key, now, now + 90_000)
            .expect("install initial cert");

        set_test_now_override(Some(now + 60_000));
        tokio::time::advance(tokio::time::Duration::from_secs(60_000)).await;

        let (succ_der, succ_key, succ_cert) = mint_test_leaf(now + 60_000, now + 150_000);
        let new_cert_pem = pem_from_der(&succ_der);
        service.set_custom_order_key(succ_key);

        let hold_cert = Arc::new(tokio::sync::Notify::new());
        let gates = AcmeServerGates {
            hold_cert: Some(Arc::clone(&hold_cert)),
            ..Default::default()
        };

        let (mock_client1, requests1) = mock_acme_server_gated(
            "https://acme.example/dir".to_string(),
            false,
            200,
            (0, 0),
            new_cert_pem.clone(),
            None,
            None,
            None,
            gates,
        );

        service.set_custom_http_client(mock_client1);
        service.set_custom_directory_url("https://acme.example/dir".to_string());

        let (shutdown1_tx, mut shutdown1_rx) = tokio::sync::watch::channel(false);
        let service_task1 = service.lifecycle_copy();
        let root_path = root.path().to_path_buf();
        let root_path1 = root_path.clone();
        let handle1 = tokio::spawn(async move {
            service_task1
                .run_acme_renewal_with_owner_state(&root_path1, &mut shutdown1_rx)
                .await
        });

        for _ in 0..50 {
            let found = requests1
                .lock()
                .unwrap()
                .iter()
                .any(|(_, uri, _)| uri.ends_with("/cert/1"));
            if found {
                break;
            }
            tokio::time::advance(tokio::time::Duration::from_millis(50)).await;
        }

        assert!(
            requests1
                .lock()
                .unwrap()
                .iter()
                .any(|(_, uri, _)| uri.ends_with("/cert/1"))
        );

        shutdown1_tx.send(true).unwrap();
        let res1 = handle1.await.expect("join 1");
        assert!(res1.is_ok());

        let tls_dir = root.path().join("mcp-endpoint/tls");
        let issuance_path = tls_dir.join("issuance.json");
        assert!(issuance_path.exists());
        let issuance_bytes = fs::read(&issuance_path).expect("read issuance.json");
        let issuance_json: serde_json::Value =
            serde_json::from_slice(&issuance_bytes).expect("parse issuance.json");
        let saved_pkcs8 = issuance_json["private_key"].as_str().unwrap().to_string();

        let sched_path = tls_dir.join("renewal.json");
        let sched_bytes = fs::read(&sched_path).expect("read renewal.json");
        let sched: crate::acme_lifecycle::RenewalScheduleRecord =
            serde_json::from_slice(&sched_bytes).expect("parse renewal.json");
        assert_eq!(
            sched.order_url,
            Some("https://acme.example/order/1".to_string())
        );

        let active1 = service.resolver.ordinary.load_full().expect("active cert");
        assert_eq!(active1.key.cert[0], pred_cert);

        // Second service on same directory
        let service2 = McpEndpointTlsService::for_authorized_hostname(
            Arc::clone(&journal_root),
            HOSTNAME.to_owned(),
            McpEndpointCertificateEnvironment::Staging,
            true,
        )
        .expect("service2");

        let active2_before = service2
            .resolver
            .ordinary
            .load_full()
            .expect("active cert 2");
        assert_eq!(active2_before.key.cert[0], pred_cert);

        let gates2 = AcmeServerGates {
            resume_already_valid: true,
            ..Default::default()
        };

        let (mock_client2, requests2) = mock_acme_server_gated(
            "https://acme.example/dir".to_string(),
            false,
            200,
            (0, 0),
            new_cert_pem,
            None,
            None,
            None,
            gates2,
        );

        service2.set_custom_http_client(mock_client2);
        service2.set_custom_directory_url("https://acme.example/dir".to_string());

        let (shutdown2_tx, mut shutdown2_rx) = tokio::sync::watch::channel(false);
        let service_task2 = service2.lifecycle_copy();
        let handle2 = tokio::spawn(async move {
            service_task2
                .run_acme_renewal_with_owner_state(&root_path, &mut shutdown2_rx)
                .await
        });

        for _ in 0..50 {
            tokio::task::yield_now().await;
            let current = service2.resolver.ordinary.load_full().unwrap();
            if current.key.cert[0] == succ_cert {
                break;
            }
            tokio::time::advance(tokio::time::Duration::from_millis(50)).await;
        }

        let active2_after = service2
            .resolver
            .ordinary
            .load_full()
            .expect("active cert 2");
        assert_eq!(active2_after.key.cert[0], succ_cert);

        let state_bytes2 = fs::read(tls_dir.join("state.json")).expect("read state.json 2");
        let state_json2: serde_json::Value =
            serde_json::from_slice(&state_bytes2).expect("parse state.json 2");
        assert_eq!(
            state_json2["private_key"].as_str().unwrap(),
            saved_pkcs8.as_str()
        );

        {
            let reqs = requests2.lock().unwrap();
            assert!(!reqs.iter().any(|(_, uri, _)| uri.ends_with("/new-order")));
            assert!(!reqs.iter().any(|(_, uri, _)| uri.ends_with("/finalize")));
        }

        shutdown2_tx.send(true).unwrap();
        let res2 = handle2.await.expect("join 2");
        assert!(res2.is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn authorization_fetch_shutdown_does_not_finalize() {
        let now = 1_700_000_000_i64;
        let _clock = lock_test_clock(Some(now));

        let root = TempDir::new().expect("state root");
        let journal_root = Arc::new(JournalRoot::open(root.path()).expect("journal root"));
        let service = McpEndpointTlsService::for_authorized_hostname(
            Arc::clone(&journal_root),
            HOSTNAME.to_owned(),
            McpEndpointCertificateEnvironment::Staging,
            false,
        )
        .expect("service");

        let (pred_der, pred_key, pred_cert) = mint_test_leaf(now, now + 90_000);
        service
            .install_ordinary_certificate(vec![pred_der], pred_key, now, now + 90_000)
            .expect("install initial cert");

        set_test_now_override(Some(now + 60_000));
        tokio::time::advance(tokio::time::Duration::from_secs(60_000)).await;

        let (succ_der, succ_key, _) = mint_test_leaf(now + 60_000, now + 150_000);
        let new_cert_pem = pem_from_der(&succ_der);
        service.set_custom_order_key(succ_key);

        let hold_authz = Arc::new(tokio::sync::Notify::new());
        let gates = AcmeServerGates {
            hold_authz: Some(Arc::clone(&hold_authz)),
            ..Default::default()
        };

        let (mock_client, requests) = mock_acme_server_gated(
            "https://acme.example/dir".to_string(),
            false,
            200,
            (0, 0),
            new_cert_pem,
            None,
            None,
            None,
            gates,
        );

        service.set_custom_http_client(mock_client);
        service.set_custom_directory_url("https://acme.example/dir".to_string());

        let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
        let service_task = service.lifecycle_copy();
        let root_path = root.path().to_path_buf();
        let handle = tokio::spawn(async move {
            service_task
                .run_acme_renewal_with_owner_state(&root_path, &mut shutdown_rx)
                .await
        });

        let mut authz_seen = false;
        for _ in 0..50 {
            if requests
                .lock()
                .unwrap()
                .iter()
                .any(|(_, uri, _)| uri == "https://acme.example/authz/1")
            {
                authz_seen = true;
                break;
            }
            tokio::time::advance(tokio::time::Duration::from_millis(50)).await;
        }
        assert!(authz_seen);

        shutdown_tx.send(true).unwrap();
        let res = handle.await.expect("join");
        assert!(res.is_ok());

        {
            let reqs = requests.lock().unwrap();
            assert!(!reqs.iter().any(|(_, uri, _)| uri.ends_with("/finalize")));
        }

        let active = service.resolver.ordinary.load_full().expect("active cert");
        assert_eq!(active.key.cert[0], pred_cert);
    }

    #[tokio::test(start_paused = true)]
    async fn unrecorded_issuance_does_not_order_and_keeps_the_leaf() {
        let now = 1_700_000_000_i64;
        let _clock = lock_test_clock(Some(now));

        let root = TempDir::new().expect("state root");
        let journal_root = Arc::new(JournalRoot::open(root.path()).expect("journal root"));
        let service = McpEndpointTlsService::for_authorized_hostname(
            Arc::clone(&journal_root),
            HOSTNAME.to_owned(),
            McpEndpointCertificateEnvironment::Staging,
            false,
        )
        .expect("service");

        let (pred_der, pred_key, pred_cert) = mint_test_leaf(now, now + 90_000);
        service
            .install_ordinary_certificate(vec![pred_der], pred_key, now, now + 90_000)
            .expect("install initial cert");

        set_test_now_override(Some(now + 60_000));
        tokio::time::advance(tokio::time::Duration::from_secs(60_000)).await;

        let (succ_der, succ_key, _) = mint_test_leaf(now + 60_000, now + 150_000);
        let new_cert_pem = pem_from_der(&succ_der);
        service.set_custom_order_key(succ_key);

        struct FaultGuard;
        impl Drop for FaultGuard {
            fn drop(&mut self) {
                crate::acme_lifecycle::FAIL_SCHEDULE_WRITE.set(false);
            }
        }
        let _guard = FaultGuard;
        // Inject at renewal publication, after the account/storage admission
        // boundary; changing directory permissions can refuse that boundary first.
        crate::acme_lifecycle::FAIL_SCHEDULE_WRITE.set(true);

        let (mock_client1, requests1) = mock_acme_server(
            "https://acme.example/dir".to_string(),
            false,
            200,
            (0, 0),
            new_cert_pem.clone(),
            None,
            None,
            None,
        );

        service.set_custom_http_client(mock_client1);
        service.set_custom_directory_url("https://acme.example/dir".to_string());

        let (shutdown1_tx, mut shutdown1_rx) = tokio::sync::watch::channel(false);
        let service_task1 = service.lifecycle_copy();
        let root_path = root.path().to_path_buf();
        let root_path1 = root_path.clone();
        let handle1 = tokio::spawn(async move {
            service_task1
                .run_acme_renewal_with_owner_state(&root_path1, &mut shutdown1_rx)
                .await
        });

        tokio::time::advance(tokio::time::Duration::from_secs(1)).await;
        for _ in 0..50 {
            if requests1
                .lock()
                .unwrap()
                .iter()
                .any(|(_, uri, _)| uri.contains("/dir"))
                || handle1.is_finished()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
        if handle1.is_finished() {
            panic!(
                "renewal task exited before publication fault: {:?}",
                handle1.await
            );
        }

        {
            let reqs = requests1.lock().unwrap();
            assert!(reqs.iter().any(|(_, uri, _)| uri.contains("/dir")));
            assert!(!reqs.iter().any(|(_, uri, _)| uri.ends_with("/new-order")));
        }
        let active1 = service
            .resolver
            .ordinary
            .load_full()
            .expect("active cert 1");
        assert_eq!(active1.key.cert[0], pred_cert);

        shutdown1_tx.send(true).unwrap();
        let res1 = handle1.await.expect("join 1");
        assert!(res1.is_ok());

        crate::acme_lifecycle::FAIL_SCHEDULE_WRITE.set(false);

        let gates2 = AcmeServerGates {
            before_new_order_response: Some(Arc::new(move || {
                crate::acme_lifecycle::FAIL_SCHEDULE_WRITE.set(true);
            })),
            ..Default::default()
        };

        let (mock_client2, requests2) = mock_acme_server_gated(
            "https://acme.example/dir".to_string(),
            false,
            200,
            (0, 0),
            new_cert_pem,
            None,
            None,
            None,
            gates2,
        );

        service.set_custom_http_client(mock_client2);

        let (shutdown2_tx, mut shutdown2_rx) = tokio::sync::watch::channel(false);
        let service_task2 = service.lifecycle_copy();
        let handle2 = tokio::spawn(async move {
            service_task2
                .run_acme_renewal_with_owner_state(&root_path, &mut shutdown2_rx)
                .await
        });

        tokio::time::advance(tokio::time::Duration::from_secs(1)).await;

        {
            let reqs = requests2.lock().unwrap();
            assert!(reqs.iter().any(|(_, uri, _)| uri.ends_with("/new-order")));
            assert!(!reqs.iter().any(|(_, uri, _)| uri.contains("/authz/")));
        }
        let active2 = service
            .resolver
            .ordinary
            .load_full()
            .expect("active cert 2");
        assert_eq!(active2.key.cert[0], pred_cert);

        crate::acme_lifecycle::FAIL_SCHEDULE_WRITE.set(false);

        shutdown2_tx.send(true).unwrap();
        let res2 = handle2.await.expect("join 2");
        assert!(res2.is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn already_replaced_does_not_repeat_replaces_across_restart() {
        let now = 1_700_000_000_i64;
        let _clock = lock_test_clock(Some(now));

        let root = TempDir::new().expect("state root");
        let journal_root = Arc::new(JournalRoot::open(root.path()).expect("journal root"));
        let service = McpEndpointTlsService::for_authorized_hostname(
            Arc::clone(&journal_root),
            HOSTNAME.to_owned(),
            McpEndpointCertificateEnvironment::Staging,
            false,
        )
        .expect("service");

        let (pred_der, pred_key, pred_cert) = mint_test_leaf(now - 1000, now + 10_000);
        service
            .install_ordinary_certificate(
                vec![pred_der.clone()],
                pred_key,
                now - 1000,
                now + 10_000,
            )
            .expect("install initial cert");

        let leaf_info =
            crate::acme_lifecycle::extract_leaf_cert_info(&pred_der).expect("leaf info");
        let expected_replaces = format!(
            "{}.{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&leaf_info.aki),
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&leaf_info.raw_serial)
        );

        let (succ_der, succ_key, _) = mint_test_leaf(now + 600, now + 20_000);
        let new_cert_pem = pem_from_der(&succ_der);
        service.set_custom_order_key(succ_key);

        let gates1 = AcmeServerGates {
            reject_replaces_once: Some(Arc::new(AtomicBool::new(true))),
            ..Default::default()
        };

        let (mock_client1, requests1) = mock_acme_server_gated(
            "https://acme.example/dir".to_string(),
            true,
            200,
            (now - 500, now - 100), // ARI window already behind now
            new_cert_pem.clone(),
            Some(expected_replaces.clone()),
            None,
            None,
            gates1,
        );

        service.set_custom_http_client(mock_client1);
        service.set_custom_directory_url("https://acme.example/dir".to_string());
        service.set_custom_picker(Arc::new(|start, _| start));

        let (shutdown1_tx, mut shutdown1_rx) = tokio::sync::watch::channel(false);
        let service_task1 = service.lifecycle_copy();
        let root_path = root.path().to_path_buf();
        let root_path1 = root_path.clone();
        let handle1 = tokio::spawn(async move {
            service_task1
                .run_acme_renewal_with_owner_state(&root_path1, &mut shutdown1_rx)
                .await
        });

        let tls_dir = root.path().join("mcp-endpoint/tls");
        let sched_path = tls_dir.join("renewal.json");
        for _ in 0..50 {
            tokio::task::yield_now().await;
            if let Ok(bytes) = fs::read(&sched_path)
                && let Ok(sched) =
                    serde_json::from_slice::<crate::acme_lifecycle::RenewalScheduleRecord>(&bytes)
                && sched.omit_replaces == Some(true)
                && sched.order_url.is_none()
            {
                break;
            }
            tokio::time::advance(tokio::time::Duration::from_millis(50)).await;
        }

        let sched_bytes1 = fs::read(&sched_path).expect("read renewal.json after first loop");
        let sched1: crate::acme_lifecycle::RenewalScheduleRecord =
            serde_json::from_slice(&sched_bytes1).expect("parse renewal.json after first loop");
        assert_eq!(sched1.omit_replaces, Some(true));
        assert!(sched1.order_url.is_none());

        let account_path = tls_dir.join("account-staging.pk8");
        let account_bytes_before = fs::read(&account_path).expect("read account pk8");

        // Shutdown during the backoff wait without advancing past issue_retry_at
        shutdown1_tx.send(true).unwrap();
        let res1 = handle1.await.expect("join 1");
        assert!(res1.is_ok());

        // Second service, same journal
        let service2 = McpEndpointTlsService::for_authorized_hostname(
            Arc::clone(&journal_root),
            HOSTNAME.to_owned(),
            McpEndpointCertificateEnvironment::Staging,
            false,
        )
        .expect("service2");

        // Advance frozen clock and tokio time by 30s so retry is due
        set_test_now_override(Some(now + 30));
        tokio::time::advance(tokio::time::Duration::from_secs(30)).await;

        let hold_authz2 = Arc::new(tokio::sync::Notify::new());
        let gates2 = AcmeServerGates {
            hold_authz: Some(Arc::clone(&hold_authz2)),
            ..Default::default()
        };

        let (mock_client2, requests2) = mock_acme_server_gated(
            "https://acme.example/dir".to_string(),
            true,
            200,
            (now - 500, now - 100),
            new_cert_pem,
            None,
            None,
            None,
            gates2,
        );

        service2.set_custom_http_client(mock_client2);
        service2.set_custom_directory_url("https://acme.example/dir".to_string());
        service2.set_custom_picker(Arc::new(|start, _| start));

        let (shutdown2_tx, mut shutdown2_rx) = tokio::sync::watch::channel(false);
        let service_task2 = service2.lifecycle_copy();
        let handle2 = tokio::spawn(async move {
            service_task2
                .run_acme_renewal_with_owner_state(&root_path, &mut shutdown2_rx)
                .await
        });

        for _ in 0..50 {
            let found = requests2
                .lock()
                .unwrap()
                .iter()
                .any(|(_, uri, _)| uri == "https://acme.example/authz/1");
            if found {
                break;
            }
            tokio::time::advance(tokio::time::Duration::from_millis(50)).await;
        }

        let mut req2_has_new_order_without_replaces = false;
        {
            let reqs2 = requests2.lock().unwrap();
            for (_, uri, body) in reqs2.iter() {
                if uri.ends_with("/new-order") && new_order_has_replaces(body) == Some(false) {
                    req2_has_new_order_without_replaces = true;
                }
            }
        }
        assert!(req2_has_new_order_without_replaces);

        let mut total_replaces = 0;
        for reqs_lock in [&requests1, &requests2] {
            let reqs = reqs_lock.lock().unwrap();
            for (_, uri, body) in reqs.iter() {
                if uri.ends_with("/new-order") && new_order_has_replaces(body) == Some(true) {
                    total_replaces += 1;
                }
            }
        }
        assert_eq!(total_replaces, 1);

        let account_bytes_after = fs::read(&account_path).expect("read account pk8 2");
        assert_eq!(account_bytes_before, account_bytes_after);

        let active2 = service2
            .resolver
            .ordinary
            .load_full()
            .expect("active cert 2");
        assert_eq!(active2.key.cert[0], pred_cert);

        shutdown2_tx.send(true).unwrap();
        let res2 = handle2.await.expect("join 2");
        assert!(res2.is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn resume_transport_error_keeps_order_url() {
        let now = 1_700_000_000_i64;
        let _clock = lock_test_clock(Some(now));

        let root = TempDir::new().expect("state root");
        let journal_root = Arc::new(JournalRoot::open(root.path()).expect("journal root"));
        let service = McpEndpointTlsService::for_authorized_hostname(
            Arc::clone(&journal_root),
            HOSTNAME.to_owned(),
            McpEndpointCertificateEnvironment::Staging,
            false,
        )
        .expect("service");

        let (pred_der, pred_key, _) = mint_test_leaf(now - 1000, now + 10_000);
        service
            .install_ordinary_certificate(
                vec![pred_der.clone()],
                pred_key,
                now - 1000,
                now + 10_000,
            )
            .expect("install initial cert");

        let leaf_info =
            crate::acme_lifecycle::extract_leaf_cert_info(&pred_der).expect("leaf info");

        let rec = crate::acme_lifecycle::RenewalScheduleRecord {
            environment: McpEndpointCertificateEnvironment::Staging,
            hostname: HOSTNAME.to_string(),
            generation: None,
            leaf_aki: base64::engine::general_purpose::STANDARD.encode(&leaf_info.aki),
            leaf_serial: base64::engine::general_purpose::STANDARD.encode(&leaf_info.raw_serial),
            renew_at: now - 50,
            window_start: Some(now - 100),
            window_end: Some(now + 100),
            ari_retry_at: None,
            ari_unavailable_since: None,
            issue_retry_at: None,
            issue_backoff_seconds: None,
            order_url: Some("https://acme.example/order/existing".to_string()),
            order_replaces: None,
            omit_replaces: None,
            replaces_submitted: None,
        };
        let sched_bytes = serde_json::to_vec(&rec).unwrap();
        let store = service.store.as_ref().unwrap();
        unix::persist_tls_renewal_bytes(&store.directory, &sched_bytes).unwrap();

        let gates1 = AcmeServerGates {
            resume_problem: Some((500, "urn:ietf:params:acme:error:serverInternal".to_string())),
            ..Default::default()
        };

        let (mock_client1, requests1) = mock_acme_server_gated(
            "https://acme.example/dir".to_string(),
            false,
            200,
            (0, 0),
            "".to_string(),
            None,
            None,
            None,
            gates1,
        );

        service.set_custom_http_client(mock_client1);
        service.set_custom_directory_url("https://acme.example/dir".to_string());

        let (shutdown1_tx, mut shutdown1_rx) = tokio::sync::watch::channel(false);
        let service_task1 = service.lifecycle_copy();
        let root_path = root.path().to_path_buf();
        let root_path1 = root_path.clone();
        let handle1 = tokio::spawn(async move {
            service_task1
                .run_acme_renewal_with_owner_state(&root_path1, &mut shutdown1_rx)
                .await
        });

        let sched_path = root.path().join("mcp-endpoint/tls/renewal.json");
        for _ in 0..50 {
            tokio::task::yield_now().await;
            if let Ok(bytes) = fs::read(&sched_path)
                && let Ok(sched) =
                    serde_json::from_slice::<crate::acme_lifecycle::RenewalScheduleRecord>(&bytes)
                && sched.issue_retry_at.is_some()
            {
                break;
            }
            tokio::time::advance(tokio::time::Duration::from_millis(50)).await;
        }

        let sched_bytes1 = fs::read(&sched_path).expect("read renewal.json after 500 loop");
        let sched1: crate::acme_lifecycle::RenewalScheduleRecord =
            serde_json::from_slice(&sched_bytes1).expect("parse renewal.json after 500 loop");
        assert_eq!(
            sched1.order_url,
            Some("https://acme.example/order/existing".to_string())
        );
        assert!(sched1.issue_retry_at.is_some() && sched1.issue_retry_at.unwrap() > now);

        {
            let reqs = requests1.lock().unwrap();
            assert!(!reqs.iter().any(|(_, uri, _)| uri.ends_with("/new-order")));
        }

        shutdown1_tx.send(true).unwrap();
        let res1 = handle1.await.expect("join 1");
        assert!(res1.is_ok());

        // Part 2: 404
        unix::persist_tls_renewal_bytes(&store.directory, &sched_bytes).unwrap();

        let gates2 = AcmeServerGates {
            resume_problem: Some((404, "urn:ietf:params:acme:error:notFound".to_string())),
            ..Default::default()
        };

        let (mock_client2, requests2) = mock_acme_server_gated(
            "https://acme.example/dir".to_string(),
            false,
            200,
            (0, 0),
            "".to_string(),
            None,
            None,
            None,
            gates2,
        );

        service.set_custom_http_client(mock_client2);

        let (shutdown2_tx, mut shutdown2_rx) = tokio::sync::watch::channel(false);
        let service_task2 = service.lifecycle_copy();
        let handle2 = tokio::spawn(async move {
            service_task2
                .run_acme_renewal_with_owner_state(&root_path, &mut shutdown2_rx)
                .await
        });

        for _ in 0..50 {
            tokio::task::yield_now().await;
            if let Ok(bytes) = fs::read(&sched_path)
                && let Ok(sched) =
                    serde_json::from_slice::<crate::acme_lifecycle::RenewalScheduleRecord>(&bytes)
                && sched.order_url.is_none()
                && sched.issue_retry_at.is_some()
            {
                break;
            }
            tokio::time::advance(tokio::time::Duration::from_millis(50)).await;
        }

        let sched_bytes2 = fs::read(&sched_path).expect("read renewal.json after 404 loop");
        let sched2: crate::acme_lifecycle::RenewalScheduleRecord =
            serde_json::from_slice(&sched_bytes2).expect("parse renewal.json after 404 loop");
        assert!(sched2.order_url.is_none());
        assert!(sched2.issue_retry_at.is_some() && sched2.issue_retry_at.unwrap() > now);

        {
            let reqs = requests2.lock().unwrap();
            assert!(!reqs.iter().any(|(_, uri, _)| uri.ends_with("/new-order")));
        }

        shutdown2_tx.send(true).unwrap();
        let res2 = handle2.await.expect("join 2");
        assert!(res2.is_ok());
    }
}
