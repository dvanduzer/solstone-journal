// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Owner-facing brain inspection and fenced refresh orchestration.

use std::collections::BTreeMap;
use std::env;
use std::path::Path;
use std::time::{Duration as StdDuration, SystemTime};

use chrono::{DateTime, Duration, Utc};
use serde_json::{Map, Value, json};
pub use solstone_core_brain::BundledRuntimePrerequisiteAssessment;
use solstone_core_brain::{
    BeginPrerequisiteRenewal, BeginRefreshError, abandon_refresh,
    assess_bundled_runtime_prerequisite, begin_prerequisite_renewal, begin_refresh,
    brain_refresh_lease_path, brain_state_path, bundled_runtime_desired_fingerprint,
    derive_active_brain_lane, evidence_component_for_reason, finish_prerequisite_renewal,
    finish_refresh, inspect_brain_state, is_valid_evidence_reason, load_existing_fingerprint_key,
    probe_file_lease_held, read_journal_config, spp_reason_diagnostic, valid_spp_reason,
};
use solstone_core_generate::{
    ClientError, ContentPart, GenerateRequest, GenerateResponse, GeneratedResponse,
    HEALTH_BRAIN_GENERATE_CONTEXT,
};
use solstone_core_spp_ratls::AttestationSession;

const COMPONENTS: [&str; 3] = ["configuration", "lane_prerequisites", "generate"];

#[derive(Clone, Debug, PartialEq)]
pub struct OwnerRefreshOptions {
    pub expected_fingerprint: Option<String>,
    pub expected_active_fingerprint: bool,
    pub expect_active_fingerprint_absent: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct OwnerView {
    pub aggregate_state: String,
    pub reason_code: Option<String>,
    pub lane: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub fingerprint_sha256: Option<String>,
    pub failing_component: Option<String>,
    pub observed_at: Option<String>,
    pub expires_at: Option<String>,
    pub path: Option<String>,
    pub checked_age: Option<String>,
    pub transient: Option<&'static str>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum OwnerRefreshOutcome {
    PersistedReady {
        view: OwnerView,
    },
    PersistedNonReady {
        view: OwnerView,
        reason_code: String,
    },
    Busy {
        view: OwnerView,
    },
    StaleExpectedFingerprint {
        view: OwnerView,
    },
    LostFence {
        view: OwnerView,
    },
    OperationalFailure {
        message: String,
    },
}

pub trait OwnerProbe {
    fn bundled_runtime_fingerprint(
        &self,
        journal: &Path,
        config: &Map<String, Value>,
    ) -> Option<String>;
    fn assess_bundled_runtime(
        &self,
        journal: &Path,
        expected: Option<&str>,
    ) -> BundledRuntimePrerequisiteAssessment;
    fn execute_generate(
        &self,
        request: &GenerateRequest,
        journal: &Path,
        config: &Map<String, Value>,
    ) -> Result<GenerateResponse, ClientError>;
    fn after_permit_acquired(&self, _journal: &Path) {}
}

pub trait OwnerSppPrerequisite {
    fn assess(&self, journal: &Path, config: &Map<String, Value>, now: DateTime<Utc>) -> Value;
}

pub struct ProductionSppPrerequisite;

impl OwnerSppPrerequisite for ProductionSppPrerequisite {
    fn assess(&self, journal: &Path, config: &Map<String, Value>, now: DateTime<Utc>) -> Value {
        spp_prerequisite(journal, config, now)
    }
}

pub fn bundled_runtime_fingerprint(journal: &Path, config: &Map<String, Value>) -> Option<String> {
    current_bundled_runtime_fingerprint(journal, config, None)
}

pub fn assess_bundled_runtime(
    journal: &Path,
    expected: Option<&str>,
) -> BundledRuntimePrerequisiteAssessment {
    assess_bundled_runtime_prerequisite(journal, expected)
}

pub struct RespondWithConfigProbe {
    pub runtime: solstone_core_generate_wire::EndpointRuntime,
}

impl OwnerProbe for RespondWithConfigProbe {
    fn bundled_runtime_fingerprint(
        &self,
        journal: &Path,
        config: &Map<String, Value>,
    ) -> Option<String> {
        bundled_runtime_fingerprint(journal, config)
    }

    fn assess_bundled_runtime(
        &self,
        journal: &Path,
        expected: Option<&str>,
    ) -> BundledRuntimePrerequisiteAssessment {
        assess_bundled_runtime(journal, expected)
    }

    fn execute_generate(
        &self,
        request: &GenerateRequest,
        journal: &Path,
        config: &Map<String, Value>,
    ) -> Result<GenerateResponse, ClientError> {
        solstone_core_generate_wire::respond_with_config(
            request,
            &self.runtime,
            journal,
            config,
            |_| None,
        )
        .map_err(ClientError::Decode)
    }
}

pub fn refresh(
    journal: &Path,
    options: &OwnerRefreshOptions,
    probe: &impl OwnerProbe,
    spp: &impl OwnerSppPrerequisite,
) -> OwnerRefreshOutcome {
    let now = Utc::now();
    if options.expected_fingerprint.is_some() && options.expect_active_fingerprint_absent {
        return OwnerRefreshOutcome::StaleExpectedFingerprint {
            view: transient_view("stale_expected_fingerprint", &empty_view()),
        };
    }
    let config = match read_journal_config(journal) {
        Ok(read) => read.config.unwrap_or_default(),
        Err(error) => {
            return OwnerRefreshOutcome::OperationalFailure {
                message: format!("thinking check failed: could not read journal config: {error}"),
            };
        }
    };
    let before = owner_view(journal, &config, now);
    let initial_resolution = derive_active_brain_lane(&config);
    let bundled_runtime_fingerprint = probe.bundled_runtime_fingerprint(journal, &config);
    if let Some(expected) = options.expected_fingerprint.as_deref() {
        let actual = if options.expected_active_fingerprint {
            before.fingerprint_sha256.as_deref()
        } else {
            (initial_resolution.lane.as_deref() == Some("bundled"))
                .then_some(bundled_runtime_fingerprint.as_deref())
                .flatten()
        };
        if actual != Some(expected) {
            return OwnerRefreshOutcome::StaleExpectedFingerprint {
                view: transient_view("stale_expected_fingerprint", &empty_view()),
            };
        }
    }
    if before.reason_code.as_deref() == Some("configuration_invalid") {
        return persisted(before);
    }
    if options.expected_fingerprint.is_some()
        && !options.expected_active_fingerprint
        && before.aggregate_state == "ready"
    {
        return OwnerRefreshOutcome::PersistedReady { view: before };
    }
    let permit = match begin_refresh(
        journal,
        now,
        None,
        if options.expected_active_fingerprint {
            options.expected_fingerprint.as_deref()
        } else {
            None
        },
        options.expect_active_fingerprint_absent,
        bundled_runtime_fingerprint.clone(),
    ) {
        Ok(Some(permit)) => permit,
        Ok(None) => {
            if probe_file_lease_held(&brain_refresh_lease_path(journal)).unwrap_or(false) {
                return OwnerRefreshOutcome::Busy {
                    view: transient_view("busy", &before),
                };
            }
            let current = owner_view(journal, &config, Utc::now());
            return persisted(current);
        }
        Err(BeginRefreshError::ExpectedFingerprintStale(_)) => {
            return OwnerRefreshOutcome::StaleExpectedFingerprint {
                view: transient_view("stale_expected_fingerprint", &empty_view()),
            };
        }
        Err(error) => {
            return OwnerRefreshOutcome::OperationalFailure {
                message: format!("thinking check failed: {error}"),
            };
        }
    };

    probe.after_permit_acquired(journal);

    let checking_config = match read_journal_config(journal) {
        Ok(read) => read.config.unwrap_or_default(),
        Err(_) => {
            return abandon_probe_failure(journal, &config, permit, now);
        }
    };
    let key = load_existing_fingerprint_key(journal);
    let recomputed = key.as_ref().and_then(|key| {
        solstone_core_brain::build_active_brain_fingerprint(
            &checking_config,
            key,
            bundled_runtime_fingerprint.clone().map(Value::String),
            Some(journal),
        )
        .ok()
        .flatten()
    });
    if recomputed.as_deref() != Some(permit.fingerprint_sha256.as_str()) {
        drop(permit);
        return OwnerRefreshOutcome::LostFence {
            view: transient_view("lost_fence", &empty_view()),
        };
    }

    let checking_view = owner_view(journal, &checking_config, Utc::now());
    let (Some(lane), Some(_provider), Some(_model)) = (
        checking_view.lane.as_deref(),
        checking_view.provider.as_deref(),
        checking_view.model.as_deref(),
    ) else {
        return abandon_probe_failure(journal, &checking_config, permit, now);
    };

    let outcome = probe_outcome(
        journal,
        &checking_config,
        lane,
        bundled_runtime_fingerprint.as_deref(),
        now,
        probe,
        spp,
    );
    let finish_bundled_runtime_fingerprint = (lane == "bundled")
        .then(|| probe.bundled_runtime_fingerprint(journal, &checking_config))
        .flatten();
    if finish_refresh(
        journal,
        permit,
        outcome,
        Utc::now(),
        finish_bundled_runtime_fingerprint,
    )
    .is_err()
    {
        return OwnerRefreshOutcome::LostFence {
            view: transient_view("lost_fence", &empty_view()),
        };
    }
    let committed = owner_view(journal, &checking_config, Utc::now());
    persisted(committed)
}

fn abandon_probe_failure(
    journal: &Path,
    fallback_config: &Map<String, Value>,
    permit: solstone_core_brain::BrainRefreshPermit,
    now: DateTime<Utc>,
) -> OwnerRefreshOutcome {
    if abandon_refresh(journal, permit, "probe_internal_error", Map::new(), now).is_err() {
        return OwnerRefreshOutcome::LostFence {
            view: transient_view("lost_fence", &empty_view()),
        };
    }
    let config = read_journal_config(journal)
        .ok()
        .and_then(|read| read.config)
        .unwrap_or_else(|| fallback_config.clone());
    let committed = owner_view(journal, &config, Utc::now());
    persisted(committed)
}

pub fn renew_prerequisites(
    journal: &Path,
    expected_fingerprint: Option<&str>,
    probe: &impl OwnerProbe,
    spp: &impl OwnerSppPrerequisite,
) -> OwnerRefreshOutcome {
    let now = Utc::now();
    let config = match read_journal_config(journal) {
        Ok(read) => read.config.unwrap_or_default(),
        Err(error) => {
            return OwnerRefreshOutcome::OperationalFailure {
                message: format!("thinking check failed: could not read journal config: {error}"),
            };
        }
    };
    let current = owner_view(journal, &config, now);
    if expected_fingerprint.is_some()
        && current.fingerprint_sha256.as_deref() != expected_fingerprint
    {
        return OwnerRefreshOutcome::StaleExpectedFingerprint {
            view: transient_view("stale_expected_fingerprint", &empty_view()),
        };
    }
    match begin_prerequisite_renewal(journal, now, None, expected_fingerprint, None) {
        BeginPrerequisiteRenewal::Started(permit) => {
            probe.after_permit_acquired(journal);

            let checking_config = match read_journal_config(journal) {
                Ok(read) => read.config.unwrap_or_default(),
                Err(_) => {
                    drop(permit);
                    return OwnerRefreshOutcome::LostFence {
                        view: transient_view("lost_fence", &empty_view()),
                    };
                }
            };

            let key = load_existing_fingerprint_key(journal);
            let recomputed = key.as_ref().and_then(|key| {
                solstone_core_brain::build_active_brain_fingerprint(
                    &checking_config,
                    key,
                    None,
                    Some(journal),
                )
                .ok()
                .flatten()
            });
            if recomputed.as_deref() != Some(&permit.fingerprint_sha256) {
                drop(permit);
                return OwnerRefreshOutcome::LostFence {
                    view: transient_view("lost_fence", &empty_view()),
                };
            }

            let component = spp.assess(journal, &checking_config, now);
            if finish_prerequisite_renewal(journal, permit, component, Utc::now(), None).is_err() {
                return OwnerRefreshOutcome::LostFence {
                    view: transient_view("lost_fence", &empty_view()),
                };
            }
            let committed = owner_view(journal, &checking_config, Utc::now());
            persisted(committed)
        }
        BeginPrerequisiteRenewal::Busy { .. } => OwnerRefreshOutcome::Busy {
            view: transient_view("busy", &current),
        },
        BeginPrerequisiteRenewal::Unsafe { .. } => refresh(
            journal,
            &OwnerRefreshOptions {
                expected_fingerprint: expected_fingerprint.map(str::to_owned),
                expected_active_fingerprint: expected_fingerprint.is_some(),
                expect_active_fingerprint_absent: false,
            },
            probe,
            spp,
        ),
    }
}

fn persisted(view: OwnerView) -> OwnerRefreshOutcome {
    if view.aggregate_state == "ready" {
        OwnerRefreshOutcome::PersistedReady { view }
    } else {
        OwnerRefreshOutcome::PersistedNonReady {
            reason_code: view.reason_code.clone().unwrap_or_default(),
            view,
        }
    }
}

fn probe_outcome(
    journal: &Path,
    config: &Map<String, Value>,
    lane: &str,
    bundled_runtime_fingerprint: Option<&str>,
    now: DateTime<Utc>,
    probe: &impl OwnerProbe,
    spp: &impl OwnerSppPrerequisite,
) -> Value {
    let lane_prerequisites = lane_prerequisite(
        journal,
        config,
        lane,
        bundled_runtime_fingerprint,
        now,
        probe,
        spp,
    );
    if let Some(reason) = lane_prerequisites
        .get("reason_code")
        .and_then(Value::as_str)
    {
        return json!({
            "configuration": component_ok(now),
            "lane_prerequisites": lane_prerequisites,
            "generate": component_not_attempted(reason, now),
        });
    }
    json!({
        "configuration": component_ok(now),
        "lane_prerequisites": lane_prerequisites,
        "generate": generate_component(journal, config, now, probe),
    })
}

fn lane_prerequisite(
    journal: &Path,
    config: &Map<String, Value>,
    lane: &str,
    bundled_runtime_fingerprint: Option<&str>,
    now: DateTime<Utc>,
    probe: &impl OwnerProbe,
    spp: &impl OwnerSppPrerequisite,
) -> Value {
    match lane {
        "bundled" => {
            let assessment = probe.assess_bundled_runtime(journal, bundled_runtime_fingerprint);
            assessment.reason_code.map_or_else(
                || component_ok(now),
                |reason| {
                    let diagnostic = runtime_diagnostic(
                        &reason,
                        assessment.phase.as_deref(),
                        assessment.runtime_reason.as_deref(),
                    );
                    component_for_reason("lane_prerequisites", &reason, diagnostic, now)
                },
            )
        }
        "byo-cloud" => {
            let provider = derive_active_brain_lane(config).provider;
            if provider == "chatgpt" {
                let status = solstone_core_chatgpt_auth::get_status(journal);
                let signed_in = status.map(|s| s.signed_in).unwrap_or(false);
                if signed_in {
                    component_ok(now)
                } else {
                    component_for_reason(
                        "lane_prerequisites",
                        "chatgpt_sign_in_required",
                        Map::new(),
                        now,
                    )
                }
            } else {
                let key_name = match provider.as_str() {
                    "google" => "GOOGLE_API_KEY",
                    "openai" => "OPENAI_API_KEY",
                    "anthropic" => "ANTHROPIC_API_KEY",
                    _ => return component_ok(now),
                };
                let configured = config
                    .get("env")
                    .and_then(Value::as_object)
                    .and_then(|env| env.get(key_name))
                    .and_then(Value::as_str);
                let present = configured.is_some_and(|key| !key.trim().is_empty())
                    || env::var(key_name).is_ok_and(|key| !key.trim().is_empty());
                if present {
                    component_ok(now)
                } else {
                    component_for_reason(
                        "lane_prerequisites",
                        "provider_key_missing",
                        Map::new(),
                        now,
                    )
                }
            }
        }
        "spp" => spp.assess(journal, config, now),
        _ => component_ok(now),
    }
}

fn spp_prerequisite(journal: &Path, config: &Map<String, Value>, now: DateTime<Utc>) -> Value {
    let endpoint = match solstone_core_local::resolve_local_endpoint(config) {
        solstone_core_local::LocalEndpointResolution::Byo(endpoint) if endpoint.is_confidential => {
            endpoint
        }
        _ => {
            return component_for_reason(
                "lane_prerequisites",
                "attestation_not_verified",
                Map::new(),
                now,
            );
        }
    };
    #[cfg(not(windows))]
    let (prerequisite_err, nvattest_dir) = {
        let _ = journal;
        match solstone_core_spp_ratls::resolve_nvattest_from_current_exe() {
            Ok(_) => (None, std::path::PathBuf::new()),
            Err(refusal) => (Some(refusal.failure), std::path::PathBuf::new()),
        }
    };
    #[cfg(windows)]
    let (prerequisite_err, nvattest_dir) = {
        let dir = solstone_core_spp_ratls::resolve_nvattest_dir(Some(config), journal);
        let status = solstone_core_spp_ratls::check_nvattest_readiness(&dir);
        (
            solstone_core_spp_ratls::classify_nvattest_prerequisite(status),
            dir,
        )
    };
    if let Some(failure) = prerequisite_err {
        let reason = valid_spp_reason(failure.reason_code);
        let diagnostic = spp_reason_diagnostic(failure.reason_code);
        return component_for_reason("lane_prerequisites", reason, diagnostic, now);
    }

    #[cfg(not(windows))]
    let readiness = |_: &Path| match solstone_core_spp_ratls::resolve_nvattest_from_current_exe() {
        Ok(_) => solstone_core_spp_ratls::NvattestEnsureStatus::AlreadyInstalled,
        Err(refusal) => match refusal.failure.reason_code {
            "nvattest_integrity_failed" => {
                solstone_core_spp_ratls::NvattestEnsureStatus::IntegrityFailed
            }
            _ => solstone_core_spp_ratls::NvattestEnsureStatus::Unavailable,
        },
    };
    #[cfg(windows)]
    let readiness = |dir: &Path| solstone_core_spp_ratls::check_nvattest_readiness(dir);

    let state = solstone_core_spp_ratls::AttestationStateStore::new();
    match solstone_core_spp_ratls::perform_fresh_reattest(
        &state,
        &endpoint.base_url,
        &nvattest_dir,
        StdDuration::from_secs(120),
        readiness,
    ) {
        Ok(channel) if channel.session.status(SystemTime::now()) == "verified" => {
            spp_component_ok(now, &channel.session)
        }
        Ok(_) => component_for_reason("lane_prerequisites", "attestation_expired", Map::new(), now),
        Err(failure) => {
            let reason = valid_spp_reason(failure.reason_code);
            let diagnostic = spp_reason_diagnostic(failure.reason_code);
            component_for_reason("lane_prerequisites", reason, diagnostic, now)
        }
    }
}

fn generate_component(
    journal: &Path,
    config: &Map<String, Value>,
    now: DateTime<Utc>,
    probe: &impl OwnerProbe,
) -> Value {
    let request = GenerateRequest {
        id: None,
        context: HEALTH_BRAIN_GENERATE_CONTEXT.to_owned(),
        contents: vec![ContentPart::Text {
            text: "Reply with the single word OK.".to_owned(),
        }],
        system_instruction: None,
        temperature: 0.0,
        max_output_tokens: 512,
        timeout_s: Some(30.0),
        json_output: false,
        json_schema: None,
        attempt_index: 0,
        exclusive_admission: false,
        transport_retries: Some(0),
    };
    let result = probe.execute_generate(&request, journal, config);
    let reason = match result {
        Ok(GenerateResponse::Generated(response)) => {
            let Some(reason) = classify_canned_generate(&response) else {
                return component_ok(now);
            };
            reason.to_owned()
        }
        Ok(GenerateResponse::Refused(response)) => map_provider_reason(
            "generate",
            response.reason_code.as_ref().map(|value| value.as_wire()),
        ),
        Err(error) => generate_client_failure_reason(&error),
    };
    component_for_reason("generate", &reason, Map::new(), now)
}

fn generate_client_failure_reason(error: &ClientError) -> String {
    match error {
        ClientError::Protocol(failure) => {
            map_provider_reason("generate", Some(&failure.error.reason))
        }
        _ => "probe_internal_error".to_owned(),
    }
}

fn classify_canned_generate(response: &GeneratedResponse) -> Option<&'static str> {
    if response.finish_reason == "max_tokens" {
        return Some("probe_output_starved");
    }
    if !response.text.trim().is_empty() {
        return None;
    }
    if generated_response_has_reasoning(response) || response.finish_reason != "stop" {
        Some("probe_output_starved")
    } else {
        Some("provider_response_invalid")
    }
}

fn generated_response_has_reasoning(response: &GeneratedResponse) -> bool {
    response
        .thinking
        .as_ref()
        .and_then(Value::as_array)
        .is_some_and(|thinking| !thinking.is_empty())
        || response.usage.as_object().is_some_and(|usage| {
            usage.iter().any(|(name, value)| {
                name.contains("reasoning")
                    && !value.is_boolean()
                    && value.as_f64().is_some_and(|count| count > 0.0)
            })
        })
}

fn map_provider_reason(component: &str, reason: Option<&str>) -> String {
    // The local endpoint wire names its two failures with a `local_` prefix;
    // the brain contract knows them without it.
    let reason = reason.map(|reason| match reason {
        "local_endpoint_unreachable" => "endpoint_unreachable",
        "local_endpoint_contract_failed" => "endpoint_contract_failed",
        other => other,
    });
    match reason {
        Some(reason) if is_valid_evidence_reason(component, reason) => reason.to_owned(),
        _ => "probe_internal_error".to_owned(),
    }
}

fn component_ok(now: DateTime<Utc>) -> Value {
    json!({"status":"ok", "observed_at": now.to_rfc3339(), "expires_at": (now + Duration::hours(26)).to_rfc3339()})
}

fn spp_component_ok(now: DateTime<Utc>, session: &AttestationSession) -> Value {
    let expires_at = [
        session.tpm_heartbeat_due_at(),
        session.gpu_reattest_due_at(),
        session.session_cap_at(),
    ]
    .into_iter()
    .min()
    .expect("attestation session has deadlines");
    json!({
        "status": "ok",
        "observed_at": now.to_rfc3339(),
        "expires_at": DateTime::<Utc>::from(expires_at).to_rfc3339(),
    })
}

fn component_not_attempted(reason: &str, now: DateTime<Utc>) -> Value {
    json!({"status":"not_attempted", "observed_at":now.to_rfc3339(), "reason_code":reason})
}

fn runtime_diagnostic(
    reason: &str,
    phase: Option<&str>,
    runtime_reason: Option<&str>,
) -> Map<String, Value> {
    let mut diagnostic = Map::new();
    if let Some(phase) = phase {
        diagnostic.insert("phase".to_owned(), Value::String(phase.to_owned()));
    }
    if reason != "local_runtime_fingerprint_mismatch"
        && let Some(runtime_reason) = runtime_reason
    {
        diagnostic.insert(
            "runtime_reason".to_owned(),
            Value::String(runtime_reason.to_owned()),
        );
    }
    diagnostic
}

fn component_for_reason(
    component: &str,
    reason: &str,
    diagnostic: Map<String, Value>,
    now: DateTime<Utc>,
) -> Value {
    evidence_component_for_reason(component, reason, diagnostic, now)
        .expect("owner evidence reason is admitted by the shared contract")
}

pub fn owner_view(journal: &Path, config: &Map<String, Value>, now: DateTime<Utc>) -> OwnerView {
    let inspection = inspect_brain_state(journal, config, now);
    let (failing_component, observed_at, expires_at) = evidence_view(inspection.record.as_ref());
    let checked_age = observed_at
        .as_deref()
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| age(now, value.with_timezone(&Utc)));
    OwnerView {
        aggregate_state: inspection.projection.aggregate_state,
        reason_code: inspection.projection.reason_code,
        lane: inspection.projection.active_lane,
        provider: inspection.projection.active_provider,
        model: inspection.projection.active_model,
        fingerprint_sha256: inspection.projection.fingerprint_sha256,
        failing_component,
        observed_at,
        expires_at,
        path: Some(brain_state_path(journal).display().to_string()),
        checked_age,
        transient: None,
    }
}

fn evidence_view(record: Option<&Value>) -> (Option<String>, Option<String>, Option<String>) {
    let Some(evidence) = record
        .and_then(|value| value.get("evidence"))
        .and_then(Value::as_object)
    else {
        return (None, None, None);
    };
    let mut ready = (None, None);
    for name in COMPONENTS {
        let Some(component) = evidence.get(name).and_then(Value::as_object) else {
            continue;
        };
        let observed = component
            .get("observed_at")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        let expires = component
            .get("expires_at")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        if component.get("status").and_then(Value::as_str) != Some("ok") {
            return (Some(name.to_owned()), observed, expires);
        }
        if ready.0.is_none() {
            ready = (observed, expires);
        }
    }
    (None, ready.0, ready.1)
}

fn empty_view() -> OwnerView {
    OwnerView {
        aggregate_state: "unknown".to_owned(),
        reason_code: None,
        lane: None,
        provider: None,
        model: None,
        fingerprint_sha256: None,
        failing_component: None,
        observed_at: None,
        expires_at: None,
        path: None,
        checked_age: None,
        transient: None,
    }
}

fn transient_view(reason: &'static str, previous: &OwnerView) -> OwnerView {
    OwnerView {
        aggregate_state: if reason == "busy" {
            "checking"
        } else {
            "unknown"
        }
        .to_owned(),
        reason_code: Some(reason.to_owned()),
        lane: previous.lane.clone(),
        provider: previous.provider.clone(),
        model: previous.model.clone(),
        fingerprint_sha256: previous.fingerprint_sha256.clone(),
        failing_component: None,
        observed_at: None,
        expires_at: None,
        path: previous.path.clone(),
        checked_age: None,
        transient: Some(reason),
    }
}

pub fn json_text(view: &OwnerView) -> String {
    let object = BTreeMap::from([
        ("aggregate_state", json!(view.aggregate_state)),
        ("expires_at", json!(view.expires_at)),
        ("failing_component", json!(view.failing_component)),
        ("fingerprint_sha256", json!(view.fingerprint_sha256)),
        ("lane", json!(view.lane)),
        ("model", json!(view.model)),
        ("observed_at", json!(view.observed_at)),
        ("path", json!(view.path)),
        ("provider", json!(view.provider)),
        ("reason_code", json!(view.reason_code)),
    ]);
    serde_json::to_string(&object).expect("owner view serializes")
}

pub fn human_text(view: &OwnerView) -> String {
    if view.transient == Some("busy") {
        return format!(
            "Thinking busy: {}",
            solstone_core_brain::brain_reason_text(Some("busy"))
        );
    }
    if view.aggregate_state == "ready" {
        let identity = match (&view.provider, &view.model, &view.lane) {
            (Some(provider), Some(model), lane) => format!(
                "{} {provider}/{model}",
                lane.as_deref()
                    .map(solstone_core_brain::lane_label)
                    .unwrap_or("unknown")
            ),
            (_, _, Some(lane)) => solstone_core_brain::lane_label(lane).to_owned(),
            _ => "unknown".to_owned(),
        };
        let age = view
            .checked_age
            .as_ref()
            .map(|value| format!(", checked {value} ago"))
            .unwrap_or_default();
        return format!("Thinking ready: {identity}{age}");
    }
    let reason = solstone_core_brain::brain_reason_text(view.reason_code.as_deref());
    let component = view
        .failing_component
        .as_ref()
        .map(|value| format!(" ({value})"))
        .unwrap_or_default();
    format!("Thinking {}: {reason}{component}", view.aggregate_state)
}

pub fn owner_exit_code(view: &OwnerView) -> u8 {
    if matches!(
        view.transient,
        Some("busy" | "stale_expected_fingerprint" | "lost_fence")
    ) {
        return 3;
    }
    match view.aggregate_state.as_str() {
        "ready" => 0,
        "blocked" | "unhealthy" => 1,
        _ => 2,
    }
}

fn age(now: DateTime<Utc>, observed: DateTime<Utc>) -> String {
    let seconds = (now - observed).num_seconds().max(0);
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m", seconds / 60)
    } else if seconds < 172800 {
        format!("{}h", seconds / 3600)
    } else {
        format!("{}d", seconds / 86400)
    }
}

fn current_bundled_runtime_fingerprint(
    journal: &Path,
    config: &Map<String, Value>,
    nvidia_probe: Option<solstone_core_local::NvidiaProbe>,
) -> Option<String> {
    let configured = config
        .get("providers")
        .and_then(Value::as_object)
        .and_then(|providers| providers.get("active"))
        .and_then(Value::as_object)
        .and_then(|active| active.get("model"))
        .and_then(Value::as_str)
        .filter(|model| !model.trim().is_empty())
        .unwrap_or("local/qwen3.5-4b");
    let model_id = solstone_core_local::install::resolve_bundled_model_id(
        configured,
        cfg!(target_os = "macos"),
    );
    let readiness = bundled_runtime_readiness(journal, &model_id, nvidia_probe)?;
    let artifacts = readiness.get("artifacts")?.as_object()?;
    let model_path = artifacts.get("model_path")?.as_str()?;
    let backend = readiness
        .get("host")
        .and_then(Value::as_object)
        .and_then(|host| host.get("backend"))
        .and_then(Value::as_str)
        .unwrap_or("metal");
    let artifact_target = readiness
        .get("target")
        .and_then(Value::as_object)
        .and_then(|target| target.get("target_fingerprint_sha256"))
        .and_then(Value::as_str)
        .unwrap_or("");
    bundled_runtime_desired_fingerprint(
        backend,
        &model_id,
        artifact_target,
        artifacts.get("binary_path").and_then(Value::as_str),
        model_path,
        artifacts.get("projector_path").and_then(Value::as_str),
    )
    .ok()
    .map(|desired| desired.sha256)
}

fn bundled_runtime_readiness(
    journal: &Path,
    model_id: &str,
    nvidia_probe: Option<solstone_core_local::NvidiaProbe>,
) -> Option<Value> {
    let journal = journal.display().to_string();
    #[cfg(target_os = "macos")]
    {
        let _ = nvidia_probe;
        solstone_core_local::install::metal_candidate::inspect_present(&Map::from_iter([
            ("journal".into(), Value::String(journal)),
            ("model_id".into(), Value::String(model_id.to_owned())),
            ("backend".into(), Value::String("metal".into())),
        ]))
        .ok()
    }
    #[cfg(not(target_os = "macos"))]
    {
        let mut input = Map::from_iter([
            ("journal".into(), Value::String(journal)),
            ("model_id".into(), Value::String(model_id.to_owned())),
        ]);
        if let Some(probe) = nvidia_probe {
            input.insert("nvidia_probe".into(), serde_json::to_value(probe).ok()?);
        }
        Some(solstone_core_local::install::readiness::inspect_local_present(input))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn test_owner_view(state: &str, reason: Option<&str>, component: Option<&str>) -> OwnerView {
        OwnerView {
            aggregate_state: state.to_owned(),
            reason_code: reason.map(str::to_owned),
            lane: None,
            provider: None,
            model: None,
            fingerprint_sha256: None,
            failing_component: component.map(str::to_owned),
            observed_at: None,
            expires_at: None,
            path: None,
            checked_age: None,
            transient: None,
        }
    }

    #[test]
    fn owner_exit_code_table_and_transient_precedence_are_exact() {
        let cases = [
            ("ready", None, 0),
            ("blocked", Some("provider_key_missing"), 1),
            ("unhealthy", Some("provider_response_invalid"), 1),
            ("unknown", Some("brain_record_missing"), 2),
            ("checking", Some("brain_check_in_progress"), 2),
        ];
        for (state, reason, expected) in cases {
            assert_eq!(
                owner_exit_code(&test_owner_view(state, reason, None)),
                expected,
                "{state}"
            );
        }
        for transient in ["busy", "stale_expected_fingerprint", "lost_fence"] {
            assert_eq!(
                owner_exit_code(&transient_view(
                    transient,
                    &test_owner_view("checking", None, None)
                )),
                3,
                "{transient}"
            );
        }
    }

    #[test]
    fn owner_human_text_covers_identity_age_and_generic_forms() {
        let mut ready = test_owner_view("ready", None, None);
        ready.lane = Some("byo-cloud".to_owned());
        ready.provider = Some("openai".to_owned());
        ready.model = Some("gpt".to_owned());
        ready.checked_age = Some("59s".to_owned());
        assert_eq!(
            human_text(&ready),
            "Thinking ready: your own model openai/gpt, checked 59s ago"
        );

        ready.provider = None;
        ready.model = None;
        ready.checked_age = None;
        assert_eq!(human_text(&ready), "Thinking ready: your own model");
        ready.lane = None;
        assert_eq!(human_text(&ready), "Thinking ready: unknown");

        assert_eq!(
            human_text(&test_owner_view(
                "unhealthy",
                Some("provider_response_invalid"),
                Some("generate"),
            )),
            "Thinking unhealthy: the answer couldn't be read (generate)"
        );
        assert_eq!(
            human_text(&test_owner_view("unknown", None, None)),
            "Thinking unknown: ok"
        );
        assert_eq!(
            human_text(&test_owner_view(
                "checking",
                Some("brain_check_in_progress"),
                None
            )),
            "Thinking checking: checking now"
        );
        assert_eq!(
            human_text(&transient_view(
                "busy",
                &test_owner_view("checking", None, None)
            )),
            "Thinking busy: a check is already running"
        );
    }

    #[test]
    fn owner_checked_age_boundaries_match_the_oracle() {
        let now = Utc.with_ymd_and_hms(2026, 1, 2, 0, 0, 0).unwrap();
        for (seconds, expected) in [
            (0, "0s"),
            (59, "59s"),
            (60, "1m"),
            (3599, "59m"),
            (3600, "1h"),
            (172799, "47h"),
            (172800, "2d"),
        ] {
            assert_eq!(age(now, now - Duration::seconds(seconds)), expected);
        }
    }

    #[test]
    fn owner_provider_reasons_are_component_scoped_and_fail_closed() {
        assert_eq!(
            map_provider_reason("generate", Some("probe_output_starved")),
            "probe_output_starved"
        );
        assert_eq!(
            map_provider_reason("generate", Some("not-in-contract")),
            "probe_internal_error"
        );
        // The local endpoint wire's own names reach the contract's codes.
        assert_eq!(
            map_provider_reason("generate", Some("local_endpoint_unreachable")),
            "endpoint_unreachable"
        );
        assert_eq!(
            map_provider_reason("generate", Some("local_endpoint_contract_failed")),
            "endpoint_contract_failed"
        );
    }

    #[test]
    fn generate_client_failures_remain_contract_valid_owner_evidence() {
        let now = Utc.with_ymd_and_hms(2026, 1, 2, 0, 0, 0).unwrap();
        let resolution = ClientError::Resolve("missing sibling".to_owned());
        let reason = generate_client_failure_reason(&resolution);
        assert_eq!(reason, "probe_internal_error");
        let component = component_for_reason("generate", &reason, Map::new(), now);
        assert_eq!(component["reason_code"], "probe_internal_error");
        assert_eq!(component["diagnostic"], json!({}));

        let protocol = ClientError::Protocol(Box::new(solstone_core_generate::ProtocolFailure {
            error: solstone_core_generate::ProtocolError {
                id: None,
                reason: "provider_key_invalid".to_owned(),
                detail: "bounded provider diagnostic".to_owned(),
            },
            status: solstone_core_generate::ChildStatus {
                exit_code: Some(70),
                signal: None,
            },
            stdout: solstone_core_generate::CapturedStream::empty(),
            stderr: solstone_core_generate::CapturedStream::empty(),
            stdin_closed_early: false,
        }));
        assert_eq!(
            generate_client_failure_reason(&protocol),
            "provider_key_invalid"
        );
    }

    fn generated_response(
        text: &str,
        finish_reason: &str,
        usage: Value,
        thinking: Option<Value>,
    ) -> GeneratedResponse {
        GeneratedResponse {
            id: None,
            text: text.to_owned(),
            model: "test-model".to_owned(),
            usage,
            finish_reason: finish_reason.to_owned(),
            thinking,
            schema_validation: None,
            input_budget: None,
            request_budget: None,
            inference: None,
            hints_applied: Vec::new(),
        }
    }

    #[test]
    fn owner_generate_classification_matches_the_canned_oracle() {
        let cases = [
            (
                "OK",
                "max_tokens",
                json!({}),
                None,
                Some("probe_output_starved"),
            ),
            ("OK", "stop", json!({}), None, None),
            (
                "",
                "stop",
                json!({}),
                None,
                Some("provider_response_invalid"),
            ),
            (
                "",
                "stop",
                json!({"reasoning_tokens": 1}),
                None,
                Some("probe_output_starved"),
            ),
            (
                "",
                "stop",
                json!({}),
                Some(json!([{"summary":"reasoned"}])),
                Some("probe_output_starved"),
            ),
            ("", "unknown", json!({}), None, Some("probe_output_starved")),
        ];
        for (text, finish_reason, usage, thinking, expected) in cases {
            let response = generated_response(text, finish_reason, usage, thinking);
            assert_eq!(classify_canned_generate(&response), expected);
        }
    }

    #[test]
    fn owner_probe_reason_vocabulary_is_closed() {
        for reason in [
            "brain_refresh_timeout",
            "chatgpt_not_eligible",
            "chatgpt_sign_in_required",
            "chatgpt_usage_limit",
            "endpoint_contract_failed",
            "endpoint_unreachable",
            "local_server_unhealthy",
            "model_not_found",
            "network_unreachable",
            "provider_key_invalid",
            "provider_quota_exceeded",
            "provider_request_rejected",
            "provider_response_invalid",
            "provider_unavailable",
            "confidential_access_ended",
        ] {
            assert_eq!(map_provider_reason("generate", Some(reason)), reason);
        }
        assert_eq!(
            map_provider_reason("generate", Some("probe_output_starved")),
            "probe_output_starved"
        );
    }

    #[test]
    fn bundled_runtime_diagnostics_only_carry_contract_fields() {
        let diagnostic = runtime_diagnostic(
            "local_runtime_not_ready",
            Some("starting"),
            Some("runtime_starting"),
        );
        assert_eq!(diagnostic["phase"], "starting");
        assert_eq!(diagnostic["runtime_reason"], "runtime_starting");

        let mismatch = runtime_diagnostic(
            "local_runtime_fingerprint_mismatch",
            Some("ready"),
            Some("runtime_ready"),
        );
        assert_eq!(mismatch["phase"], "ready");
        assert!(!mismatch.contains_key("runtime_reason"));
    }

    #[cfg(all(test, feature = "full-tests"))]
    mod full {
        use super::*;
        use std::path::PathBuf;
        use std::sync::{Mutex, mpsc};

        struct PanicSpp;
        impl OwnerSppPrerequisite for PanicSpp {
            fn assess(
                &self,
                _journal: &Path,
                _config: &Map<String, Value>,
                _now: DateTime<Utc>,
            ) -> Value {
                panic!("SPP assess must not be called");
            }
        }

        struct TestProbe<F> {
            fingerprint: String,
            generate_fn: Mutex<F>,
            captured_requests: Mutex<Vec<GenerateRequest>>,
        }

        impl<F> TestProbe<F>
        where
            F: FnMut(
                &GenerateRequest,
                &Path,
                &Map<String, Value>,
            ) -> Result<GenerateResponse, ClientError>,
        {
            fn new(fingerprint: String, generate_fn: F) -> Self {
                Self {
                    fingerprint,
                    generate_fn: Mutex::new(generate_fn),
                    captured_requests: Mutex::new(Vec::new()),
                }
            }
        }

        impl<F> OwnerProbe for TestProbe<F>
        where
            F: FnMut(
                &GenerateRequest,
                &Path,
                &Map<String, Value>,
            ) -> Result<GenerateResponse, ClientError>,
        {
            fn bundled_runtime_fingerprint(
                &self,
                _journal: &Path,
                _config: &Map<String, Value>,
            ) -> Option<String> {
                Some(self.fingerprint.clone())
            }

            fn assess_bundled_runtime(
                &self,
                journal: &Path,
                expected: Option<&str>,
            ) -> BundledRuntimePrerequisiteAssessment {
                assess_bundled_runtime_prerequisite(journal, expected)
            }

            fn execute_generate(
                &self,
                request: &GenerateRequest,
                journal: &Path,
                config: &Map<String, Value>,
            ) -> Result<GenerateResponse, ClientError> {
                self.captured_requests.lock().unwrap().push(request.clone());
                (self.generate_fn.lock().unwrap())(request, journal, config)
            }
        }

        fn setup_temp_journal() -> tempfile::TempDir {
            let temp = tempfile::tempdir().expect("temp dir");
            std::fs::create_dir_all(temp.path().join("config")).expect("config dir");
            std::fs::write(
                temp.path().join("config/journal.json"),
                br#"{"providers":{"active":{"provider":"local"}}}"#,
            )
            .expect("config write");
            temp
        }

        fn write_runtime_file(journal: &Path, phase: &str, desired_fingerprint: &str) {
            let runtime_dir = journal.join("health/providers/runtime");
            std::fs::create_dir_all(&runtime_dir).expect("runtime dir");
            std::fs::write(
                runtime_dir.join("local.json"),
                serde_json::to_vec(&json!({
                    "phase": phase,
                    "desired_fingerprint_sha256": desired_fingerprint,
                }))
                .expect("serialize runtime"),
            )
            .expect("write runtime");
        }

        #[test]
        fn owner_refresh_bundled_success_converges_to_ready() {
            let temp = setup_temp_journal();
            let fingerprint = "a".repeat(64);
            write_runtime_file(temp.path(), "ready", &fingerprint);
            let probe = TestProbe::new(fingerprint.clone(), |_, _, _| {
                Ok(GenerateResponse::Generated(Box::new(generated_response(
                    "OK",
                    "stop",
                    json!({}),
                    None,
                ))))
            });
            let outcome = refresh(
                temp.path(),
                &OwnerRefreshOptions {
                    expected_fingerprint: None,
                    expected_active_fingerprint: false,
                    expect_active_fingerprint_absent: false,
                },
                &probe,
                &PanicSpp,
            );
            assert!(
                matches!(outcome, OwnerRefreshOutcome::PersistedReady { ref view } if owner_exit_code(view) == 0)
            );

            let config = read_journal_config(temp.path()).unwrap().config.unwrap();
            let inspection = inspect_brain_state(temp.path(), &config, Utc::now());
            assert_eq!(inspection.projection.aggregate_state, "ready");
            assert_eq!(inspection.projection.reason_code, None);
            assert!(inspection.projection.fingerprint_sha256.is_some());

            let record_val = inspection.record.expect("record exists");
            assert_eq!(record_val["reason_code"], serde_json::Value::Null);
            assert_eq!(record_val["checking"], serde_json::Value::Null);
            solstone_core_brain::validate_brain_state_record(&record_val, Utc::now())
                .expect("record validates");

            let captured = probe.captured_requests.lock().unwrap();
            assert_eq!(captured.len(), 1);
            assert_eq!(captured[0].timeout_s, Some(30.0));
            assert_eq!(captured[0].transport_retries, Some(0));
        }

        #[test]
        fn owner_refresh_bundled_prerequisite_failures_skip_generate_and_stay_non_ready() {
            let failure_cases = [
                (None, "local_runtime_not_ready"),
                (
                    Some(("ready", "c".repeat(64))),
                    "local_runtime_fingerprint_mismatch",
                ),
                (
                    Some(("starting", "a".repeat(64))),
                    "local_runtime_not_ready",
                ),
            ];

            for (runtime, expected_reason) in failure_cases {
                let temp = setup_temp_journal();
                let fingerprint = "a".repeat(64);
                if let Some((phase, desired)) = runtime {
                    write_runtime_file(temp.path(), phase, &desired);
                }

                let imports_dir = temp.path().join("imports");
                std::fs::create_dir_all(&imports_dir).unwrap();
                std::fs::write(imports_dir.join("planted.txt"), b"arbitrary-import-data").unwrap();

                let assessment =
                    assess_bundled_runtime_prerequisite(temp.path(), Some(&fingerprint));
                assert_eq!(assessment.reason_code.as_deref(), Some(expected_reason));

                let probe = TestProbe::new(
                    fingerprint.clone(),
                    |_, _, _| -> Result<GenerateResponse, ClientError> {
                        panic!("generate must not be called on bundled prerequisite failure");
                    },
                );

                let outcome = refresh(
                    temp.path(),
                    &OwnerRefreshOptions {
                        expected_fingerprint: None,
                        expected_active_fingerprint: false,
                        expect_active_fingerprint_absent: false,
                    },
                    &probe,
                    &PanicSpp,
                );
                assert!(
                    matches!(outcome, OwnerRefreshOutcome::PersistedNonReady { ref view, .. } if owner_exit_code(view) != 0)
                );

                let config = read_journal_config(temp.path()).unwrap().config.unwrap();
                let inspection = inspect_brain_state(temp.path(), &config, Utc::now());
                assert_ne!(inspection.projection.aggregate_state, "ready");
                let captured = probe.captured_requests.lock().unwrap();
                assert!(captured.is_empty());
            }
        }

        #[test]
        fn owner_refresh_generate_failure_records_unhealthy_or_blocked() {
            let temp = setup_temp_journal();
            let fingerprint = "a".repeat(64);
            write_runtime_file(temp.path(), "ready", &fingerprint);
            let probe = TestProbe::new(
                fingerprint.clone(),
                |_, _, _| -> Result<GenerateResponse, ClientError> {
                    Ok(GenerateResponse::Generated(Box::new(generated_response(
                        "",
                        "stop",
                        json!({}),
                        None,
                    ))))
                },
            );
            let outcome = refresh(
                temp.path(),
                &OwnerRefreshOptions {
                    expected_fingerprint: None,
                    expected_active_fingerprint: false,
                    expect_active_fingerprint_absent: false,
                },
                &probe,
                &PanicSpp,
            );
            assert!(
                matches!(outcome, OwnerRefreshOutcome::PersistedNonReady { ref view, .. } if owner_exit_code(view) == 1)
            );

            let config = read_journal_config(temp.path()).unwrap().config.unwrap();
            let inspection = inspect_brain_state(temp.path(), &config, Utc::now());
            assert!(
                matches!(
                    inspection.projection.aggregate_state.as_str(),
                    "unhealthy" | "blocked"
                ),
                "state: {}",
                inspection.projection.aggregate_state
            );
            assert_ne!(inspection.projection.aggregate_state, "ready");
        }

        #[test]
        fn owner_refresh_stale_expected_fingerprint_short_circuits() {
            let temp = setup_temp_journal();
            let fingerprint = "a".repeat(64);
            write_runtime_file(temp.path(), "ready", &fingerprint);
            let probe = TestProbe::new(fingerprint.clone(), |_, _, _| {
                Ok(GenerateResponse::Generated(Box::new(generated_response(
                    "OK",
                    "stop",
                    json!({}),
                    None,
                ))))
            });
            let outcome1 = refresh(
                temp.path(),
                &OwnerRefreshOptions {
                    expected_fingerprint: None,
                    expected_active_fingerprint: false,
                    expect_active_fingerprint_absent: false,
                },
                &probe,
                &PanicSpp,
            );
            assert!(
                matches!(outcome1, OwnerRefreshOutcome::PersistedReady { ref view } if owner_exit_code(view) == 0)
            );

            let brain_record_path = temp.path().join("health/brain.json");
            let initial_bytes = std::fs::read(&brain_record_path).expect("read brain.json");

            let stale_probe = TestProbe::new(
                fingerprint.clone(),
                |_, _, _| -> Result<GenerateResponse, ClientError> {
                    panic!("generate must not be called on stale fingerprint");
                },
            );
            let outcome2 = refresh(
                temp.path(),
                &OwnerRefreshOptions {
                    expected_fingerprint: Some("b".repeat(64)),
                    expected_active_fingerprint: false,
                    expect_active_fingerprint_absent: false,
                },
                &stale_probe,
                &PanicSpp,
            );
            assert!(
                matches!(outcome2, OwnerRefreshOutcome::StaleExpectedFingerprint { ref view } if owner_exit_code(view) == 3)
            );

            let after_bytes = std::fs::read(&brain_record_path).expect("read brain.json after");
            assert_eq!(initial_bytes, after_bytes);
            assert!(stale_probe.captured_requests.lock().unwrap().is_empty());
        }

        #[test]
        fn snapshot_fence_mismatch_skips_probe_and_returns_lost_fence() {
            struct MismatchProbe {
                fingerprint: String,
                rewrite_bytes: &'static [u8],
            }
            impl OwnerProbe for MismatchProbe {
                fn bundled_runtime_fingerprint(
                    &self,
                    _: &Path,
                    _: &Map<String, Value>,
                ) -> Option<String> {
                    Some(self.fingerprint.clone())
                }
                fn assess_bundled_runtime(
                    &self,
                    journal: &Path,
                    expected: Option<&str>,
                ) -> BundledRuntimePrerequisiteAssessment {
                    assess_bundled_runtime_prerequisite(journal, expected)
                }
                fn execute_generate(
                    &self,
                    _: &GenerateRequest,
                    _: &Path,
                    _: &Map<String, Value>,
                ) -> Result<GenerateResponse, ClientError> {
                    panic!("generate must not be called on fence mismatch");
                }
                fn after_permit_acquired(&self, journal: &Path) {
                    std::fs::write(journal.join("config/journal.json"), self.rewrite_bytes)
                        .unwrap();
                }
            }

            let temp = setup_temp_journal();
            let fingerprint = "a".repeat(64);
            write_runtime_file(temp.path(), "ready", &fingerprint);

            let probe = MismatchProbe {
                fingerprint: fingerprint.clone(),
                rewrite_bytes:
                    br#"{"providers":{"active":{"provider":"google","model":"gemini-1.5-flash"}}}"#,
            };
            let outcome = refresh(
                temp.path(),
                &OwnerRefreshOptions {
                    expected_fingerprint: None,
                    expected_active_fingerprint: false,
                    expect_active_fingerprint_absent: false,
                },
                &probe,
                &PanicSpp,
            );

            assert!(
                matches!(outcome, OwnerRefreshOutcome::LostFence { ref view } if owner_exit_code(view) == 3)
            );

            let config = read_journal_config(temp.path()).unwrap().config.unwrap();
            let inspection = inspect_brain_state(temp.path(), &config, Utc::now());
            assert_ne!(inspection.projection.aggregate_state, "ready");
            assert!(!probe_file_lease_held(&brain_refresh_lease_path(temp.path())).unwrap());

            let temp2 = setup_temp_journal();
            write_runtime_file(temp2.path(), "ready", &fingerprint);

            let probe2 = MismatchProbe {
                fingerprint,
                rewrite_bytes: br#"{"providers":{"active":{"provider":"not-a-provider"}}}"#,
            };
            let outcome2 = refresh(
                temp2.path(),
                &OwnerRefreshOptions {
                    expected_fingerprint: None,
                    expected_active_fingerprint: false,
                    expect_active_fingerprint_absent: false,
                },
                &probe2,
                &PanicSpp,
            );

            assert!(
                matches!(outcome2, OwnerRefreshOutcome::LostFence { ref view } if owner_exit_code(view) == 3)
            );

            let brain_record_bytes = std::fs::read(temp2.path().join("health/brain.json"))
                .expect("read health/brain.json");
            let brain_record: Value =
                serde_json::from_slice(&brain_record_bytes).expect("parse health/brain.json");
            assert!(brain_record.get("checking").is_some());
            assert!(!brain_record["checking"].is_null());
            assert_ne!(
                brain_record.get("reason_code").and_then(Value::as_str),
                Some("probe_internal_error")
            );
            assert!(!probe_file_lease_held(&brain_refresh_lease_path(temp2.path())).unwrap());
        }

        #[test]
        fn renewal_fallback_uses_the_caller_journal_and_executor() {
            let temp = setup_temp_journal();
            let fingerprint = "a".repeat(64);
            write_runtime_file(temp.path(), "ready", &fingerprint);

            struct CapturingProbe {
                fingerprint: String,
                captured_path: Mutex<Option<PathBuf>>,
            }
            impl OwnerProbe for CapturingProbe {
                fn bundled_runtime_fingerprint(
                    &self,
                    _: &Path,
                    _: &Map<String, Value>,
                ) -> Option<String> {
                    Some(self.fingerprint.clone())
                }
                fn assess_bundled_runtime(
                    &self,
                    journal: &Path,
                    expected: Option<&str>,
                ) -> BundledRuntimePrerequisiteAssessment {
                    assess_bundled_runtime_prerequisite(journal, expected)
                }
                fn execute_generate(
                    &self,
                    _: &GenerateRequest,
                    journal: &Path,
                    _: &Map<String, Value>,
                ) -> Result<GenerateResponse, ClientError> {
                    *self.captured_path.lock().unwrap() = Some(journal.to_path_buf());
                    Ok(GenerateResponse::Generated(Box::new(generated_response(
                        "OK",
                        "stop",
                        json!({}),
                        None,
                    ))))
                }
            }

            let probe = CapturingProbe {
                fingerprint,
                captured_path: Mutex::new(None),
            };

            let outcome = renew_prerequisites(temp.path(), None, &probe, &PanicSpp);
            assert!(
                matches!(outcome, OwnerRefreshOutcome::PersistedReady { ref view } if owner_exit_code(view) == 0)
            );
            assert_eq!(
                probe.captured_path.lock().unwrap().as_deref(),
                Some(temp.path())
            );
        }

        #[test]
        fn spp_refusal_skips_generate_and_stays_non_ready() {
            let temp = tempfile::tempdir().expect("temp dir");
            std::fs::create_dir_all(temp.path().join("config")).expect("config dir");
            let config_json = br#"{"providers":{"active":{"provider":"local","model":"local/qwen3.5-4b"},"local":{"endpoint_url":"https://example.invalid/v1","served_model_id":"model","credential":"secret"}},"services":{"confidential":{"endpoint_url":"https://example.invalid/v1","served_model_id":"model","credential_fingerprint_sha256":"2bb80d537b1da3e38bd30361aa855686bde0eacd7162fef6a25fe97bf527a25b"}}}"#;
            std::fs::write(temp.path().join("config/journal.json"), config_json)
                .expect("config write");

            let parsed: Map<String, Value> = serde_json::from_slice(config_json).unwrap();
            assert_eq!(
                derive_active_brain_lane(&parsed).lane.as_deref(),
                Some("spp")
            );

            struct RefusingSpp;
            impl OwnerSppPrerequisite for RefusingSpp {
                fn assess(&self, _: &Path, _: &Map<String, Value>, now: DateTime<Utc>) -> Value {
                    component_for_reason(
                        "lane_prerequisites",
                        "attestation_not_verified",
                        Map::new(),
                        now,
                    )
                }
            }

            struct NoGenerateProbe;
            impl OwnerProbe for NoGenerateProbe {
                fn bundled_runtime_fingerprint(
                    &self,
                    _: &Path,
                    _: &Map<String, Value>,
                ) -> Option<String> {
                    None
                }
                fn assess_bundled_runtime(
                    &self,
                    _: &Path,
                    _: Option<&str>,
                ) -> BundledRuntimePrerequisiteAssessment {
                    unreachable!()
                }
                fn execute_generate(
                    &self,
                    _: &GenerateRequest,
                    _: &Path,
                    _: &Map<String, Value>,
                ) -> Result<GenerateResponse, ClientError> {
                    panic!("generate must not be called when SPP prerequisites refuse");
                }
            }

            let outcome = refresh(
                temp.path(),
                &OwnerRefreshOptions {
                    expected_fingerprint: None,
                    expected_active_fingerprint: false,
                    expect_active_fingerprint_absent: false,
                },
                &NoGenerateProbe,
                &RefusingSpp,
            );
            assert!(
                matches!(outcome, OwnerRefreshOutcome::PersistedNonReady { ref view, .. } if view.aggregate_state != "ready")
            );
        }

        #[test]
        fn paused_executor_holds_the_lease_until_return() {
            let temp = setup_temp_journal();
            let fingerprint = "a".repeat(64);
            write_runtime_file(temp.path(), "ready", &fingerprint);

            let (entered_tx, entered_rx) = mpsc::channel();
            let (release_tx, release_rx) = mpsc::channel();
            let release_rx = Mutex::new(release_rx);

            let probe = TestProbe::new(fingerprint.clone(), move |_, _, _| {
                entered_tx.send(()).unwrap();
                release_rx.lock().unwrap().recv().unwrap();
                Ok(GenerateResponse::Generated(Box::new(generated_response(
                    "OK",
                    "stop",
                    json!({}),
                    None,
                ))))
            });

            let journal_path = temp.path().to_path_buf();
            let (result_tx, result_rx) = mpsc::channel();
            let handle = std::thread::spawn(move || {
                let outcome = refresh(
                    &journal_path,
                    &OwnerRefreshOptions {
                        expected_fingerprint: None,
                        expected_active_fingerprint: false,
                        expect_active_fingerprint_absent: false,
                    },
                    &probe,
                    &PanicSpp,
                );
                // This is the actual worker result observer, not an unrelated channel.
                assert!(result_tx.send(outcome.clone()).is_err());
                outcome
            });

            entered_rx.recv_timeout(StdDuration::from_secs(10)).unwrap();
            drop(result_rx);
            let held = probe_file_lease_held(&brain_refresh_lease_path(temp.path())).unwrap();
            let competing = TestProbe::new(fingerprint, |_, _, _| {
                panic!("a competing refresh must not execute while the worker owns the lease")
            });
            let busy = refresh(
                temp.path(),
                &OwnerRefreshOptions {
                    expected_fingerprint: None,
                    expected_active_fingerprint: false,
                    expect_active_fingerprint_absent: false,
                },
                &competing,
                &PanicSpp,
            );

            release_tx.send(()).unwrap();
            let outcome = handle.join().unwrap();
            assert!(held);
            assert!(matches!(busy, OwnerRefreshOutcome::Busy { .. }));
            assert!(
                matches!(outcome, OwnerRefreshOutcome::PersistedReady { ref view } if owner_exit_code(view) == 0)
            );
            assert!(!probe_file_lease_held(&brain_refresh_lease_path(temp.path())).unwrap());
        }

        #[test]
        fn unwind_releases_the_lease_and_does_not_publish_ready() {
            let temp = setup_temp_journal();
            let fingerprint = "a".repeat(64);
            write_runtime_file(temp.path(), "ready", &fingerprint);

            struct PanickingProbe {
                fingerprint: String,
            }
            impl OwnerProbe for PanickingProbe {
                fn bundled_runtime_fingerprint(
                    &self,
                    _: &Path,
                    _: &Map<String, Value>,
                ) -> Option<String> {
                    Some(self.fingerprint.clone())
                }
                fn assess_bundled_runtime(
                    &self,
                    journal: &Path,
                    expected: Option<&str>,
                ) -> BundledRuntimePrerequisiteAssessment {
                    assess_bundled_runtime_prerequisite(journal, expected)
                }
                fn execute_generate(
                    &self,
                    _: &GenerateRequest,
                    _: &Path,
                    _: &Map<String, Value>,
                ) -> Result<GenerateResponse, ClientError> {
                    panic!("intentional panic inside generate");
                }
            }

            let probe = PanickingProbe { fingerprint };
            let journal_path = temp.path().to_path_buf();
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                refresh(
                    &journal_path,
                    &OwnerRefreshOptions {
                        expected_fingerprint: None,
                        expected_active_fingerprint: false,
                        expect_active_fingerprint_absent: false,
                    },
                    &probe,
                    &PanicSpp,
                )
            }));

            assert!(!probe_file_lease_held(&brain_refresh_lease_path(temp.path())).unwrap());

            let config = read_journal_config(temp.path()).unwrap().config.unwrap();
            let inspection = inspect_brain_state(temp.path(), &config, Utc::now());
            assert_ne!(inspection.projection.aggregate_state, "ready");
        }

        #[test]
        fn respond_with_config_probe_uses_the_snapshot_not_the_file() {
            let temp = tempfile::tempdir().expect("temp dir");
            std::fs::create_dir_all(temp.path().join("config")).expect("config dir");
            std::fs::write(
                temp.path().join("config/journal.json"),
                br#"{"providers":{"active":{"provider":"unknown"}}}"#,
            )
            .expect("write config");

            let probe = RespondWithConfigProbe {
                runtime: solstone_core_generate_wire::EndpointRuntime::default(),
            };

            let request = GenerateRequest {
                id: None,
                context: HEALTH_BRAIN_GENERATE_CONTEXT.to_owned(),
                contents: vec![ContentPart::Text {
                    text: "Reply with the single word OK.".to_owned(),
                }],
                system_instruction: None,
                temperature: 0.0,
                max_output_tokens: 512,
                timeout_s: Some(30.0),
                json_output: false,
                json_schema: None,
                attempt_index: 0,
                exclusive_admission: false,
                transport_retries: Some(0),
            };

            let snapshot = json!({
                "providers": {
                    "active": {
                        "provider": "google"
                    }
                }
            })
            .as_object()
            .unwrap()
            .clone();

            let result = probe.execute_generate(&request, temp.path(), &snapshot);
            match result {
                Ok(GenerateResponse::Refused(refusal)) => {
                    assert_eq!(refusal.provider.as_deref(), Some("google"));
                    assert_eq!(
                        refusal.reason_code.as_ref().map(|r| r.as_wire()),
                        Some("provider_key_missing")
                    );
                }
                other => panic!("expected refused provider_key_missing, got {other:?}"),
            }
        }

        #[test]
        fn callable_safe_renewal_reuses_the_existing_generate_evidence() {
            let temp = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(temp.path().join("config")).unwrap();
            let config = json!({"providers":{"active":{"provider":"local","model":"local/qwen3.5-4b"},"local":{"endpoint_url":"https://example.invalid/v1","served_model_id":"model","credential":"secret"}},"services":{"confidential":{"endpoint_url":"https://example.invalid/v1","served_model_id":"model","credential_fingerprint_sha256":"2bb80d537b1da3e38bd30361aa855686bde0eacd7162fef6a25fe97bf527a25b"}}});
            std::fs::write(
                temp.path().join("config/journal.json"),
                serde_json::to_vec(&config).unwrap(),
            )
            .unwrap();
            let now = Utc::now();
            let component = json!({"status":"ok","observed_at":now.to_rfc3339(),"expires_at":(now + Duration::days(1)).to_rfc3339()});
            let permit = begin_refresh(temp.path(), now, None, None, false, None)
                .unwrap()
                .unwrap();
            finish_refresh(temp.path(),permit,json!({"configuration":component,"lane_prerequisites":component,"generate":component}),Utc::now(),None).unwrap();
            let before: Value =
                serde_json::from_slice(&std::fs::read(brain_state_path(temp.path())).unwrap())
                    .unwrap();
            let view = owner_view(temp.path(), config.as_object().unwrap(), Utc::now());
            assert_eq!(view.aggregate_state, "ready");
            struct ReadySpp;
            impl OwnerSppPrerequisite for ReadySpp {
                fn assess(&self, _: &Path, _: &Map<String, Value>, now: DateTime<Utc>) -> Value {
                    json!({"status":"ok","observed_at":now.to_rfc3339(),"expires_at":(now + Duration::days(1)).to_rfc3339()})
                }
            }
            let probe = TestProbe::new("a".repeat(64), |_, _, _| {
                panic!("safe prerequisite renewal must not execute Generate")
            });
            let result = renew_prerequisites(
                temp.path(),
                view.fingerprint_sha256.as_deref(),
                &probe,
                &ReadySpp,
            );
            assert!(matches!(result, OwnerRefreshOutcome::PersistedReady { .. }));
            let after: Value =
                serde_json::from_slice(&std::fs::read(brain_state_path(temp.path())).unwrap())
                    .unwrap();
            assert_eq!(
                after["evidence"]["generate"],
                before["evidence"]["generate"]
            );
            assert_eq!(
                after["evidence"]["configuration"],
                before["evidence"]["configuration"]
            );
            assert!(probe.captured_requests.lock().unwrap().is_empty());
            assert!(!probe_file_lease_held(&brain_refresh_lease_path(temp.path())).unwrap());
        }

        #[test]
        fn callable_snapshot_aba_ignores_ambient_provider_overrides() {
            let temp = tempfile::tempdir().unwrap();
            let receipt = temp.path().join("child-receipt");
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "tests::full::snapshot_aba_ambient_child",
                    "--ignored",
                ])
                .env("BRAIN_OWNER_SNAPSHOT_RECEIPT", &receipt)
                .env(
                    "SOLSTONE_GENERATE_PROVIDER_OVERRIDE",
                    "unsupported-ambient-provider",
                )
                .env("SOLSTONE_GENERATE_MODEL_OVERRIDE", "ambient-model")
                .env(
                    "SOLSTONE_GENERATE_API_KEY_OVERRIDE",
                    "synthetic-ambient-key",
                )
                .env("SOLSTONE_GENERATE_BASE_URL_OVERRIDE", "http://127.0.0.1:9")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "child stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            // A misspelled exact test name may exit zero with no test executed.
            assert_eq!(
                std::fs::read(receipt).unwrap(),
                b"snapshot-aba-ambient-passed"
            );
        }

        #[test]
        #[ignore = "run by callable_snapshot_aba_ignores_ambient_provider_overrides in its isolated process"]
        fn snapshot_aba_ambient_child() {
            assert_eq!(
                std::env::var("SOLSTONE_GENERATE_PROVIDER_OVERRIDE").unwrap(),
                "unsupported-ambient-provider"
            );
            let receipt = std::env::var_os("BRAIN_OWNER_SNAPSHOT_RECEIPT").expect("parent receipt");
            let temp = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(temp.path().join("config")).unwrap();
            let a = json!({"providers":{"active":{"provider":"google","model":"snapshot-model"}}});
            let b = json!({"providers":{"active":{"provider":"openai","model":"file-model"}}});
            let file = temp.path().join("config/journal.json");
            std::fs::write(&file, serde_json::to_vec(&a).unwrap()).unwrap();
            let snapshot = read_journal_config(temp.path()).unwrap().config.unwrap();
            let probe = RespondWithConfigProbe {
                runtime: solstone_core_generate_wire::EndpointRuntime::default(),
            };
            let request = GenerateRequest {
                id: None,
                context: HEALTH_BRAIN_GENERATE_CONTEXT.to_owned(),
                contents: vec![ContentPart::Text {
                    text: "Reply with the single word OK.".to_owned(),
                }],
                system_instruction: None,
                temperature: 0.0,
                max_output_tokens: 512,
                timeout_s: Some(0.25),
                json_output: false,
                json_schema: None,
                attempt_index: 0,
                exclusive_admission: false,
                transport_retries: Some(0),
            };
            for file_config in [&a, &b, &a] {
                std::fs::write(&file, serde_json::to_vec(file_config).unwrap()).unwrap();
                match probe.execute_generate(&request, temp.path(), &snapshot) {
                    Ok(GenerateResponse::Refused(refusal)) => {
                        assert_eq!(refusal.provider.as_deref(), Some("google"));
                        assert_eq!(
                            refusal.reason_code.as_ref().map(|reason| reason.as_wire()),
                            Some("provider_key_missing")
                        );
                    }
                    other => panic!("checked snapshot lost: {other:?}"),
                }
            }
            std::fs::write(receipt, b"snapshot-aba-ambient-passed").unwrap();
        }
    }
}
