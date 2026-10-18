// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Bind a provider authentication rejection to the credential that was sent.

use std::path::Path;

use serde_json::{Map, Value};
use solstone_core_brain::{
    StoredCredentialBinding, attach_credential_binding, classify_stored_binding,
    endpoint_credential_binding_hmac, generate_fingerprint_key, load_existing_fingerprint_key,
    managed_credential_binding_hmac,
};
use solstone_core_generate::{ContentPart, GenerateRequest, HEALTH_BRAIN_GENERATE_CONTEXT};
use solstone_core_journal_config_write::{JournalConfigMutation, mutate_journal_config};
use solstone_core_journal_io::{JournalRoot, ObjectIdentity};
use solstone_core_local::endpoint::{LocalEndpointResolution, resolve_local_endpoint};

/// Admission and retain context for one dispatch call.
///
/// `None` at a provider lookup means the call is journal-free: no admission and no retain.
pub(crate) struct ContentGate<'a> {
    pub journal: &'a Path,
    pub admit_content: bool,
    pub cancellation: &'a solstone_core_generate::Cancellation<'a>,
}

pub(crate) struct RootWitness {
    root: JournalRoot,
    identity: ObjectIdentity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ManagedAdmission {
    MissingKey,
    Rejected,
    Allow,
}

const CANNED_TEXT: &str = "Reply with the single word OK.";

pub(crate) fn canned_probe(request: &GenerateRequest) -> bool {
    let context_matches = request.context == "settings.cloud.validate_key"
        || request.context == HEALTH_BRAIN_GENERATE_CONTEXT;
    context_matches
        && request.contents.len() == 1
        && matches!(
            &request.contents[0],
            ContentPart::Text { text } if text == CANNED_TEXT
        )
        && request.system_instruction.is_none()
        && request.json_schema.is_none()
        && !request.json_output
        && request.temperature == 0.0
        && request.max_output_tokens == 512
        && request.transport_retries == Some(0)
        && request.attempt_index == 0
        && !request.exclusive_admission
}

pub(crate) fn managed_admission(
    credential: Option<&str>,
    request: &GenerateRequest,
    admit_content: bool,
    binding: StoredCredentialBinding,
) -> ManagedAdmission {
    if credential.is_none() {
        return ManagedAdmission::MissingKey;
    }
    if admit_content && !canned_probe(request) && refuses_content(binding) {
        ManagedAdmission::Rejected
    } else {
        ManagedAdmission::Allow
    }
}

pub(crate) fn endpoint_admission_refuses(
    request: &GenerateRequest,
    admit_content: bool,
    binding: StoredCredentialBinding,
) -> bool {
    admit_content && !canned_probe(request) && refuses_content(binding)
}

fn refuses_content(binding: StoredCredentialBinding) -> bool {
    matches!(
        binding,
        StoredCredentialBinding::Matches | StoredCredentialBinding::Corrupt
    )
}

pub(crate) fn classify_managed_binding(
    journal: &Path,
    config: &Map<String, Value>,
    provider: &str,
    credential: &str,
) -> StoredCredentialBinding {
    let key = load_existing_fingerprint_key(journal);
    let expected = key
        .as_ref()
        .map(|key| managed_credential_binding_hmac(key, provider, credential));
    classify_stored_binding(
        stored_validation(config, provider),
        key.as_ref(),
        expected.as_deref(),
    )
}

pub(crate) fn classify_endpoint_binding(
    journal: &Path,
    config: &Map<String, Value>,
    base_url: &str,
    credential_or_empty: &str,
) -> StoredCredentialBinding {
    let key = load_existing_fingerprint_key(journal);
    let expected = key
        .as_ref()
        .map(|key| endpoint_credential_binding_hmac(key, base_url, credential_or_empty));
    classify_stored_binding(
        stored_validation(config, "local"),
        key.as_ref(),
        expected.as_deref(),
    )
}

/// Whether the final HTTP result is stored as `provider_key_invalid`.
///
/// A missing status is a connection failure and is not stored. This predicate does
/// not replace the live HTTP classifier.
pub(crate) fn retain_provider_key_rejection(
    provider: &str,
    status: Option<u16>,
    body: &str,
    credential_present: bool,
    confidential: bool,
    bundled: bool,
) -> bool {
    let Some(status) = status else {
        return false;
    };
    match provider {
        "openai" | "anthropic" => status == 401,
        "google" => status == 401 || body.contains("API_KEY_INVALID"),
        "local" => status == 401 && credential_present && !confidential && !bundled,
        _ => false,
    }
}

pub(crate) fn capture_root_witness(journal: &Path) -> Option<RootWitness> {
    let root = JournalRoot::open(journal).ok()?;
    let identity = root.identity();
    Some(RootWitness { root, identity })
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn retain_managed_rejection(
    gate: &ContentGate<'_>,
    witness: Option<&RootWitness>,
    request: &GenerateRequest,
    provider: &str,
    env_var: &str,
    sent_secret: &str,
    status: u16,
    body: &str,
) {
    if !gate.admit_content || gate.cancellation.is_cancelled() || canned_probe(request) {
        return;
    }
    if !retain_provider_key_rejection(provider, Some(status), body, true, false, false) {
        return;
    }
    let Some(witness) = witness else {
        log::warn!("provider key rejection was not retained");
        return;
    };
    let Some(key) = binding_key(gate.journal) else {
        return;
    };
    let hmac = managed_credential_binding_hmac(&key, provider, sent_secret);
    retain_under_lock(gate, witness, provider, &hmac, |config| {
        file_active_provider(config) == provider
            && config_env_value(config, env_var).as_deref() == Some(sent_secret)
    });
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn retain_endpoint_rejection(
    gate: &ContentGate<'_>,
    witness: Option<&RootWitness>,
    request: &GenerateRequest,
    sent_base: &str,
    sent_credential: &str,
    status: u16,
    body: &str,
) {
    if !gate.admit_content || gate.cancellation.is_cancelled() || canned_probe(request) {
        return;
    }
    if !retain_provider_key_rejection("local", Some(status), body, true, false, false) {
        return;
    }
    let Some(witness) = witness else {
        log::warn!("provider key rejection was not retained");
        return;
    };
    let Some(key) = binding_key(gate.journal) else {
        return;
    };
    let hmac = endpoint_credential_binding_hmac(&key, sent_base, sent_credential);
    retain_under_lock(gate, witness, "local", &hmac, |config| {
        file_active_provider(config) == "local"
            && endpoint_still_sent(config, sent_base, sent_credential)
    });
}

fn binding_key(journal: &Path) -> Option<[u8; 32]> {
    if let Some(key) = load_existing_fingerprint_key(journal) {
        return Some(key);
    }
    match generate_fingerprint_key(journal) {
        Ok(key) => Some(key),
        Err(error) => {
            log::warn!("provider key rejection was not retained: {error}");
            None
        }
    }
}

fn retain_under_lock(
    gate: &ContentGate<'_>,
    witness: &RootWitness,
    provider: &str,
    hmac: &str,
    accept_config: impl FnOnce(&Map<String, Value>) -> bool,
) {
    let retained = mutate_journal_config(gate.journal, Default::default(), |config| {
        let accept = witness.root.revalidate_canonical_binding().is_ok()
            && witness.root.identity() == witness.identity
            && !gate.cancellation.is_cancelled()
            && accept_config(config);
        let changed = accept && store_rejection(config, provider, hmac);
        JournalConfigMutation { changed, value: () }
    });
    if let Err(error) = retained {
        log::warn!("provider key rejection was not retained: {error}");
    }
}

fn endpoint_still_sent(
    config: &Map<String, Value>,
    sent_base: &str,
    sent_credential: &str,
) -> bool {
    match resolve_local_endpoint(config) {
        LocalEndpointResolution::Byo(endpoint) => {
            !endpoint.is_confidential
                && !endpoint.is_bundled
                && endpoint.base_url == sent_base
                && endpoint.credential.as_deref() == Some(sent_credential)
        }
        LocalEndpointResolution::Bundled => false,
    }
}

fn store_rejection(config: &mut Map<String, Value>, provider: &str, hmac_hex: &str) -> bool {
    let mut entry = Map::new();
    entry.insert("valid".to_owned(), Value::Bool(false));
    entry.insert(
        "reason_code".to_owned(),
        Value::String("provider_key_invalid".to_owned()),
    );
    attach_credential_binding(&mut entry, hmac_hex);
    let next = Value::Object(entry);
    let validations = key_validation_map(config);
    if validations.get(provider) == Some(&next) {
        return false;
    }
    validations.insert(provider.to_owned(), next);
    true
}

fn key_validation_map(config: &mut Map<String, Value>) -> &mut Map<String, Value> {
    if !config.get("providers").is_some_and(Value::is_object) {
        config.insert("providers".to_owned(), Value::Object(Map::new()));
    }
    let providers = config
        .get_mut("providers")
        .and_then(Value::as_object_mut)
        .expect("providers object");
    if !providers
        .get("key_validation")
        .is_some_and(Value::is_object)
    {
        providers.insert("key_validation".to_owned(), Value::Object(Map::new()));
    }
    providers
        .get_mut("key_validation")
        .and_then(Value::as_object_mut)
        .expect("key_validation object")
}

fn stored_validation<'a>(config: &'a Map<String, Value>, provider: &str) -> Option<&'a Value> {
    config
        .get("providers")
        .and_then(Value::as_object)
        .and_then(|providers| providers.get("key_validation"))
        .and_then(Value::as_object)
        .and_then(|entries| entries.get(provider))
}

fn file_active_provider(config: &Map<String, Value>) -> String {
    config
        .get("providers")
        .and_then(Value::as_object)
        .and_then(|providers| providers.get("active"))
        .and_then(Value::as_object)
        .and_then(|active| active.get("provider"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("none")
        .to_owned()
}

fn config_env_value(config: &Map<String, Value>, env_var: &str) -> Option<String> {
    config
        .get("env")
        .and_then(Value::as_object)
        .and_then(|env| env.get(env_var))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn content_request() -> GenerateRequest {
        GenerateRequest {
            id: None,
            context: "segment.thought".to_owned(),
            contents: vec![ContentPart::Text {
                text: "canary-body".to_owned(),
            }],
            system_instruction: None,
            temperature: 0.2,
            max_output_tokens: 32,
            timeout_s: Some(5.0),
            json_output: false,
            json_schema: None,
            attempt_index: 0,
            exclusive_admission: false,
            transport_retries: Some(0),
        }
    }

    fn canned_request(context: &str) -> GenerateRequest {
        GenerateRequest {
            id: None,
            context: context.to_owned(),
            contents: vec![ContentPart::Text {
                text: CANNED_TEXT.to_owned(),
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
        }
    }

    #[test]
    fn retain_openai_and_anthropic_401_only() {
        for provider in ["openai", "anthropic"] {
            assert!(retain_provider_key_rejection(
                provider,
                Some(401),
                "{}",
                true,
                false,
                false
            ));
            assert!(!retain_provider_key_rejection(
                provider,
                Some(403),
                "{}",
                true,
                false,
                false
            ));
            assert!(!retain_provider_key_rejection(
                provider,
                Some(403),
                "API_KEY_INVALID",
                true,
                false,
                false
            ));
            assert!(!retain_provider_key_rejection(
                provider, None, "", true, false, false
            ));
        }
    }

    #[test]
    fn retain_google_401_and_api_key_invalid_marker() {
        assert!(retain_provider_key_rejection(
            "google",
            Some(401),
            "{}",
            true,
            false,
            false
        ));
        assert!(retain_provider_key_rejection(
            "google",
            Some(400),
            "{\"error\":\"API_KEY_INVALID\"}",
            true,
            false,
            false
        ));
        assert!(retain_provider_key_rejection(
            "google",
            Some(403),
            "API_KEY_INVALID",
            true,
            false,
            false
        ));
    }

    #[test]
    fn retain_skips_opaque_google_403_quota_network_and_5xx() {
        assert_eq!(
            crate::google::classify_http_failure(403, "{}"),
            "provider_key_invalid"
        );
        assert!(!retain_provider_key_rejection(
            "google",
            Some(403),
            "{}",
            true,
            false,
            false
        ));
        assert!(!retain_provider_key_rejection(
            "google",
            Some(429),
            "{}",
            true,
            false,
            false
        ));
        assert!(!retain_provider_key_rejection(
            "google",
            Some(503),
            "{}",
            true,
            false,
            false
        ));
        assert!(!retain_provider_key_rejection(
            "openai",
            Some(500),
            "{}",
            true,
            false,
            false
        ));
        assert!(!retain_provider_key_rejection(
            "google", None, "", true, false, false
        ));
        assert!(!retain_provider_key_rejection(
            "local",
            Some(401),
            "{}",
            false,
            false,
            false
        ));
        assert!(!retain_provider_key_rejection(
            "local",
            Some(401),
            "{}",
            true,
            true,
            false
        ));
        assert!(!retain_provider_key_rejection(
            "local",
            Some(401),
            "{}",
            true,
            false,
            true
        ));
    }

    fn journal(name: &str) -> std::path::PathBuf {
        let path = std::path::PathBuf::from(format!(
            "/var/tmp/solstone-binding-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(path.join("health")).expect("journal");
        path
    }

    fn write_key(path: &std::path::Path) -> [u8; 32] {
        let key = [7u8; 32];
        std::fs::write(path.join("health/brain-fingerprint.key"), key).expect("key");
        key
    }

    fn config_with(provider: &str, entry: Value) -> Map<String, Value> {
        let mut validation = Map::new();
        validation.insert(provider.to_owned(), entry);
        Map::from_iter([(
            "providers".to_owned(),
            serde_json::json!({ "key_validation": validation }),
        )])
    }

    #[test]
    fn admission_refuses_missing_managed_key() {
        let request = content_request();
        let canned = canned_request("settings.cloud.validate_key");
        assert_eq!(
            managed_admission(None, &request, true, StoredCredentialBinding::Unbound),
            ManagedAdmission::MissingKey
        );
        assert_eq!(
            managed_admission(None, &canned, false, StoredCredentialBinding::Matches),
            ManagedAdmission::MissingKey
        );
    }

    #[test]
    fn admission_refuses_matching_and_corrupt_rejection() {
        let path = journal("matching");
        let key = write_key(&path);
        let hmac = managed_credential_binding_hmac(&key, "openai", "synthetic-key");
        let mut entry = Map::new();
        entry.insert("valid".to_owned(), Value::Bool(false));
        entry.insert(
            "reason_code".to_owned(),
            Value::String("provider_key_invalid".to_owned()),
        );
        attach_credential_binding(&mut entry, &hmac);
        let config = config_with("openai", Value::Object(entry));
        let binding = classify_managed_binding(&path, &config, "openai", "synthetic-key");
        assert_eq!(binding, StoredCredentialBinding::Matches);
        assert_eq!(
            managed_admission(Some("synthetic-key"), &content_request(), true, binding),
            ManagedAdmission::Rejected
        );

        let corrupt = config_with(
            "openai",
            serde_json::json!({
                "valid": false,
                "reason_code": "provider_key_invalid",
                "binding_version": 2,
                "credential_hmac": hmac
            }),
        );
        let binding = classify_managed_binding(&path, &corrupt, "openai", "synthetic-key");
        assert_eq!(binding, StoredCredentialBinding::Corrupt);
        assert_eq!(
            managed_admission(Some("synthetic-key"), &content_request(), true, binding),
            ManagedAdmission::Rejected
        );
        let bare = path.join("health/brain-fingerprint.key");
        assert!(bare.is_file());
        let _ = std::fs::remove_dir_all(path);
    }

    #[test]
    fn admission_allows_other_provider_other_key_legacy_and_unavailable() {
        let path = journal("allow");
        let key = write_key(&path);
        let foreign = managed_credential_binding_hmac(&key, "openai", "other-key");
        let mut other_key = Map::new();
        other_key.insert("valid".to_owned(), Value::Bool(false));
        other_key.insert(
            "reason_code".to_owned(),
            Value::String("provider_key_invalid".to_owned()),
        );
        attach_credential_binding(&mut other_key, &foreign);
        let config = config_with("openai", Value::Object(other_key));
        assert_eq!(
            classify_managed_binding(&path, &config, "openai", "synthetic-key"),
            StoredCredentialBinding::DoesNotReject
        );
        assert_eq!(
            classify_managed_binding(&path, &config, "anthropic", "synthetic-key"),
            StoredCredentialBinding::Unbound
        );
        let legacy = config_with(
            "openai",
            serde_json::json!({
                "valid": false,
                "reason_code": "provider_key_invalid",
                "error": "rejected",
                "timestamp": "2026-01-01T00:00:00Z"
            }),
        );
        assert_eq!(
            classify_managed_binding(&path, &legacy, "openai", "synthetic-key"),
            StoredCredentialBinding::Unbound
        );
        let unavailable = config_with(
            "openai",
            serde_json::json!({
                "valid": false,
                "reason_code": "validation_unavailable",
                "binding_version": 1,
                "credential_hmac": foreign
            }),
        );
        assert_eq!(
            classify_managed_binding(&path, &unavailable, "openai", "synthetic-key"),
            StoredCredentialBinding::DoesNotReject
        );
        assert_eq!(
            managed_admission(
                Some("synthetic-key"),
                &content_request(),
                true,
                StoredCredentialBinding::DoesNotReject
            ),
            ManagedAdmission::Allow
        );
        assert_eq!(
            managed_admission(
                Some("synthetic-key"),
                &content_request(),
                true,
                StoredCredentialBinding::Unbound
            ),
            ManagedAdmission::Allow
        );
        let before = std::fs::read(path.join("health/brain-fingerprint.key")).unwrap();
        let _ = classify_managed_binding(&path, &config, "openai", "synthetic-key");
        assert_eq!(
            std::fs::read(path.join("health/brain-fingerprint.key")).unwrap(),
            before
        );
        let _ = std::fs::remove_dir_all(path);
    }

    #[test]
    fn admission_allows_keyless_custom_without_evidence() {
        let request = content_request();
        assert!(!endpoint_admission_refuses(
            &request,
            true,
            StoredCredentialBinding::Unbound
        ));
        assert!(!endpoint_admission_refuses(
            &request,
            true,
            StoredCredentialBinding::DoesNotReject
        ));
        assert!(endpoint_admission_refuses(
            &request,
            true,
            StoredCredentialBinding::Matches
        ));
        let path = journal("keyless");
        let config = Map::new();
        assert_eq!(
            classify_endpoint_binding(&path, &config, "http://127.0.0.1:9", ""),
            StoredCredentialBinding::Unbound
        );
        assert!(!path.join("health/brain-fingerprint.key").exists());
        let _ = std::fs::remove_dir_all(path);
    }

    #[test]
    fn canned_probe_shape_is_not_refused() {
        for context in ["settings.cloud.validate_key", HEALTH_BRAIN_GENERATE_CONTEXT] {
            let canned = canned_request(context);
            assert!(canned_probe(&canned));
            assert_eq!(
                managed_admission(
                    Some("synthetic-key"),
                    &canned,
                    true,
                    StoredCredentialBinding::Matches
                ),
                ManagedAdmission::Allow
            );
            assert!(!endpoint_admission_refuses(
                &canned,
                true,
                StoredCredentialBinding::Corrupt
            ));
        }
        let mut almost = canned_request("settings.cloud.validate_key");
        almost.contents = vec![ContentPart::Text {
            text: "Reply with OK".to_owned(),
        }];
        assert!(!canned_probe(&almost));
        assert_eq!(
            managed_admission(
                Some("synthetic-key"),
                &almost,
                true,
                StoredCredentialBinding::Matches
            ),
            ManagedAdmission::Rejected
        );
        almost = canned_request("segment.thought");
        almost.contents = vec![ContentPart::Text {
            text: CANNED_TEXT.to_owned(),
        }];
        assert!(!canned_probe(&almost));
    }

    #[test]
    fn retain_managed_rejection_without_content_admission_writes_nothing() {
        let path = journal("retain-config");
        std::fs::create_dir_all(path.join("config")).expect("config directory");
        let config = serde_json::json!({
            "env": {"OPENAI_API_KEY": "synthetic-key"},
            "providers": {"active": {"provider": "openai", "model": "gpt-binding"}}
        });
        std::fs::write(
            path.join("config/journal.json"),
            serde_json::to_vec(&config).expect("config bytes"),
        )
        .expect("config writes");
        let before = std::fs::read(path.join("config/journal.json")).expect("config bytes");
        let witness = capture_root_witness(&path).expect("root witness");
        let cancellation = solstone_core_generate::Cancellation::never();
        let gate = ContentGate {
            journal: &path,
            admit_content: false,
            cancellation: &cancellation,
        };
        retain_managed_rejection(
            &gate,
            Some(&witness),
            &content_request(),
            "openai",
            "OPENAI_API_KEY",
            "synthetic-key",
            401,
            "{}",
        );
        let after = std::fs::read(path.join("config/journal.json")).expect("config bytes");
        assert_eq!(after, before);
        assert!(!String::from_utf8_lossy(&after).contains("key_validation"));
        assert!(!path.join("health/brain-fingerprint.key").is_file());
        let _ = std::fs::remove_dir_all(path);
    }
}

#[cfg(all(test, feature = "full-tests"))]
mod loopback {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use serde_json::{Map, Value, json};
    use solstone_core_generate::{Cancellation, ContentPart, GenerateRequest, GenerateResponse};

    use super::CANNED_TEXT;

    const MARKER: &str = "SOLSTONE_BINDING_CHILD";
    const JOURNAL_ENV: &str = "SOLSTONE_BINDING_JOURNAL";
    const RECEIPT: &str = "SOLSTONE_BINDING_RECEIPT ";
    const CANARY: &str = "binding-canary";
    const SECRET: &str = "sk-binding-secret";

    fn is_child(case: &str) -> bool {
        match std::env::var(MARKER) {
            Ok(seen) if seen == case => true,
            Ok(seen) => panic!("stale binding marker {seen} for {case}"),
            Err(_) => false,
        }
    }

    fn content_request() -> GenerateRequest {
        GenerateRequest {
            id: None,
            context: "segment.thought".to_owned(),
            contents: vec![ContentPart::Text {
                text: CANARY.to_owned(),
            }],
            system_instruction: None,
            temperature: 0.2,
            max_output_tokens: 32,
            timeout_s: Some(30.0),
            json_output: false,
            json_schema: None,
            attempt_index: 0,
            exclusive_admission: false,
            transport_retries: Some(0),
        }
    }

    fn canned_request() -> GenerateRequest {
        GenerateRequest {
            id: None,
            context: "settings.cloud.validate_key".to_owned(),
            contents: vec![ContentPart::Text {
                text: CANNED_TEXT.to_owned(),
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
        }
    }

    fn reason_code(result: Result<GenerateResponse, String>) -> String {
        match result {
            Ok(GenerateResponse::Generated(_)) => "generated".to_owned(),
            Ok(GenerateResponse::Cancelled) => "cancelled".to_owned(),
            Ok(GenerateResponse::Refused(refusal)) => refusal
                .reason_code
                .as_ref()
                .map(|code| code.as_wire().to_owned())
                .unwrap_or_else(|| "refused".to_owned()),
            Err(_) => "dispatch_error".to_owned(),
        }
    }

    fn emit(value: &Value) {
        println!("{RECEIPT}{value}");
    }

    fn binding_present(journal: &Path) -> bool {
        std::fs::read_to_string(journal.join("config/journal.json"))
            .map(|text| text.contains("credential_hmac"))
            .unwrap_or(false)
    }

    fn journal_dir(name: &str) -> PathBuf {
        let path = PathBuf::from("/var/tmp").join(format!(
            "solstone-binding-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(path.join("config")).expect("journal directory");
        path
    }

    fn write_config(journal: &Path, config: &Value) {
        std::fs::write(
            journal.join("config/journal.json"),
            serde_json::to_vec(config).expect("config bytes"),
        )
        .expect("config writes");
    }

    fn managed_config(provider: &str, env_var: &str, key: Option<&str>, model: &str) -> Value {
        let mut env = Map::new();
        if let Some(key) = key {
            env.insert(env_var.to_owned(), json!(key));
        }
        json!({
            "env": env,
            "providers": {"active": {"provider": provider, "model": model}}
        })
    }

    fn plant_managed_rejection(journal: &Path, provider: &str, credential: &str) -> Value {
        let key = solstone_core_brain::generate_fingerprint_key(journal).expect("binding key");
        let hmac = solstone_core_brain::managed_credential_binding_hmac(&key, provider, credential);
        let mut entry = Map::new();
        entry.insert("valid".to_owned(), Value::Bool(false));
        entry.insert(
            "reason_code".to_owned(),
            Value::String("provider_key_invalid".to_owned()),
        );
        solstone_core_brain::attach_credential_binding(&mut entry, &hmac);
        Value::Object(entry)
    }

    fn child_journal() -> PathBuf {
        PathBuf::from(std::env::var(JOURNAL_ENV).expect("binding journal"))
    }

    fn respond_content() -> String {
        let journal = child_journal();
        let runtime = crate::EndpointRuntime::default();
        let cancellation = Cancellation::never();
        reason_code(crate::respond(
            &content_request(),
            &runtime,
            &journal,
            |_| None,
            &cancellation,
        ))
    }

    fn respond_canned() -> String {
        let journal = child_journal();
        let runtime = crate::EndpointRuntime::default();
        let cancellation = Cancellation::never();
        reason_code(crate::respond(
            &canned_request(),
            &runtime,
            &journal,
            |_| None,
            &cancellation,
        ))
    }

    fn respond_context() -> String {
        let journal = child_journal();
        let runtime = crate::EndpointRuntime::default();
        let cancellation = Cancellation::never();
        let priority =
            solstone_core_journal_config::work_priority::WorkPriority::from_environment()
                .expect("work priority");
        let context = crate::GenerateCallContext {
            priority,
            cancellation: &cancellation,
        };
        reason_code(crate::respond_with_context(
            &content_request(),
            &runtime,
            &journal,
            |_| None,
            &context,
        ))
    }

    fn spawn_child(case: &str, journal: &Path, extra: &[(&str, &str)]) -> Child {
        let mut command = Command::new(std::env::current_exe().expect("test executable"));
        command
            .arg("--exact")
            .arg(format!("credential_binding::loopback::{case}"))
            .arg("--nocapture")
            .env_clear()
            .env("PATH", "")
            .env(MARKER, case)
            .env(JOURNAL_ENV, journal)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, value) in extra {
            command.env(key, value);
        }
        command.spawn().expect("binding child")
    }

    fn finish_child(child: Child) -> Value {
        let output = child.wait_with_output().expect("binding child output");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "binding child failed: {stdout}{stderr}"
        );
        let line = stdout
            .lines()
            .find(|line| line.starts_with(RECEIPT))
            .expect("binding receipt");
        serde_json::from_str(&line[RECEIPT.len()..]).expect("binding receipt json")
    }

    fn bind_listener() -> (TcpListener, u16) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback listener");
        let port = listener.local_addr().expect("loopback port").port();
        (listener, port)
    }

    fn read_http(stream: &mut TcpStream) -> String {
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 2048];
        loop {
            match stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(count) => {
                    buffer.extend_from_slice(&chunk[..count]);
                    if let Some(split) = buffer.windows(4).position(|window| window == b"\r\n\r\n")
                    {
                        let headers = String::from_utf8_lossy(&buffer[..split]);
                        let length = headers
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().ok())?
                            })
                            .unwrap_or(0);
                        if buffer.len() >= split + 4 + length {
                            break;
                        }
                    }
                }
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        || error.kind() == std::io::ErrorKind::TimedOut =>
                {
                    break;
                }
                Err(_) => break,
            }
        }
        String::from_utf8_lossy(&buffer).into_owned()
    }

    fn write_http(stream: &mut TcpStream, status: u16, body: &str) {
        let message = format!(
            "HTTP/1.1 {status} Binding\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = stream.write_all(message.as_bytes());
    }

    fn accept_request(listener: &TcpListener, timeout: Duration) -> Option<(TcpStream, String)> {
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let deadline = Instant::now() + timeout;
        loop {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream.set_nonblocking(false).expect("blocking stream");
                    stream
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .expect("read timeout");
                    let body = read_http(&mut stream);
                    return Some((stream, body));
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return None;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(error) => panic!("loopback accept failed: {error}"),
            }
        }
    }

    fn base_env(port: u16) -> (String, String) {
        (
            crate::overrides::BASE_URL_OVERRIDE_ENV.to_owned(),
            format!("http://127.0.0.1:{port}"),
        )
    }

    fn config_text(journal: &Path) -> String {
        std::fs::read_to_string(journal.join("config/journal.json")).unwrap_or_default()
    }

    #[test]
    fn loopback_first_401_posts_canary_then_same_credential_posts_nothing() {
        const CASE: &str = "loopback_first_401_posts_canary_then_same_credential_posts_nothing";
        if is_child(CASE) {
            let journal = child_journal();
            let first = respond_content();
            let second = respond_content();
            emit(&json!({"reasons":[first, second], "bound": binding_present(&journal)}));
            return;
        }
        let journal = journal_dir("first-401");
        write_config(
            &journal,
            &managed_config("openai", "OPENAI_API_KEY", Some(SECRET), "gpt-binding"),
        );
        let (listener, port) = bind_listener();
        let (name, base) = base_env(port);
        let child = spawn_child(CASE, &journal, &[(&name, &base)]);
        let (mut stream, body) =
            accept_request(&listener, Duration::from_secs(5)).expect("first post");
        assert!(body.contains(CANARY));
        assert!(!body.contains(SECRET));
        write_http(&mut stream, 401, "{}");
        drop(stream);
        assert!(accept_request(&listener, Duration::from_secs(2)).is_none());
        let receipt = finish_child(child);
        assert_eq!(receipt["reasons"][0], "provider_key_invalid");
        assert_eq!(receipt["reasons"][1], "provider_key_invalid");
        assert_eq!(receipt["bound"], true);
        let _ = std::fs::remove_dir_all(journal);
    }

    #[test]
    fn loopback_model_only_change_still_blocks_and_corrected_key_posts() {
        const CASE: &str = "loopback_model_only_change_still_blocks_and_corrected_key_posts";
        if is_child(CASE) {
            let phase = std::env::var("SOLSTONE_BINDING_PHASE").unwrap_or_default();
            let journal = child_journal();
            let reason = respond_content();
            emit(&json!({"phase": phase, "reason": reason, "bound": binding_present(&journal)}));
            return;
        }
        let journal = journal_dir("model-only");
        write_config(
            &journal,
            &managed_config("openai", "OPENAI_API_KEY", Some(SECRET), "gpt-binding"),
        );
        let (listener, port) = bind_listener();
        let (name, base) = base_env(port);
        let child = spawn_child(
            CASE,
            &journal,
            &[(&name, &base), ("SOLSTONE_BINDING_PHASE", "first")],
        );
        let (mut stream, body) =
            accept_request(&listener, Duration::from_secs(5)).expect("first post");
        assert!(body.contains(CANARY));
        write_http(&mut stream, 401, "{}");
        let receipt = finish_child(child);
        assert_eq!(receipt["reason"], "provider_key_invalid");
        assert_eq!(receipt["bound"], true);
        let mut config: Value =
            serde_json::from_str(&config_text(&journal)).expect("stored config");
        config["providers"]["active"]["model"] = json!("gpt-binding-other");
        write_config(&journal, &config);
        let blocked = spawn_child(
            CASE,
            &journal,
            &[(&name, &base), ("SOLSTONE_BINDING_PHASE", "blocked")],
        );
        assert!(accept_request(&listener, Duration::from_secs(2)).is_none());
        let blocked = finish_child(blocked);
        assert_eq!(blocked["reason"], "provider_key_invalid");
        config["env"]["OPENAI_API_KEY"] = json!("sk-binding-corrected");
        config["providers"]
            .as_object_mut()
            .expect("providers")
            .remove("key_validation");
        write_config(&journal, &config);
        let corrected = spawn_child(
            CASE,
            &journal,
            &[(&name, &base), ("SOLSTONE_BINDING_PHASE", "corrected")],
        );
        let (mut stream, body) =
            accept_request(&listener, Duration::from_secs(5)).expect("corrected post");
        assert!(body.contains(CANARY));
        assert!(!body.contains("sk-binding-corrected"));
        write_http(&mut stream, 200, "{}");
        let _ = finish_child(corrected);
        let _ = std::fs::remove_dir_all(journal);
    }

    #[test]
    fn loopback_other_provider_posts() {
        const CASE: &str = "loopback_other_provider_posts";
        if is_child(CASE) {
            let journal = child_journal();
            emit(&json!({"reason": respond_content(), "bound": binding_present(&journal)}));
            return;
        }
        let journal = journal_dir("other-provider");
        let rejection = plant_managed_rejection(&journal, "openai", SECRET);
        let mut config = managed_config(
            "anthropic",
            "ANTHROPIC_API_KEY",
            Some(SECRET),
            "claude-binding",
        );
        config["providers"]["key_validation"] = json!({"openai": rejection});
        config["env"]["OPENAI_API_KEY"] = json!(SECRET);
        write_config(&journal, &config);
        let (listener, port) = bind_listener();
        let (name, base) = base_env(port);
        let child = spawn_child(CASE, &journal, &[(&name, &base)]);
        let (mut stream, body) =
            accept_request(&listener, Duration::from_secs(5)).expect("anthropic post");
        assert!(body.contains(CANARY));
        write_http(&mut stream, 200, "{}");
        let receipt = finish_child(child);
        assert_ne!(receipt["reason"], "provider_key_invalid");
        let _ = std::fs::remove_dir_all(journal);
    }

    #[test]
    fn loopback_respond_with_context_ignores_generate_override() {
        const CASE: &str = "loopback_respond_with_context_ignores_generate_override";
        if is_child(CASE) {
            emit(&json!({"reason": respond_context()}));
            return;
        }
        let journal = journal_dir("context-override");
        write_config(
            &journal,
            &managed_config("openai", "OPENAI_API_KEY", None, "gpt-binding"),
        );
        let (listener, port) = bind_listener();
        let (name, base) = base_env(port);
        let key_name = crate::overrides::API_KEY_OVERRIDE_ENV.to_owned();
        let child = spawn_child(CASE, &journal, &[(&name, &base), (&key_name, SECRET)]);
        assert!(accept_request(&listener, Duration::from_secs(2)).is_none());
        let receipt = finish_child(child);
        assert_eq!(receipt["reason"], "provider_key_missing");
        assert!(!binding_present(&journal));
        let _ = std::fs::remove_dir_all(journal);
    }

    #[test]
    fn loopback_opaque_403_429_5xx_and_network_do_not_stick() {
        const CASE: &str = "loopback_opaque_403_429_5xx_and_network_do_not_stick";
        if is_child(CASE) {
            let journal = child_journal();
            let mut reasons = Vec::new();
            for _ in 0..5 {
                reasons.push(respond_content());
            }
            emit(&json!({"reasons": reasons, "bound": binding_present(&journal)}));
            return;
        }
        let journal = journal_dir("unsticky");
        write_config(
            &journal,
            &managed_config("openai", "OPENAI_API_KEY", Some(SECRET), "gpt-binding"),
        );
        let (listener, port) = bind_listener();
        let (name, base) = base_env(port);
        let child = spawn_child(CASE, &journal, &[(&name, &base)]);
        for (status, body) in [(403, "{}"), (429, "{}"), (500, "{}"), (0, ""), (200, "{}")] {
            let (mut stream, request) =
                accept_request(&listener, Duration::from_secs(5)).expect("provider post");
            assert!(request.contains(CANARY));
            if status == 0 {
                drop(stream);
            } else {
                write_http(&mut stream, status, body);
            }
        }
        let receipt = finish_child(child);
        assert_eq!(receipt["bound"], false);
        assert_eq!(receipt["reasons"].as_array().expect("reasons").len(), 5);
        let _ = std::fs::remove_dir_all(journal);
    }

    #[test]
    fn loopback_canned_child_and_respond_with_config_post_while_content_does_not() {
        const CASE: &str =
            "loopback_canned_child_and_respond_with_config_post_while_content_does_not";
        if is_child(CASE) {
            let journal = child_journal();
            let content = respond_content();
            let canned = respond_canned();
            emit(
                &json!({"content": content, "canned": canned, "bound": binding_present(&journal)}),
            );
            return;
        }
        let journal = journal_dir("canned");
        let rejection = plant_managed_rejection(&journal, "openai", SECRET);
        let mut config = managed_config("openai", "OPENAI_API_KEY", Some(SECRET), "gpt-binding");
        config["providers"]["key_validation"] = json!({"openai": rejection});
        write_config(&journal, &config);
        let (listener, port) = bind_listener();
        let (name, base) = base_env(port);
        let child = spawn_child(CASE, &journal, &[(&name, &base)]);
        let (mut stream, body) =
            accept_request(&listener, Duration::from_secs(5)).expect("canned post");
        assert!(body.contains(CANNED_TEXT));
        assert!(!body.contains(CANARY));
        write_http(&mut stream, 200, "{}");
        assert!(accept_request(&listener, Duration::from_secs(1)).is_none());
        let receipt = finish_child(child);
        assert_eq!(receipt["content"], "provider_key_invalid");
        assert_eq!(receipt["bound"], true);

        let (endpoint_listener, endpoint_port) = bind_listener();
        let endpoint = format!("http://127.0.0.1:{endpoint_port}");
        let endpoint_key =
            solstone_core_brain::load_existing_fingerprint_key(&journal).expect("fingerprint key");
        let endpoint_hmac = solstone_core_brain::endpoint_credential_binding_hmac(
            &endpoint_key,
            &endpoint,
            "endpoint-secret",
        );
        let mut entry = Map::new();
        entry.insert("valid".to_owned(), Value::Bool(false));
        entry.insert(
            "reason_code".to_owned(),
            Value::String("provider_key_invalid".to_owned()),
        );
        solstone_core_brain::attach_credential_binding(&mut entry, &endpoint_hmac);
        let local_config = json!({
            "providers": {
                "active": {"provider": "local", "model": "served-model"},
                "local": {
                    "endpoint_url": endpoint,
                    "served_model_id": "served-model",
                    "credential": "endpoint-secret"
                },
                "key_validation": {"local": Value::Object(entry)}
            }
        });
        let local_config = local_config.as_object().expect("config").clone();
        let runtime = crate::EndpointRuntime::default();
        let cancellation = Cancellation::never();
        let cleanup = journal.clone();
        let posted = std::thread::spawn(move || {
            crate::respond_with_config(
                &content_request(),
                &runtime,
                &journal,
                &local_config,
                |_| None,
                &cancellation,
            )
        });
        let (mut stream, body) =
            accept_request(&endpoint_listener, Duration::from_secs(5)).expect("config post");
        assert!(body.contains(CANARY));
        write_http(&mut stream, 200, "{}");
        let _ = posted.join().expect("config respond");
        let _ = std::fs::remove_dir_all(cleanup);
    }

    #[test]
    fn loopback_stale_401_after_key_provider_or_root_change_writes_nothing() {
        const CASE: &str = "loopback_stale_401_after_key_provider_or_root_change_writes_nothing";
        if is_child(CASE) {
            let journal = child_journal();
            emit(&json!({"reason": respond_content(), "bound": binding_present(&journal)}));
            return;
        }
        stale_after(&|journal| {
            let mut config: Value = serde_json::from_str(&config_text(journal)).expect("config");
            config["env"]["OPENAI_API_KEY"] = json!("sk-binding-replaced");
            write_config(journal, &config);
            journal.to_path_buf()
        });
        stale_after(&|journal| {
            let mut config: Value = serde_json::from_str(&config_text(journal)).expect("config");
            config["providers"]["active"]["provider"] = json!("anthropic");
            write_config(journal, &config);
            journal.to_path_buf()
        });
        stale_after(&|journal| {
            let renamed = journal.with_extension("renamed");
            std::fs::rename(journal, &renamed).expect("rename journal");
            renamed
        });
    }

    fn stale_after(mutate: &dyn Fn(&Path) -> PathBuf) {
        const CASE: &str = "loopback_stale_401_after_key_provider_or_root_change_writes_nothing";
        let journal = journal_dir("stale");
        write_config(
            &journal,
            &managed_config("openai", "OPENAI_API_KEY", Some(SECRET), "gpt-binding"),
        );
        let (listener, port) = bind_listener();
        let (name, base) = base_env(port);
        let child = spawn_child(CASE, &journal, &[(&name, &base)]);
        let (mut stream, body) =
            accept_request(&listener, Duration::from_secs(5)).expect("stale post");
        assert!(body.contains(CANARY));
        let kept = mutate(&journal);
        write_http(&mut stream, 401, "{}");
        let receipt = finish_child(child);
        assert_eq!(receipt["reason"], "provider_key_invalid");
        assert_eq!(receipt["bound"], false);
        assert!(!config_text(&kept).contains("credential_hmac"));
        let _ = std::fs::remove_dir_all(&journal);
        let _ = std::fs::remove_dir_all(&kept);
    }

    #[test]
    fn loopback_cancelled_call_writes_nothing() {
        let journal = journal_dir("cancelled");
        write_config(
            &journal,
            &managed_config("openai", "OPENAI_API_KEY", Some(SECRET), "gpt-binding"),
        );
        let check = || true;
        let cancellation = Cancellation::from_fn(&check);
        let runtime = crate::EndpointRuntime::default();
        let response = crate::respond(
            &content_request(),
            &runtime,
            &journal,
            |_| None,
            &cancellation,
        )
        .expect("cancelled respond");
        assert!(matches!(response, GenerateResponse::Cancelled));
        assert!(!binding_present(&journal));
        let _ = std::fs::remove_dir_all(journal);
    }

    #[test]
    fn loopback_confidential_attestation_refusal_fails_before_content() {
        let (listener, port) = bind_listener();
        let endpoint = format!("http://127.0.0.1:{port}");
        let journal = journal_dir("confidential");
        let credential = "endpoint-credential";
        let fingerprint = sha256_hex(credential);
        write_config(
            &journal,
            &json!({
                "providers": {
                    "active": {"provider": "local", "model": "served-model"},
                    "local": {
                        "endpoint_url": endpoint,
                        "served_model_id": "served-model",
                        "credential": credential
                    }
                },
                "services": {
                    "confidential": {
                        "endpoint_url": endpoint,
                        "served_model_id": "served-model",
                        "credential_fingerprint_sha256": fingerprint
                    }
                }
            }),
        );
        let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let bodies_thread = bodies.clone();
        let stop_thread = stop.clone();
        let server = std::thread::spawn(move || {
            while !stop_thread.load(Ordering::Relaxed) {
                if let Some((mut stream, body)) =
                    accept_request(&listener, Duration::from_millis(50))
                {
                    bodies_thread.lock().expect("bodies").push(body);
                    write_http(&mut stream, 401, "{}");
                }
            }
        });
        let runtime = crate::EndpointRuntime::default();
        let cancellation = Cancellation::never();
        let response = crate::respond(
            &content_request(),
            &runtime,
            &journal,
            |_| None,
            &cancellation,
        )
        .expect("confidential respond");
        match response {
            GenerateResponse::Refused(refusal) => assert!(matches!(
                refusal.reason,
                solstone_core_generate::RefusalReason::AttestationNotVerified
                    | solstone_core_generate::RefusalReason::AttestationFailed
                    | solstone_core_generate::RefusalReason::AttestationStale
            )),
            other => panic!("attestation refusal expected, got {other:?}"),
        }
        stop.store(true, Ordering::Relaxed);
        let _ = server.join();
        let seen = bodies.lock().expect("bodies");
        assert!(seen.iter().all(|body| !body.contains(CANARY)));
        assert!(!binding_present(&journal));
        let _ = std::fs::remove_dir_all(journal);
    }

    fn sha256_hex(value: &str) -> String {
        ring::digest::digest(&ring::digest::SHA256, value.as_bytes())
            .as_ref()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    #[test]
    fn loopback_keyed_custom_401_blocks_second_post_and_keyless_still_posts() {
        let (listener, port) = bind_listener();
        let endpoint = format!("http://127.0.0.1:{port}");
        let journal = journal_dir("custom");
        write_config(
            &journal,
            &json!({
                "providers": {
                    "active": {"provider": "local", "model": "served-model"},
                    "local": {
                        "endpoint_url": endpoint,
                        "served_model_id": "served-model",
                        "credential": "endpoint-secret"
                    }
                }
            }),
        );
        let stop = Arc::new(AtomicBool::new(false));
        let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
        let statuses = Arc::new(Mutex::new(std::collections::VecDeque::<u16>::new()));
        let stop_thread = stop.clone();
        let bodies_thread = bodies.clone();
        let statuses_thread = statuses.clone();
        let server = std::thread::spawn(move || {
            while !stop_thread.load(Ordering::Relaxed) {
                if let Some((mut stream, body)) =
                    accept_request(&listener, Duration::from_millis(50))
                {
                    bodies_thread.lock().expect("bodies").push(body);
                    let status = statuses_thread
                        .lock()
                        .expect("statuses")
                        .pop_front()
                        .unwrap_or(200);
                    write_http(&mut stream, status, "{}");
                }
            }
        });
        statuses.lock().expect("statuses").push_back(401);
        let runtime = crate::EndpointRuntime::default();
        let cancellation = Cancellation::never();
        let first = crate::respond(
            &content_request(),
            &runtime,
            &journal,
            |_| None,
            &cancellation,
        )
        .expect("first custom respond");
        assert_eq!(reason_code(Ok(first)), "provider_key_invalid");
        assert!(binding_present(&journal));
        assert_eq!(bodies.lock().expect("bodies").len(), 1);
        assert!(bodies.lock().expect("bodies")[0].contains(CANARY));
        let second = crate::respond(
            &content_request(),
            &runtime,
            &journal,
            |_| None,
            &cancellation,
        )
        .expect("second custom respond");
        assert_eq!(reason_code(Ok(second)), "provider_key_invalid");
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(bodies.lock().expect("bodies").len(), 1);
        let mut config: Value = serde_json::from_str(&config_text(&journal)).expect("config");
        config["providers"]["local"]
            .as_object_mut()
            .expect("local")
            .remove("credential");
        write_config(&journal, &config);
        statuses.lock().expect("statuses").push_back(200);
        let third = crate::respond(
            &content_request(),
            &runtime,
            &journal,
            |_| None,
            &cancellation,
        )
        .expect("keyless respond");
        assert_ne!(reason_code(Ok(third)), "provider_key_invalid");
        assert_eq!(bodies.lock().expect("bodies").len(), 2);
        assert!(bodies.lock().expect("bodies")[1].contains(CANARY));
        stop.store(true, Ordering::Relaxed);
        let _ = server.join();
        let _ = std::fs::remove_dir_all(journal);
    }

    #[test]
    fn loopback_reread_journal_blocks_new_respond() {
        const CASE: &str = "loopback_reread_journal_blocks_new_respond";
        if is_child(CASE) {
            let journal = child_journal();
            emit(&json!({"reason": respond_content(), "bound": binding_present(&journal)}));
            return;
        }
        let journal = journal_dir("reread");
        write_config(
            &journal,
            &managed_config("openai", "OPENAI_API_KEY", Some(SECRET), "gpt-binding"),
        );
        let (listener, port) = bind_listener();
        let (name, base) = base_env(port);
        let first = spawn_child(CASE, &journal, &[(&name, &base)]);
        let (mut stream, body) =
            accept_request(&listener, Duration::from_secs(5)).expect("first process");
        assert!(body.contains(CANARY));
        write_http(&mut stream, 401, "{}");
        let receipt = finish_child(first);
        assert_eq!(receipt["reason"], "provider_key_invalid");
        assert_eq!(receipt["bound"], true);
        let second = spawn_child(CASE, &journal, &[(&name, &base)]);
        assert!(accept_request(&listener, Duration::from_secs(2)).is_none());
        let receipt = finish_child(second);
        assert_eq!(receipt["reason"], "provider_key_invalid");
        assert_eq!(receipt["bound"], true);
        let _ = std::fs::remove_dir_all(journal);
    }
}
