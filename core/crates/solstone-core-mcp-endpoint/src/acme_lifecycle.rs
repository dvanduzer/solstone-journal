// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::fmt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axum::http;
use base64::Engine as _;
use instant_acme::{CertificateIdentifier, HttpClient, NewOrder, OrderStatus};
use rustls::pki_types::Der;
use serde::de::{self, Deserializer, MapAccess, Visitor};
use serde::{Deserialize, Serialize, Serializer};
use solstone_core_journal_config::McpEndpointCertificateEnvironment;
use tokio::sync::watch;
use x509_parser::parse_x509_certificate;

use crate::acme_http::{AriRetry, RawRetryAfterCapturingClient, parse_retry_after};
use crate::tls::{
    McpEndpointCertificateLifecycleError, McpEndpointTlsError, McpEndpointTlsService,
    current_epoch_seconds,
};
use crate::unix::{self, TlsStateDirectory};

/// Constant defining prolonged ARI failure threshold (7 days in seconds).
pub const ARI_PROLONGED_FAILURE: i64 = 7 * 86400;

/// Initial retry delay for failed issuance (30s).
pub const INITIAL_ISSUE_BACKOFF_SECONDS: u64 = 30;

/// Maximum retry delay for failed issuance (1 hour).
pub const MAX_ISSUE_BACKOFF_SECONDS: u64 = 3600;

/// Number of attempts before setting the issuance deadline on `:alreadyReplaced`.
pub const ALREADY_REPLACED_ATTEMPTS: u32 = 3;

/// Strict persistent renewal schedule record stored in `renewal.json`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenewalScheduleRecord {
    pub environment: McpEndpointCertificateEnvironment,
    pub hostname: String,
    pub generation: Option<u64>,
    pub leaf_aki: String,
    pub leaf_serial: String,
    pub renew_at: i64,
    pub window_start: Option<i64>,
    pub window_end: Option<i64>,
    pub ari_retry_at: Option<i64>,
    pub ari_unavailable_since: Option<i64>,
    pub issue_retry_at: Option<i64>,
    pub issue_backoff_seconds: Option<u64>,
    pub order_url: Option<String>,
}

impl RenewalScheduleRecord {
    /// Return whether this row matches the target environment, hostname, generation, and leaf id.
    pub fn matches(
        &self,
        environment: McpEndpointCertificateEnvironment,
        hostname: &str,
        generation: Option<u64>,
        leaf_aki: &[u8],
        leaf_serial: &[u8],
    ) -> bool {
        self.environment == environment
            && self.hostname == hostname
            && self.generation == generation
            && self.leaf_aki == base64::engine::general_purpose::STANDARD.encode(leaf_aki)
            && self.leaf_serial == base64::engine::general_purpose::STANDARD.encode(leaf_serial)
    }

    /// Return whether this row matches a state where no leaf certificate is installed.
    pub fn matches_no_leaf(
        &self,
        environment: McpEndpointCertificateEnvironment,
        hostname: &str,
        generation: Option<u64>,
    ) -> bool {
        self.environment == environment
            && self.hostname == hostname
            && self.generation == generation
            && self.leaf_aki.is_empty()
            && self.leaf_serial.is_empty()
    }
}

impl Serialize for RenewalScheduleRecord {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        use serde::ser::SerializeMap;

        let mut map = serializer.serialize_map(None)?;
        let env_str = match self.environment {
            McpEndpointCertificateEnvironment::Staging => "staging",
            McpEndpointCertificateEnvironment::Production => "production",
        };
        map.serialize_entry("environment", env_str)?;
        map.serialize_entry("hostname", &self.hostname)?;
        if let Some(generation) = self.generation {
            map.serialize_entry("generation", &generation)?;
        }
        map.serialize_entry("leaf_aki", &self.leaf_aki)?;
        map.serialize_entry("leaf_serial", &self.leaf_serial)?;
        map.serialize_entry("renew_at", &self.renew_at)?;
        map.serialize_entry("window_start", &self.window_start)?;
        map.serialize_entry("window_end", &self.window_end)?;
        map.serialize_entry("ari_retry_at", &self.ari_retry_at)?;
        map.serialize_entry("ari_unavailable_since", &self.ari_unavailable_since)?;
        map.serialize_entry("issue_retry_at", &self.issue_retry_at)?;
        map.serialize_entry("issue_backoff_seconds", &self.issue_backoff_seconds)?;
        map.serialize_entry("order_url", &self.order_url)?;
        map.end()
    }
}

const RENEWAL_FIELDS: &[&str] = &[
    "environment",
    "hostname",
    "generation",
    "leaf_aki",
    "leaf_serial",
    "renew_at",
    "window_start",
    "window_end",
    "ari_retry_at",
    "ari_unavailable_since",
    "issue_retry_at",
    "issue_backoff_seconds",
    "order_url",
];

fn assign_schedule_field<'de, A, T>(
    slot: &mut Option<T>,
    _field: &str,
    map: &mut A,
) -> Result<(), A::Error>
where
    A: MapAccess<'de>,
    T: Deserialize<'de>,
{
    if slot.is_some() {
        return Err(de::Error::custom("duplicate renewal schedule field"));
    }
    *slot = Some(map.next_value()?);
    Ok(())
}

impl<'de> Deserialize<'de> for RenewalScheduleRecord {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ScheduleVisitor;

        impl<'de> Visitor<'de> for ScheduleVisitor {
            type Value = RenewalScheduleRecord;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a strict renewal schedule object")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut environment: Option<String> = None;
                let mut hostname: Option<String> = None;
                let mut generation: Option<Option<u64>> = None;
                let mut leaf_aki: Option<String> = None;
                let mut leaf_serial: Option<String> = None;
                let mut renew_at: Option<i64> = None;
                let mut window_start: Option<Option<i64>> = None;
                let mut window_end: Option<Option<i64>> = None;
                let mut ari_retry_at: Option<Option<i64>> = None;
                let mut ari_unavailable_since: Option<Option<i64>> = None;
                let mut issue_retry_at: Option<Option<i64>> = None;
                let mut issue_backoff_seconds: Option<Option<u64>> = None;
                let mut order_url: Option<Option<String>> = None;

                while let Some(field) = map.next_key::<String>()? {
                    match field.as_str() {
                        "environment" => assign_schedule_field(&mut environment, &field, &mut map)?,
                        "hostname" => assign_schedule_field(&mut hostname, &field, &mut map)?,
                        "generation" => assign_schedule_field(&mut generation, &field, &mut map)?,
                        "leaf_aki" => assign_schedule_field(&mut leaf_aki, &field, &mut map)?,
                        "leaf_serial" => assign_schedule_field(&mut leaf_serial, &field, &mut map)?,
                        "renew_at" => assign_schedule_field(&mut renew_at, &field, &mut map)?,
                        "window_start" => {
                            assign_schedule_field(&mut window_start, &field, &mut map)?
                        }
                        "window_end" => assign_schedule_field(&mut window_end, &field, &mut map)?,
                        "ari_retry_at" => {
                            assign_schedule_field(&mut ari_retry_at, &field, &mut map)?
                        }
                        "ari_unavailable_since" => {
                            assign_schedule_field(&mut ari_unavailable_since, &field, &mut map)?
                        }
                        "issue_retry_at" => {
                            assign_schedule_field(&mut issue_retry_at, &field, &mut map)?
                        }
                        "issue_backoff_seconds" => {
                            assign_schedule_field(&mut issue_backoff_seconds, &field, &mut map)?
                        }
                        "order_url" => assign_schedule_field(&mut order_url, &field, &mut map)?,
                        _ => return Err(de::Error::unknown_field(&field, RENEWAL_FIELDS)),
                    }
                }

                let env_str = environment.ok_or_else(|| de::Error::missing_field("environment"))?;
                let env = match env_str.as_str() {
                    "staging" => McpEndpointCertificateEnvironment::Staging,
                    "production" => McpEndpointCertificateEnvironment::Production,
                    _ => return Err(de::Error::custom("unknown certificate environment")),
                };

                Ok(RenewalScheduleRecord {
                    environment: env,
                    hostname: hostname.ok_or_else(|| de::Error::missing_field("hostname"))?,
                    generation: generation.flatten(),
                    leaf_aki: leaf_aki.ok_or_else(|| de::Error::missing_field("leaf_aki"))?,
                    leaf_serial: leaf_serial
                        .ok_or_else(|| de::Error::missing_field("leaf_serial"))?,
                    renew_at: renew_at.ok_or_else(|| de::Error::missing_field("renew_at"))?,
                    window_start: window_start.flatten(),
                    window_end: window_end.flatten(),
                    ari_retry_at: ari_retry_at.flatten(),
                    ari_unavailable_since: ari_unavailable_since.flatten(),
                    issue_retry_at: issue_retry_at.flatten(),
                    issue_backoff_seconds: issue_backoff_seconds.flatten(),
                    order_url: order_url.flatten(),
                })
            }
        }

        deserializer.deserialize_map(ScheduleVisitor)
    }
}

/// Extracted leaf certificate metadata needed for renewal decisions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeafCertInfo {
    pub not_before: i64,
    pub not_after: i64,
    pub aki: Vec<u8>,
    pub raw_serial: Vec<u8>,
}

/// Extract leaf certificate metadata from DER bytes.
pub fn extract_leaf_cert_info(der: &[u8]) -> Result<LeafCertInfo, String> {
    let (_, parsed_cert) =
        parse_x509_certificate(der).map_err(|e| format!("failed to parse certificate: {e}"))?;
    let not_before = parsed_cert.validity().not_before.timestamp();
    let not_after = parsed_cert.validity().not_after.timestamp();
    let aki = parsed_cert
        .iter_extensions()
        .find_map(|ext| match ext.parsed_extension() {
            x509_parser::extensions::ParsedExtension::AuthorityKeyIdentifier(aki_ext) => {
                aki_ext.key_identifier.as_ref().map(|k| k.0.to_vec())
            }
            _ => None,
        })
        .ok_or_else(|| "certificate missing AuthorityKeyIdentifier extension".to_string())?;
    let raw_serial = parsed_cert.tbs_certificate.raw_serial().to_vec();
    Ok(LeafCertInfo {
        not_before,
        not_after,
        aki,
        raw_serial,
    })
}

/// Construct an owned [`CertificateIdentifier`] from [`LeafCertInfo`].
pub fn cert_id_from_leaf(leaf: &LeafCertInfo) -> CertificateIdentifier<'static> {
    CertificateIdentifier::new(
        Der::from_slice(&leaf.aki),
        Der::from_slice(&leaf.raw_serial),
    )
    .into_owned()
}

/// Outcome of attempting to query ARI renewal information.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AriOutcome {
    None,
    Success {
        window_start: i64,
        window_end: i64,
        retry_after: Option<AriRetry>,
    },
    TemporaryFailure {
        retry_after: AriRetry,
    },
}

/// Decision output by the pure decision engine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RenewalAction {
    Wait {
        until_epoch_seconds: i64,
        schedule: Option<RenewalScheduleRecord>,
    },
    QueryAri {
        cert_id: CertificateIdentifier<'static>,
        schedule: Option<RenewalScheduleRecord>,
    },
    IssueNow {
        replaces: Option<CertificateIdentifier<'static>>,
        schedule: Option<RenewalScheduleRecord>,
    },
    ResumeOrder {
        order_url: String,
        schedule: Option<RenewalScheduleRecord>,
    },
}

/// Default system random picker for choosing an instant in `[start, end)`.
pub fn system_random_picker(start: i64, end: i64) -> i64 {
    if start >= end {
        return start;
    }
    let span = (end - start) as u64;
    let mut bytes = [0u8; 8];
    let rng = ring::rand::SystemRandom::new();
    if ring::rand::SecureRandom::fill(&rng, &mut bytes).is_ok() {
        let val = u64::from_le_bytes(bytes);
        start + (val % span) as i64
    } else {
        start + (span / 2) as i64
    }
}

/// Pure decision engine for ACME certificate renewal.
#[allow(clippy::too_many_arguments)]
pub fn decide_renewal_action(
    now: i64,
    leaf: Option<&LeafCertInfo>,
    saved: Option<&RenewalScheduleRecord>,
    environment: McpEndpointCertificateEnvironment,
    hostname: &str,
    generation: Option<u64>,
    directory_has_renewal_info: bool,
    ari_outcome: &AriOutcome,
    force_staging_renewal: bool,
    picker: &dyn Fn(i64, i64) -> i64,
) -> RenewalAction {
    // 1. Mismatch check: discard saved row if it does not match current state.
    let saved = match (saved, leaf) {
        (Some(s), Some(l))
            if s.matches(environment, hostname, generation, &l.aki, &l.raw_serial) =>
        {
            Some(s)
        }
        (Some(s), None) if s.matches_no_leaf(environment, hostname, generation) => Some(s),
        _ => None,
    };

    // Helper: is issuance/ordering currently blocked by `issue_retry_at`?
    let issue_blocked = saved.and_then(|s| s.issue_retry_at).filter(|&t| now < t);

    // 2. Force staging renewal latch: issues once on start without waiting.
    if force_staging_renewal {
        if let Some(blocked_until) = issue_blocked {
            return RenewalAction::Wait {
                until_epoch_seconds: blocked_until,
                schedule: saved.cloned(),
            };
        }
        let replaces = if directory_has_renewal_info {
            leaf.map(cert_id_from_leaf)
        } else {
            None
        };
        return RenewalAction::IssueNow {
            replaces,
            schedule: saved.cloned(),
        };
    }

    // 3. Stored order URL resumes without creating a new order.
    if let Some(order_url) = saved.and_then(|s| s.order_url.as_ref()) {
        if let Some(blocked_until) = issue_blocked {
            return RenewalAction::Wait {
                until_epoch_seconds: blocked_until,
                schedule: saved.cloned(),
            };
        }
        return RenewalAction::ResumeOrder {
            order_url: order_url.clone(),
            schedule: saved.cloned(),
        };
    }

    // 4. No leaf certificate exists: issue now without replaces.
    let Some(l) = leaf else {
        if let Some(blocked_until) = issue_blocked {
            return RenewalAction::Wait {
                until_epoch_seconds: blocked_until,
                schedule: saved.cloned(),
            };
        }
        let sched = saved.cloned().unwrap_or_else(|| RenewalScheduleRecord {
            environment,
            hostname: hostname.to_owned(),
            generation,
            leaf_aki: String::new(),
            leaf_serial: String::new(),
            renew_at: now,
            window_start: None,
            window_end: None,
            ari_retry_at: None,
            ari_unavailable_since: None,
            issue_retry_at: None,
            issue_backoff_seconds: None,
            order_url: None,
        });
        return RenewalAction::IssueNow {
            replaces: None,
            schedule: Some(sched),
        };
    };

    // 5. Expired leaf: due now without ARI GET.
    if now >= l.not_after {
        if let Some(blocked_until) = issue_blocked {
            return RenewalAction::Wait {
                until_epoch_seconds: blocked_until,
                schedule: saved.cloned(),
            };
        }
        let replaces = if directory_has_renewal_info {
            Some(cert_id_from_leaf(l))
        } else {
            None
        };
        return RenewalAction::IssueNow {
            replaces,
            schedule: saved.cloned(),
        };
    }

    // 6. Valid leaf certificate: compute backstop.
    let lifetime = (l.not_after - l.not_before).max(0);
    let backstop = l.not_after - lifetime / 3;
    let cert_id = cert_id_from_leaf(l);

    // 7. Directory has no renewalInfo: backstop governs renewal.
    let no_ari_schedule = RenewalScheduleRecord {
        environment,
        hostname: hostname.to_owned(),
        generation,
        leaf_aki: base64::engine::general_purpose::STANDARD.encode(&l.aki),
        leaf_serial: base64::engine::general_purpose::STANDARD.encode(&l.raw_serial),
        renew_at: saved.map(|s| s.renew_at).unwrap_or(backstop),
        window_start: None,
        window_end: None,
        ari_retry_at: None,
        ari_unavailable_since: None,
        issue_retry_at: saved.and_then(|s| s.issue_retry_at),
        issue_backoff_seconds: saved.and_then(|s| s.issue_backoff_seconds),
        order_url: None,
    };

    if !directory_has_renewal_info {
        if now >= backstop {
            if let Some(blocked_until) = issue_blocked {
                return RenewalAction::Wait {
                    until_epoch_seconds: blocked_until,
                    schedule: Some(saved.cloned().unwrap_or(no_ari_schedule)),
                };
            }
            return RenewalAction::IssueNow {
                replaces: None,
                schedule: Some(saved.cloned().unwrap_or(no_ari_schedule)),
            };
        }
        return RenewalAction::Wait {
            until_epoch_seconds: backstop,
            schedule: Some(saved.cloned().unwrap_or(no_ari_schedule)),
        };
    }

    // 8. Directory has renewalInfo: full ARI lifecycle.
    let base_schedule = RenewalScheduleRecord {
        environment,
        hostname: hostname.to_owned(),
        generation,
        leaf_aki: base64::engine::general_purpose::STANDARD.encode(&l.aki),
        leaf_serial: base64::engine::general_purpose::STANDARD.encode(&l.raw_serial),
        renew_at: saved.map(|s| s.renew_at).unwrap_or(backstop),
        window_start: saved.and_then(|s| s.window_start),
        window_end: saved.and_then(|s| s.window_end),
        ari_retry_at: saved.and_then(|s| s.ari_retry_at),
        ari_unavailable_since: saved.and_then(|s| s.ari_unavailable_since),
        issue_retry_at: saved.and_then(|s| s.issue_retry_at),
        issue_backoff_seconds: saved.and_then(|s| s.issue_backoff_seconds),
        order_url: None,
    };

    match ari_outcome {
        AriOutcome::Success {
            window_start,
            window_end,
            retry_after,
        } if *window_start < *window_end => {
            let renew_at = match saved {
                Some(s)
                    if s.window_start == Some(*window_start)
                        && s.window_end == Some(*window_end) =>
                {
                    s.renew_at
                }
                _ => picker(*window_start, *window_end),
            };

            let mut sched = base_schedule;
            sched.window_start = Some(*window_start);
            sched.window_end = Some(*window_end);
            sched.renew_at = renew_at;
            sched.ari_retry_at = retry_after.map(|r| r.retry_at);
            sched.ari_unavailable_since = None;

            if now >= renew_at {
                if let Some(blocked_until) = issue_blocked {
                    return RenewalAction::Wait {
                        until_epoch_seconds: blocked_until,
                        schedule: Some(sched),
                    };
                }
                return RenewalAction::IssueNow {
                    replaces: Some(cert_id),
                    schedule: Some(sched),
                };
            }

            let wake = match sched.ari_retry_at {
                Some(ari_t) if ari_t < renew_at => ari_t,
                _ => renew_at,
            };
            RenewalAction::Wait {
                until_epoch_seconds: wake,
                schedule: Some(sched),
            }
        }
        AriOutcome::Success {
            window_start: _,
            window_end: _,
            retry_after,
        } => {
            let fallback_retry = retry_after.unwrap_or_else(|| parse_retry_after(None, now));
            handle_temporary_failure(
                now,
                backstop,
                cert_id,
                base_schedule,
                saved,
                issue_blocked,
                fallback_retry,
            )
        }
        AriOutcome::TemporaryFailure { retry_after } => handle_temporary_failure(
            now,
            backstop,
            cert_id,
            base_schedule,
            saved,
            issue_blocked,
            *retry_after,
        ),
        AriOutcome::None => {
            if let Some(s) = saved {
                if s.window_start.is_some() && now >= s.renew_at {
                    if let Some(blocked_until) = issue_blocked {
                        return RenewalAction::Wait {
                            until_epoch_seconds: blocked_until,
                            schedule: Some(s.clone()),
                        };
                    }
                    return RenewalAction::IssueNow {
                        replaces: Some(cert_id),
                        schedule: Some(s.clone()),
                    };
                }

                if let Some(since) = s.ari_unavailable_since
                    && now >= since + ARI_PROLONGED_FAILURE
                    && now >= backstop
                {
                    if let Some(blocked_until) = issue_blocked {
                        return RenewalAction::Wait {
                            until_epoch_seconds: blocked_until,
                            schedule: Some(s.clone()),
                        };
                    }
                    return RenewalAction::IssueNow {
                        replaces: Some(cert_id),
                        schedule: Some(s.clone()),
                    };
                }

                if let Some(ari_t) = s.ari_retry_at
                    && now < ari_t
                {
                    let wait_until = s
                        .window_start
                        .map(|_| s.renew_at.min(ari_t))
                        .unwrap_or(ari_t.min(backstop));
                    return RenewalAction::Wait {
                        until_epoch_seconds: wait_until,
                        schedule: Some(s.clone()),
                    };
                }
            }
            RenewalAction::QueryAri {
                cert_id,
                schedule: saved.cloned(),
            }
        }
    }
}

fn handle_temporary_failure(
    now: i64,
    backstop: i64,
    cert_id: CertificateIdentifier<'static>,
    base_schedule: RenewalScheduleRecord,
    _saved: Option<&RenewalScheduleRecord>,
    issue_blocked: Option<i64>,
    retry_after: AriRetry,
) -> RenewalAction {
    let mut sched = base_schedule;
    sched.ari_unavailable_since = sched.ari_unavailable_since.or(Some(now));
    sched.ari_retry_at = Some(retry_after.retry_at);

    // 1. Prolonged failure check (7 days + backstop due)
    let prolonged = sched
        .ari_unavailable_since
        .is_some_and(|since| now >= since + ARI_PROLONGED_FAILURE);
    if prolonged && now >= backstop {
        if let Some(blocked_until) = issue_blocked {
            return RenewalAction::Wait {
                until_epoch_seconds: blocked_until,
                schedule: Some(sched),
            };
        }
        return RenewalAction::IssueNow {
            replaces: Some(cert_id),
            schedule: Some(sched),
        };
    }

    // 2. Saved window check
    if sched.window_start.is_some() {
        if now >= sched.renew_at {
            if let Some(blocked_until) = issue_blocked {
                return RenewalAction::Wait {
                    until_epoch_seconds: blocked_until,
                    schedule: Some(sched),
                };
            }
            return RenewalAction::IssueNow {
                replaces: Some(cert_id),
                schedule: Some(sched),
            };
        }
        let wake = sched.renew_at.min(retry_after.retry_at);
        return RenewalAction::Wait {
            until_epoch_seconds: wake,
            schedule: Some(sched),
        };
    }

    // 3. No saved window: fallback to backstop
    if now >= backstop {
        if let Some(blocked_until) = issue_blocked {
            return RenewalAction::Wait {
                until_epoch_seconds: blocked_until,
                schedule: Some(sched),
            };
        }
        return RenewalAction::IssueNow {
            replaces: Some(cert_id),
            schedule: Some(sched),
        };
    }

    let wake = backstop.min(retry_after.retry_at);
    RenewalAction::Wait {
        until_epoch_seconds: wake,
        schedule: Some(sched),
    }
}

/// The result of probing the ACME directory for ARI support.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirectoryProbe {
    Present,
    Absent,
    Unknown,
}

/// Probe the ACME directory at `directory_url` for ARI support.
pub async fn probe_directory_renewal_info(
    http: &dyn HttpClient,
    directory_url: &str,
) -> DirectoryProbe {
    let req = match http::Request::builder()
        .method(http::Method::GET)
        .uri(directory_url)
        .header(http::header::USER_AGENT, "solstone-mcp-endpoint")
        .body(instant_acme::BodyWrapper::default())
    {
        Ok(r) => r,
        Err(_) => return DirectoryProbe::Unknown,
    };

    let rsp = match http.request(req).await {
        Ok(r) => r,
        Err(_) => return DirectoryProbe::Unknown,
    };

    if !rsp.parts.status.is_success() {
        return DirectoryProbe::Unknown;
    }

    let mut body_reader = rsp.body;
    let body = match body_reader.into_bytes().await {
        Ok(b) => b,
        Err(_) => return DirectoryProbe::Unknown,
    };

    #[derive(Deserialize)]
    struct DirectoryDoc {
        #[serde(rename = "renewalInfo")]
        renewal_info: Option<String>,
    }

    match serde_json::from_slice::<DirectoryDoc>(&body) {
        Ok(doc) => {
            if doc.renewal_info.is_some() {
                DirectoryProbe::Present
            } else {
                DirectoryProbe::Absent
            }
        }
        Err(_) => DirectoryProbe::Unknown,
    }
}

/// Configuration parameters for running the ACME renewal lifecycle loop.
pub struct RenewalLoopParams<'a> {
    pub journal_root: Option<&'a Path>,
    pub directory: &'a TlsStateDirectory,
    pub environment: McpEndpointCertificateEnvironment,
    pub hostname: &'a str,
    pub generation: Option<u64>,
    pub account_kid: String,
    pub account_key_pkcs8: Vec<u8>,
    pub tls_service: &'a McpEndpointTlsService,
    pub shutdown: &'a mut watch::Receiver<bool>,
    pub http_client: Arc<dyn HttpClient>,
    pub picker: Option<Arc<dyn Fn(i64, i64) -> i64 + Send + Sync>>,
}

/// Run the unified ACME renewal lifecycle loop until shutdown.
pub async fn run_renewal_lifecycle(
    params: RenewalLoopParams<'_>,
) -> Result<(), McpEndpointCertificateLifecycleError> {
    if *params.shutdown.borrow() || params.shutdown.has_changed().is_err() {
        return Ok(());
    }

    let default_directory = match params.environment {
        McpEndpointCertificateEnvironment::Production => {
            instant_acme::LetsEncrypt::Production.url()
        }
        McpEndpointCertificateEnvironment::Staging => instant_acme::LetsEncrypt::Staging.url(),
    };
    #[cfg(any(test, feature = "test-hooks"))]
    let effective_dir = params
        .tls_service
        .effective_directory_url()
        .unwrap_or_else(|| default_directory.to_string());
    #[cfg(not(any(test, feature = "test-hooks")))]
    let effective_dir = default_directory.to_string();
    let directory_url = effective_dir.as_str();

    let capturing_client = RawRetryAfterCapturingClient::new(Arc::clone(&params.http_client));

    // Connect to account using `AccountBuilder::from_parts` inside select against shutdown
    let account = loop {
        let pkcs8_der =
            rustls::pki_types::PrivatePkcs8KeyDer::from(params.account_key_pkcs8.clone());
        let account_builder_fut =
            instant_acme::Account::builder_with_http(Box::new(capturing_client.clone()))
                .from_parts(
                    params.account_kid.clone(),
                    pkcs8_der,
                    directory_url.to_string(),
                );

        let res = tokio::select! {
            changed = params.shutdown.changed() => {
                if changed.is_err() || *params.shutdown.borrow() {
                    return Ok(());
                }
                continue;
            }
            res = account_builder_fut => res,
        };

        match res {
            Ok(acc) => break acc,
            Err(_e) => {
                log::error!("failed to connect ACME account");
                tokio::select! {
                    changed = params.shutdown.changed() => {
                        if changed.is_err() || *params.shutdown.borrow() {
                            return Ok(());
                        }
                    }
                    _ = tokio::time::sleep(Duration::from_secs(INITIAL_ISSUE_BACKOFF_SECONDS)) => {}
                }
            }
        }
    };

    let mut force_staging = params.tls_service.force_staging_renewal();
    let mut already_replaced_attempts = 0_u32;
    let mut rate_limit_retry_count = 0_u32;
    let mut ari_outcome = AriOutcome::None;
    let mut last_directory_has_renewal_info: Option<bool> = None;

    let picker_fn: &(dyn Fn(i64, i64) -> i64 + Send + Sync) = match params.picker.as_deref() {
        Some(p) => p,
        None => &system_random_picker,
    };

    loop {
        if *params.shutdown.borrow() || params.shutdown.has_changed().is_err() {
            return Ok(());
        }

        let now = current_epoch_seconds();

        // 1. Inspect current leaf certificate from disk (if present)
        let leaf = load_current_leaf_info(params.directory, params.hostname, params.environment);

        // 2. Read saved renewal schedule from disk
        let saved_record = load_saved_schedule(
            params.directory,
            params.environment,
            params.hostname,
            params.generation,
            leaf.as_ref(),
        );

        // 3. Inspect directory support for renewalInfo with probe
        let probe = tokio::select! {
            changed = params.shutdown.changed() => {
                if changed.is_err() || *params.shutdown.borrow() {
                    return Ok(());
                }
                continue;
            }
            p = probe_directory_renewal_info(params.http_client.as_ref(), directory_url) => p,
        };

        let directory_has_renewal_info: bool = match probe {
            DirectoryProbe::Present => {
                last_directory_has_renewal_info = Some(true);
                true
            }
            DirectoryProbe::Absent => {
                last_directory_has_renewal_info = Some(false);
                false
            }
            DirectoryProbe::Unknown => {
                if let Some(last) = last_directory_has_renewal_info {
                    last
                } else if saved_record.as_ref().and_then(|s| s.window_start).is_some() {
                    // With a saved window, treat as present and take temporary ARI failure path
                    ari_outcome = AriOutcome::TemporaryFailure {
                        retry_after: parse_retry_after(None, now),
                    };
                    true
                } else {
                    let wait_delay = parse_retry_after(None, now).delay_seconds;
                    tokio::select! {
                        changed = params.shutdown.changed() => {
                            if changed.is_err() || *params.shutdown.borrow() {
                                return Ok(());
                            }
                        }
                        _ = tokio::time::sleep(Duration::from_secs(wait_delay as u64)) => {}
                    }
                    continue;
                }
            }
        };

        // 4. Pure decision
        let decision = decide_renewal_action(
            now,
            leaf.as_ref(),
            saved_record.as_ref(),
            params.environment,
            params.hostname,
            params.generation,
            directory_has_renewal_info,
            &ari_outcome,
            force_staging,
            picker_fn,
        );

        match decision {
            RenewalAction::Wait {
                until_epoch_seconds,
                schedule,
            } => {
                if let Some(ref sched) = schedule {
                    persist_schedule(params.directory, params.tls_service, sched);
                }
                ari_outcome = AriOutcome::None;
                let delay = Duration::from_secs((until_epoch_seconds - now).max(0) as u64);
                tokio::select! {
                    changed = params.shutdown.changed() => {
                        if changed.is_err() || *params.shutdown.borrow() {
                            return Ok(());
                        }
                        continue;
                    }
                    _ = tokio::time::sleep(delay) => {}
                }
            }
            RenewalAction::QueryAri { cert_id, schedule } => {
                if let Some(ref sched) = schedule {
                    persist_schedule(params.directory, params.tls_service, sched);
                }
                capturing_client.clear_retry_after();
                let ari_fut = account.renewal_info(&cert_id);
                tokio::select! {
                    changed = params.shutdown.changed() => {
                        if changed.is_err() || *params.shutdown.borrow() {
                            return Ok(());
                        }
                        continue;
                    }
                    res = ari_fut => {
                        let captured = capturing_client.captured_retry_after();
                        let retry_after = parse_retry_after(captured.as_deref(), current_epoch_seconds());
                        match res {
                            Ok((info, _)) => {
                                let start = info.suggested_window.start.unix_timestamp();
                                let end = info.suggested_window.end.unix_timestamp();
                                ari_outcome = AriOutcome::Success {
                                    window_start: start,
                                    window_end: end,
                                    retry_after: Some(retry_after),
                                };
                            }
                            Err(_) => {
                                ari_outcome = AriOutcome::TemporaryFailure { retry_after };
                            }
                        }
                    }
                }
            }
            RenewalAction::IssueNow { replaces, schedule } => {
                // Clear force staging latch once an issuance attempt starts
                force_staging = false;
                if let Some(ref sched) = schedule {
                    persist_schedule(params.directory, params.tls_service, sched);
                }

                let idents = [instant_acme::Identifier::Dns(params.hostname.to_string())];
                let mut order_builder = NewOrder::new(&idents);
                if let Some(rep) = replaces.clone() {
                    order_builder = order_builder.replaces(rep);
                }

                let order_fut = account.new_order(&order_builder);
                let order_res = tokio::select! {
                    changed = params.shutdown.changed() => {
                        if changed.is_err() || *params.shutdown.borrow() {
                            return Ok(());
                        }
                        continue;
                    }
                    res = order_fut => res,
                };

                match order_res {
                    Ok(order) => {
                        rate_limit_retry_count = 0;
                        already_replaced_attempts = 0;
                        let order_url = order.url().to_string();
                        // Persist order_url immediately before challenge
                        let mut sched = schedule.unwrap_or_else(|| RenewalScheduleRecord {
                            environment: params.environment,
                            hostname: params.hostname.to_string(),
                            generation: params.generation,
                            leaf_aki: leaf
                                .as_ref()
                                .map(|l| base64::engine::general_purpose::STANDARD.encode(&l.aki))
                                .unwrap_or_default(),
                            leaf_serial: leaf
                                .as_ref()
                                .map(|l| {
                                    base64::engine::general_purpose::STANDARD.encode(&l.raw_serial)
                                })
                                .unwrap_or_default(),
                            renew_at: now,
                            window_start: None,
                            window_end: None,
                            ari_retry_at: None,
                            ari_unavailable_since: None,
                            issue_retry_at: None,
                            issue_backoff_seconds: None,
                            order_url: Some(order_url.clone()),
                        });
                        sched.order_url = Some(order_url);
                        persist_schedule(params.directory, params.tls_service, &sched);

                        // Complete challenge, finalize, and activate
                        match execute_order_completion(
                            order,
                            params.hostname,
                            params.tls_service,
                            params.shutdown,
                            false, // fresh order
                        )
                        .await
                        {
                            OrderCompletionOutcome::Installed => {
                                sched.order_url = None;
                                sched.issue_retry_at = None;
                                sched.issue_backoff_seconds = None;
                                persist_schedule(params.directory, params.tls_service, &sched);
                                ari_outcome = AriOutcome::None;
                            }
                            OrderCompletionOutcome::Cancelled => {
                                // Cancelled on shutdown: leave order_url, installed cert, and account files alone
                                return Ok(());
                            }
                            OrderCompletionOutcome::Incomplete
                            | OrderCompletionOutcome::ValidKeyNotInMemory => {
                                record_issuance_deadline_backoff(
                                    params.directory,
                                    params.tls_service,
                                    &mut sched,
                                    now,
                                );
                            }
                            OrderCompletionOutcome::Failed(e) => {
                                log::error!("order completion failed");
                                record_publication_or_issuance_failure(
                                    params.directory,
                                    params.tls_service,
                                    &mut sched,
                                    e,
                                    now,
                                );
                            }
                        }
                    }
                    Err(instant_acme::Error::Api(problem))
                        if problem
                            .r#type
                            .as_deref()
                            .is_some_and(|s| s.ends_with(":alreadyReplaced")) =>
                    {
                        already_replaced_attempts = already_replaced_attempts.saturating_add(1);
                        if already_replaced_attempts >= ALREADY_REPLACED_ATTEMPTS {
                            if let Some(mut sched) = schedule {
                                record_issuance_deadline_backoff(
                                    params.directory,
                                    params.tls_service,
                                    &mut sched,
                                    now,
                                );
                            }
                            already_replaced_attempts = 0;
                        }
                    }
                    Err(instant_acme::Error::Api(problem))
                        if problem
                            .r#type
                            .as_deref()
                            .is_some_and(|s| s.ends_with(":rateLimited")) =>
                    {
                        if let Some(root) = params.journal_root {
                            let delay = 1_i64 << rate_limit_retry_count.min(16);
                            crate::owner_state::write_mcp_rate_limit_state(
                                root,
                                params.hostname,
                                chrono::Utc::now() + chrono::Duration::seconds(delay),
                            );
                        }
                        rate_limit_retry_count = rate_limit_retry_count.saturating_add(1);
                        if let Some(mut sched) = schedule {
                            record_issuance_deadline_backoff(
                                params.directory,
                                params.tls_service,
                                &mut sched,
                                now,
                            );
                        }
                    }
                    Err(_e) => {
                        log::error!("ACME new_order failed");
                        if let Some(mut sched) = schedule {
                            record_issuance_deadline_backoff(
                                params.directory,
                                params.tls_service,
                                &mut sched,
                                now,
                            );
                        }
                    }
                }
            }
            RenewalAction::ResumeOrder {
                order_url,
                schedule,
            } => {
                let order_fut = account.order(order_url.clone());
                let order_res = tokio::select! {
                    changed = params.shutdown.changed() => {
                        if changed.is_err() || *params.shutdown.borrow() {
                            return Ok(());
                        }
                        continue;
                    }
                    res = order_fut => res,
                };

                match order_res {
                    Ok(mut order) => {
                        if order.state().status == OrderStatus::Invalid {
                            if let Some(mut sched) = schedule {
                                sched.order_url = None;
                                record_issuance_deadline_backoff(
                                    params.directory,
                                    params.tls_service,
                                    &mut sched,
                                    now,
                                );
                            }
                        } else {
                            match execute_order_completion(
                                order,
                                params.hostname,
                                params.tls_service,
                                params.shutdown,
                                true, // resumed order
                            )
                            .await
                            {
                                OrderCompletionOutcome::Installed => {
                                    if let Some(mut sched) = schedule {
                                        sched.order_url = None;
                                        sched.issue_retry_at = None;
                                        sched.issue_backoff_seconds = None;
                                        persist_schedule(
                                            params.directory,
                                            params.tls_service,
                                            &sched,
                                        );
                                        ari_outcome = AriOutcome::None;
                                    }
                                }
                                OrderCompletionOutcome::Cancelled => {
                                    return Ok(());
                                }
                                OrderCompletionOutcome::ValidKeyNotInMemory => {
                                    log::warn!(
                                        "resumed order is Valid but CSR key was not generated in this process"
                                    );
                                    if let Some(mut sched) = schedule {
                                        record_issuance_deadline_backoff(
                                            params.directory,
                                            params.tls_service,
                                            &mut sched,
                                            now,
                                        );
                                    }
                                }
                                OrderCompletionOutcome::Incomplete => {
                                    if let Some(mut sched) = schedule {
                                        record_issuance_deadline_backoff(
                                            params.directory,
                                            params.tls_service,
                                            &mut sched,
                                            now,
                                        );
                                    }
                                }
                                OrderCompletionOutcome::Failed(e) => {
                                    log::error!("resumed order completion failed");
                                    if let Some(mut sched) = schedule {
                                        record_publication_or_issuance_failure(
                                            params.directory,
                                            params.tls_service,
                                            &mut sched,
                                            e,
                                            now,
                                        );
                                    }
                                }
                            }
                        }
                    }
                    Err(_e) => {
                        log::error!("failed to resume ACME order");
                        if let Some(mut sched) = schedule {
                            sched.order_url = None;
                            record_issuance_deadline_backoff(
                                params.directory,
                                params.tls_service,
                                &mut sched,
                                now,
                            );
                        }
                    }
                }
            }
        }
    }
}

pub enum OrderCompletionOutcome {
    Installed,
    Cancelled,
    Incomplete,
    ValidKeyNotInMemory,
    Failed(McpEndpointTlsError),
}

async fn execute_order_completion(
    mut order: instant_acme::Order,
    hostname: &str,
    tls_service: &McpEndpointTlsService,
    shutdown: &mut watch::Receiver<bool>,
    is_resumed: bool,
) -> OrderCompletionOutcome {
    // If order was resumed and is already Valid, but key was not generated in this process:
    if is_resumed && order.state().status == OrderStatus::Valid {
        return OrderCompletionOutcome::ValidKeyNotInMemory;
    }

    // 1. Authorizations & Challenges
    let mut challenge_guard = None;
    {
        let mut authzs = order.authorizations();
        while let Some(authz_res) = authzs.next().await {
            let mut authz = match authz_res {
                Ok(a) => a,
                Err(_) => return OrderCompletionOutcome::Failed(McpEndpointTlsError::State),
            };
            if authz.status == instant_acme::AuthorizationStatus::Valid {
                continue;
            }
            let mut challenge = match authz.challenge(instant_acme::ChallengeType::TlsAlpn01) {
                Some(c) => c,
                None => return OrderCompletionOutcome::Failed(McpEndpointTlsError::State),
            };
            let key_auth = challenge.key_authorization();
            let digest_bytes = key_auth.digest();
            let digest: [u8; 32] = match digest_bytes.as_ref().try_into() {
                Ok(d) => d,
                Err(_) => return OrderCompletionOutcome::Failed(McpEndpointTlsError::State),
            };

            // Install challenge and hold guard
            let guard = match tls_service.install_acme_challenge(digest) {
                Ok(g) => g,
                Err(e) => return OrderCompletionOutcome::Failed(e),
            };
            challenge_guard = Some(guard);

            let mut set_ready_fut = std::pin::pin!(challenge.set_ready());
            let ready_res = loop {
                tokio::select! {
                    changed = shutdown.changed() => {
                        if changed.is_err() || *shutdown.borrow() {
                            return OrderCompletionOutcome::Cancelled;
                        }
                    }
                    res = &mut set_ready_fut => break res,
                }
            };
            if ready_res.is_err() {
                return OrderCompletionOutcome::Failed(McpEndpointTlsError::State);
            }
        }
    }

    // Wait until order is ready / valid or cancelled
    loop {
        let mut refresh_fut = std::pin::pin!(order.refresh());
        let refresh_res = loop {
            tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        return OrderCompletionOutcome::Cancelled;
                    }
                }
                res = &mut refresh_fut => break res,
            }
        };

        let state = match refresh_res {
            Ok(s) => s,
            Err(_) => return OrderCompletionOutcome::Failed(McpEndpointTlsError::State),
        };
        if state.error.is_some() {
            return OrderCompletionOutcome::Failed(McpEndpointTlsError::State);
        }
        match state.status {
            OrderStatus::Ready | OrderStatus::Valid => break,
            OrderStatus::Invalid => {
                return OrderCompletionOutcome::Failed(McpEndpointTlsError::State);
            }
            OrderStatus::Pending | OrderStatus::Processing => {
                tokio::select! {
                    changed = shutdown.changed() => {
                        if changed.is_err() || *shutdown.borrow() {
                            return OrderCompletionOutcome::Cancelled;
                        }
                    }
                    _ = tokio::time::sleep(Duration::from_millis(250)) => {}
                }
            }
        }
    }

    // Drop challenge guard before finalize
    drop(challenge_guard);

    // 2. Finalize CSR if ready
    let private_key_der = if order.state().status == OrderStatus::Ready {
        let key_pair = {
            #[cfg(test)]
            let custom = tls_service
                .custom_order_key
                .lock()
                .ok()
                .and_then(|g| g.clone());
            #[cfg(not(test))]
            let custom: Option<Vec<u8>> = None;

            if let Some(der) = custom {
                match rcgen::KeyPair::try_from(der.as_slice()) {
                    Ok(k) => k,
                    Err(_) => return OrderCompletionOutcome::Failed(McpEndpointTlsError::State),
                }
            } else {
                match rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256) {
                    Ok(k) => k,
                    Err(_) => return OrderCompletionOutcome::Failed(McpEndpointTlsError::State),
                }
            }
        };
        let mut params = match rcgen::CertificateParams::new(vec![hostname.to_string()]) {
            Ok(p) => p,
            Err(_) => return OrderCompletionOutcome::Failed(McpEndpointTlsError::State),
        };
        params.distinguished_name = rcgen::DistinguishedName::new();
        let csr = match params.serialize_request(&key_pair) {
            Ok(c) => c,
            Err(_) => return OrderCompletionOutcome::Failed(McpEndpointTlsError::State),
        };

        let mut finalize_fut = std::pin::pin!(order.finalize_csr(csr.der()));
        let finalize_res = loop {
            tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        return OrderCompletionOutcome::Cancelled;
                    }
                }
                res = &mut finalize_fut => break res,
            }
        };
        if finalize_res.is_err() {
            return OrderCompletionOutcome::Failed(McpEndpointTlsError::State);
        }
        key_pair.serialize_der()
    } else if is_resumed && order.state().status == OrderStatus::Valid {
        return OrderCompletionOutcome::ValidKeyNotInMemory;
    } else {
        return OrderCompletionOutcome::Incomplete;
    };

    // 3. Poll certificate until valid
    loop {
        let mut refresh_fut = std::pin::pin!(order.refresh());
        let refresh_res = loop {
            tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        return OrderCompletionOutcome::Cancelled;
                    }
                }
                res = &mut refresh_fut => break res,
            }
        };

        let state = match refresh_res {
            Ok(s) => s,
            Err(_) => return OrderCompletionOutcome::Failed(McpEndpointTlsError::State),
        };
        if state.error.is_some() {
            return OrderCompletionOutcome::Failed(McpEndpointTlsError::State);
        }
        match state.status {
            OrderStatus::Valid => break,
            OrderStatus::Invalid => {
                return OrderCompletionOutcome::Failed(McpEndpointTlsError::State);
            }
            OrderStatus::Ready | OrderStatus::Pending | OrderStatus::Processing => {
                tokio::select! {
                    changed = shutdown.changed() => {
                        if changed.is_err() || *shutdown.borrow() {
                            return OrderCompletionOutcome::Cancelled;
                        }
                    }
                    _ = tokio::time::sleep(Duration::from_millis(250)) => {}
                }
            }
        }
    }

    // 4. Download and activate certificate
    let mut cert_fut = std::pin::pin!(order.certificate());
    let cert_res = loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return OrderCompletionOutcome::Cancelled;
                }
            }
            res = &mut cert_fut => break res,
        }
    };

    let cert_pem = match cert_res {
        Ok(Some(pem)) => pem,
        _ => return OrderCompletionOutcome::Failed(McpEndpointTlsError::State),
    };

    let parsed_pems = match pem::parse_many(cert_pem.as_bytes()) {
        Ok(p) => p,
        Err(_) => return OrderCompletionOutcome::Failed(McpEndpointTlsError::State),
    };
    let certificate_chain: Vec<Vec<u8>> =
        parsed_pems.into_iter().map(|p| p.into_contents()).collect();
    if certificate_chain.is_empty() {
        return OrderCompletionOutcome::Failed(McpEndpointTlsError::State);
    }

    let (_, parsed_leaf) = match parse_x509_certificate(&certificate_chain[0]) {
        Ok(l) => l,
        Err(_) => return OrderCompletionOutcome::Failed(McpEndpointTlsError::State),
    };
    let not_before = parsed_leaf.validity().not_before.timestamp();
    let not_after = parsed_leaf.validity().not_after.timestamp();

    if let Err(e) = tls_service.install_ordinary_certificate(
        certificate_chain,
        private_key_der,
        not_before,
        not_after,
    ) {
        return OrderCompletionOutcome::Failed(e);
    }

    OrderCompletionOutcome::Installed
}

fn load_current_leaf_info(
    directory: &TlsStateDirectory,
    hostname: &str,
    environment: McpEndpointCertificateEnvironment,
) -> Option<LeafCertInfo> {
    let bytes = unix::read_tls_state_bytes(directory).ok().flatten()?;
    match crate::tls::classify_stored_state(&bytes, hostname, environment).ok()? {
        crate::tls::StoredCertificateClassification::Same(decoded) => {
            extract_leaf_cert_info(&decoded.certificate_chain[0]).ok()
        }
        crate::tls::StoredCertificateClassification::Foreign => None,
    }
}

fn load_saved_schedule(
    directory: &TlsStateDirectory,
    environment: McpEndpointCertificateEnvironment,
    hostname: &str,
    generation: Option<u64>,
    leaf: Option<&LeafCertInfo>,
) -> Option<RenewalScheduleRecord> {
    let bytes = unix::read_tls_renewal_bytes(directory).ok().flatten()?;
    let record: RenewalScheduleRecord = serde_json::from_slice(&bytes).ok()?;
    if let Some(l) = leaf {
        if record.matches(environment, hostname, generation, &l.aki, &l.raw_serial) {
            Some(record)
        } else {
            None
        }
    } else if record.matches_no_leaf(environment, hostname, generation) {
        Some(record)
    } else {
        None
    }
}

pub(crate) fn persist_schedule(
    directory: &TlsStateDirectory,
    _tls_service: &McpEndpointTlsService,
    schedule: &RenewalScheduleRecord,
) {
    if let Ok(bytes) = serde_json::to_vec(schedule)
        && let Err(_e) = unix::persist_tls_renewal_bytes(directory, &bytes)
    {
        log::error!("failed to persist renewal schedule");
        #[cfg(test)]
        if let Ok(mut slot) = _tls_service.last_publication_error.lock() {
            *slot = Some(McpEndpointTlsError::State);
        }
    }
}

fn record_publication_or_issuance_failure(
    directory: &TlsStateDirectory,
    tls_service: &McpEndpointTlsService,
    sched: &mut RenewalScheduleRecord,
    _error: McpEndpointTlsError,
    now: i64,
) {
    #[cfg(test)]
    if let Ok(mut slot) = tls_service.last_publication_error.lock() {
        *slot = Some(_error);
    }
    record_issuance_deadline_backoff(directory, tls_service, sched, now);
}

fn record_issuance_deadline_backoff(
    directory: &TlsStateDirectory,
    tls_service: &McpEndpointTlsService,
    sched: &mut RenewalScheduleRecord,
    now: i64,
) {
    let next_backoff = match sched.issue_backoff_seconds {
        None => INITIAL_ISSUE_BACKOFF_SECONDS,
        Some(prev) => (prev * 2).min(MAX_ISSUE_BACKOFF_SECONDS),
    };
    sched.issue_backoff_seconds = Some(next_backoff);
    sched.issue_retry_at = Some(now + next_backoff as i64);
    persist_schedule(directory, tls_service, sched);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn certificate_identifier_display_equals_independent_base64url() {
        let aki = vec![1, 2, 3, 4, 5, 255];
        let serial = vec![0x00, 0x01, 0x02, 0xfe, 0xff]; // Leading 0x00 preserved

        let expected = format!(
            "{}.{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&aki),
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&serial)
        );

        let cert_id = CertificateIdentifier::new(Der::from_slice(&aki), Der::from_slice(&serial));
        assert_eq!(cert_id.to_string(), expected);

        let idents = [instant_acme::Identifier::Dns(
            "ab12cd34.solstone.me".to_string(),
        )];

        // Serialize NewOrder with replaces
        let order = NewOrder::new(&idents).replaces(cert_id);
        let serialized = serde_json::to_string(&order).expect("order serializes");
        assert!(serialized.contains(&format!("\"replaces\":\"{expected}\"")));

        // Initial issuance without replaces
        let order_no_rep = NewOrder::new(&idents);
        let serialized_no_rep = serde_json::to_string(&order_no_rep).expect("order serializes");
        assert!(!serialized_no_rep.contains("\"replaces\""));
    }

    #[test]
    fn strict_renewal_schedule_json_visitor() {
        let rec = RenewalScheduleRecord {
            environment: McpEndpointCertificateEnvironment::Production,
            hostname: "ab12cd34.solstone.me".to_string(),
            generation: Some(42),
            leaf_aki: "YWtp".to_string(),
            leaf_serial: "c2VyaWFs".to_string(),
            renew_at: 1_700_000_000,
            window_start: Some(1_699_000_000),
            window_end: Some(1_701_000_000),
            ari_retry_at: Some(1_700_050_000),
            ari_unavailable_since: None,
            issue_retry_at: None,
            issue_backoff_seconds: None,
            order_url: Some("https://acme.example/order/1".to_string()),
        };

        let json = serde_json::to_string(&rec).expect("serialize");
        let parsed: RenewalScheduleRecord = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(rec, parsed);

        // Duplicate field fails
        let dup_json = json.replace(
            "\"hostname\":\"ab12cd34.solstone.me\"",
            "\"hostname\":\"ab12cd34.solstone.me\",\"hostname\":\"ab12cd34.solstone.me\"",
        );
        assert!(serde_json::from_str::<RenewalScheduleRecord>(&dup_json).is_err());

        // Unknown field fails
        let unknown_json = json.replace("}", ",\"unknown_field\": 123}");
        assert!(serde_json::from_str::<RenewalScheduleRecord>(&unknown_json).is_err());
    }

    #[test]
    fn decision_matrix_no_renewal_info() {
        let now = 1_700_000_000;
        let not_before = now - 60_000;
        let not_after = now + 30_000;
        // lifetime = 90_000. backstop = not_after - 30_000 = now.
        let leaf = LeafCertInfo {
            not_before,
            not_after,
            aki: vec![1, 2, 3],
            raw_serial: vec![0, 4, 5],
        };

        // At backstop instant -> due now with no replaces
        let act = decide_renewal_action(
            now,
            Some(&leaf),
            None,
            McpEndpointCertificateEnvironment::Production,
            "ab12cd34.solstone.me",
            None,
            false, // no ARI
            &AriOutcome::None,
            false,
            &|s, _| s,
        );
        assert!(matches!(
            act,
            RenewalAction::IssueNow { replaces: None, .. }
        ));

        // Before backstop instant -> wait until backstop
        let act_before = decide_renewal_action(
            now - 5_000,
            Some(&leaf),
            None,
            McpEndpointCertificateEnvironment::Production,
            "ab12cd34.solstone.me",
            None,
            false,
            &AriOutcome::None,
            false,
            &|s, _| s,
        );
        assert!(matches!(
            act_before,
            RenewalAction::Wait {
                until_epoch_seconds,
                ..
            } if until_epoch_seconds == now
        ));
    }

    #[test]
    fn decision_matrix_empty_reversed_and_malformed_windows() {
        let now = 1_700_000_000;
        let not_before = now - 60_000;
        let not_after = now + 60_000;
        let leaf = LeafCertInfo {
            not_before,
            not_after,
            aki: vec![1, 2, 3],
            raw_serial: vec![0, 4, 5],
        };

        // Reversed window: start > end
        let outcome = AriOutcome::Success {
            window_start: now + 5000,
            window_end: now + 1000,
            retry_after: Some(AriRetry {
                delay_seconds: 3600,
                retry_at: now + 3600,
            }),
        };
        let act = decide_renewal_action(
            now,
            Some(&leaf),
            None,
            McpEndpointCertificateEnvironment::Production,
            "ab12cd34.solstone.me",
            None,
            true,
            &outcome,
            false,
            &|s, _| s,
        );
        assert!(matches!(act, RenewalAction::Wait { .. }));
        if let RenewalAction::Wait {
            schedule: Some(s), ..
        } = act
        {
            assert!(s.ari_unavailable_since.is_some());
            assert_eq!(s.ari_retry_at, Some(now + 3600));
        } else {
            panic!("expected Wait with schedule");
        }
    }

    #[test]
    fn decision_matrix_temporary_failure_and_prolonged_bound() {
        let now = 1_700_000_000;
        let not_before = now - 60_000;
        let not_after = now + 30_000;
        // lifetime = 90_000. backstop = not_after - 30_000 = now.
        let leaf = LeafCertInfo {
            not_before,
            not_after,
            aki: vec![1, 2, 3],
            raw_serial: vec![0, 4, 5],
        };

        let outcome = AriOutcome::TemporaryFailure {
            retry_after: AriRetry {
                delay_seconds: 120,
                retry_at: now + 120,
            },
        };

        let saved = RenewalScheduleRecord {
            environment: McpEndpointCertificateEnvironment::Production,
            hostname: "ab12cd34.solstone.me".to_string(),
            generation: None,
            leaf_aki: base64::engine::general_purpose::STANDARD.encode(&leaf.aki),
            leaf_serial: base64::engine::general_purpose::STANDARD.encode(&leaf.raw_serial),
            renew_at: now + 5000,
            window_start: Some(now + 4000),
            window_end: Some(now + 6000),
            ari_retry_at: None,
            ari_unavailable_since: None,
            issue_retry_at: None,
            issue_backoff_seconds: None,
            order_url: None,
        };

        let act = decide_renewal_action(
            now,
            Some(&leaf),
            Some(&saved),
            McpEndpointCertificateEnvironment::Production,
            "ab12cd34.solstone.me",
            None,
            true,
            &outcome,
            false,
            &|s, _| s,
        );
        assert!(matches!(
            act,
            RenewalAction::Wait {
                until_epoch_seconds,
                schedule: Some(s)
            } if until_epoch_seconds == now + 120 && s.renew_at == now + 5000
        ));

        // After 7-day prolonged failure + backstop due -> issues now even though renew_at is in future
        let mut prolonged_saved = saved;
        prolonged_saved.ari_unavailable_since = Some(now - ARI_PROLONGED_FAILURE - 10);
        let act_prolonged = decide_renewal_action(
            now,
            Some(&leaf),
            Some(&prolonged_saved),
            McpEndpointCertificateEnvironment::Production,
            "ab12cd34.solstone.me",
            None,
            true,
            &outcome,
            false,
            &|s, _| s,
        );
        assert!(matches!(
            act_prolonged,
            RenewalAction::IssueNow {
                replaces: Some(_),
                ..
            }
        ));
    }

    #[test]
    fn decision_matrix_reload_restores_and_stored_order_url_resumes() {
        let now = 1_700_000_000;
        let leaf = LeafCertInfo {
            not_before: now - 1000,
            not_after: now + 50_000,
            aki: vec![10],
            raw_serial: vec![20],
        };

        let saved = RenewalScheduleRecord {
            environment: McpEndpointCertificateEnvironment::Production,
            hostname: "ab12cd34.solstone.me".to_string(),
            generation: None,
            leaf_aki: base64::engine::general_purpose::STANDARD.encode(&leaf.aki),
            leaf_serial: base64::engine::general_purpose::STANDARD.encode(&leaf.raw_serial),
            renew_at: now - 100, // Due stays due!
            window_start: Some(now - 200),
            window_end: Some(now + 200),
            ari_retry_at: None,
            ari_unavailable_since: None,
            issue_retry_at: None,
            issue_backoff_seconds: None,
            order_url: None,
        };

        let act = decide_renewal_action(
            now,
            Some(&leaf),
            Some(&saved),
            McpEndpointCertificateEnvironment::Production,
            "ab12cd34.solstone.me",
            None,
            true,
            &AriOutcome::None,
            false,
            &|s, _| s,
        );
        assert!(matches!(
            act,
            RenewalAction::IssueNow {
                replaces: Some(_),
                ..
            }
        ));

        // Stored order_url resumes
        let mut saved_with_order = saved;
        saved_with_order.order_url = Some("https://acme.example/orders/42".to_string());
        let act_resume = decide_renewal_action(
            now,
            Some(&leaf),
            Some(&saved_with_order),
            McpEndpointCertificateEnvironment::Production,
            "ab12cd34.solstone.me",
            None,
            true,
            &AriOutcome::None,
            false,
            &|s, _| s,
        );
        assert!(matches!(
            act_resume,
            RenewalAction::ResumeOrder {
                order_url,
                ..
            } if order_url == "https://acme.example/orders/42"
        ));
    }
}
