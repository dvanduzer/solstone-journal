// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! A retained authentication rejection stays on the credential that was sent.
//! Ordinary key correction is what lets a later production call post those bytes.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::json;
use solstone_core_generate::{Cancellation, ContentPart, GenerateRequest};

const MARKER: &str = "SOLSTONE_BINDING_COMPOSITION_CHILD";
const JOURNAL_ENV: &str = "SOLSTONE_BINDING_JOURNAL";
const RECEIPT: &str = "SOLSTONE_BINDING_RECEIPT ";
const CANARY: &str = "composition-canary-bytes";
const SECRET: &str = "sk-composition-secret";
const CORRECTED: &str = "sk-composition-corrected";
const CASE: &str = "credential_binding_composition::bound_rejection_composition_posts_retained_bytes_after_correction";

#[test]
fn bound_rejection_composition_posts_retained_bytes_after_correction() {
    if std::env::var(MARKER).ok().as_deref() == Some(CASE) {
        let journal = PathBuf::from(std::env::var(JOURNAL_ENV).expect("journal"));
        let runtime = solstone_core_generate_wire::EndpointRuntime::default();
        let cancellation = Cancellation::never();
        let request = GenerateRequest {
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
        };
        let result = solstone_core_generate_wire::respond(
            &request,
            &runtime,
            &journal,
            |_| None,
            &cancellation,
        );
        let reason = match result {
            Ok(solstone_core_generate::GenerateResponse::Refused(refusal)) => refusal
                .reason_code
                .as_ref()
                .map(|code| code.as_wire().to_owned())
                .unwrap_or_else(|| "refused".to_owned()),
            Ok(solstone_core_generate::GenerateResponse::Generated(_)) => "generated".to_owned(),
            Ok(solstone_core_generate::GenerateResponse::Cancelled) => "cancelled".to_owned(),
            Err(_) => "dispatch_error".to_owned(),
        };
        println!("{RECEIPT}{{\"reason\":\"{reason}\"}}");
        return;
    }

    let journal = PathBuf::from("/var/tmp").join(format!(
        "solstone-binding-composition-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::create_dir_all(journal.join("config")).expect("journal");
    std::fs::write(
        journal.join("config/journal.json"),
        serde_json::to_vec(&json!({
            "env": {"OPENAI_API_KEY": SECRET},
            "providers": {"active": {"provider": "openai", "model": "gpt-binding"}}
        }))
        .expect("config"),
    )
    .expect("config writes");
    let now = chrono::Utc::now() - chrono::Duration::seconds(30);
    let record_path = solstone_core_brain::brain_state_path(&journal);
    std::fs::create_dir_all(record_path.parent().expect("health")).expect("health");
    std::fs::write(
        &record_path,
        serde_json::to_vec(&json!({
            "schema_version": 1,
            "revision": 1,
            "updated_at": now.to_rfc3339(),
            "aggregate_state": "ready",
            "reason_code": null,
            "active_lane": "byo-cloud",
            "active_provider": "openai",
            "active_model": "gpt-binding",
            "fingerprint_sha256": "a".repeat(64),
            "diagnostic": {},
            "checking": null,
            "runtime_failure_marker": null,
            "evidence": {
                "configuration": {"status": "ok", "observed_at": now.to_rfc3339(), "expires_at": "2030-01-01T00:00:00Z"},
                "generate": {"status": "ok", "observed_at": now.to_rfc3339(), "expires_at": "2030-01-01T00:00:00Z"},
                "lane_prerequisites": {"status": "ok", "observed_at": now.to_rfc3339(), "expires_at": "2030-01-01T00:00:00Z"}
            }
        }))
        .expect("record"),
    )
    .expect("record writes");

    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    let port = listener.local_addr().expect("port").port();
    let base = format!("http://127.0.0.1:{port}");
    let mut child = Command::new(std::env::current_exe().expect("test executable"));
    child
        .arg("--exact")
        .arg(CASE)
        .arg("--nocapture")
        .env_clear()
        .env("PATH", "")
        .env(MARKER, CASE)
        .env(JOURNAL_ENV, &journal)
        .env(
            solstone_core_generate_wire::overrides::BASE_URL_OVERRIDE_ENV,
            &base,
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = child.spawn().expect("composition child");
    let (mut stream, body) = accept_request(&listener);
    assert!(body.contains(CANARY), "first post carries the canary");
    assert!(!body.contains(SECRET));
    write_http(&mut stream, 401, "{}");
    let output = child.wait_with_output().expect("child output");
    assert!(output.status.success(), "child failed");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("provider_key_invalid"));

    let config = solstone_core_thinking::read_config(&journal).expect("config");
    let stored = config["providers"]["key_validation"]["openai"].to_string();
    assert!(stored.contains("credential_hmac"));
    assert!(!stored.contains(SECRET));
    let inspection =
        solstone_core_brain::inspect_brain_state(&journal, &config, chrono::Utc::now());
    assert_eq!(
        inspection.projection.reason_code.as_deref(),
        Some("provider_key_invalid")
    );
    assert_eq!(inspection.projection.aggregate_state, "unhealthy");
    let entry = config["providers"]["key_validation"]["openai"].clone();
    let admitted = solstone_core_journal_io::JournalRoot::open(&journal).expect("journal root");
    let cancellation = Cancellation::never();
    solstone_core_thinking::providers::save_key(
        &journal,
        &admitted,
        &cancellation,
        "OPENAI_API_KEY",
        "openai",
        CORRECTED,
        Some(json!({"valid": true})),
        SECRET,
        Some(&entry),
    )
    .expect("ordinary key correction");

    let mut child = Command::new(std::env::current_exe().expect("test executable"));
    child
        .arg("--exact")
        .arg(CASE)
        .arg("--nocapture")
        .env_clear()
        .env("PATH", "")
        .env(MARKER, CASE)
        .env(JOURNAL_ENV, &journal)
        .env(
            solstone_core_generate_wire::overrides::BASE_URL_OVERRIDE_ENV,
            &base,
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = child.spawn().expect("corrected child");
    let (mut stream, body) = accept_request(&listener);
    assert!(
        body.contains(CANARY),
        "corrected post carries the same bytes"
    );
    assert!(!body.contains(CORRECTED));
    write_http(&mut stream, 200, "{}");
    let _ = child.wait_with_output().expect("corrected child");
    assert!(!journal.join("health/work-retry").exists());
    let _ = std::fs::remove_dir_all(journal);
}

const OWNER_CASE: &str =
    "credential_binding_composition::owner_correction_keeps_the_unprocessed_segment";

#[test]
fn owner_correction_keeps_the_unprocessed_segment() {
    if std::env::var(MARKER).ok().as_deref() == Some(OWNER_CASE) {
        let journal = PathBuf::from(std::env::var(JOURNAL_ENV).expect("journal"));
        let phase = std::env::var("SOLSTONE_BINDING_PHASE").unwrap_or_default();
        let runtime = solstone_core_generate_wire::EndpointRuntime::default();
        let cancellation = Cancellation::never();
        if phase == "probe" {
            let request = GenerateRequest {
                id: None,
                context: "settings.cloud.validate_key".to_owned(),
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
            let result = solstone_core_generate_wire::respond(
                &request,
                &runtime,
                &journal,
                |_| None,
                &cancellation,
            );
            let valid = matches!(
                result,
                Ok(solstone_core_generate::GenerateResponse::Generated(_))
            );
            println!("{RECEIPT}{{\"valid\":{valid}}}");
            return;
        }
        let source = PathBuf::from(std::env::var("SOLSTONE_BINDING_SOURCE").expect("source"));
        let text = String::from_utf8(std::fs::read(&source).expect("source bytes")).expect("text");
        let request = GenerateRequest {
            id: None,
            context: "segment.thought".to_owned(),
            contents: vec![ContentPart::Text { text }],
            system_instruction: None,
            temperature: 0.2,
            max_output_tokens: 32,
            timeout_s: Some(30.0),
            json_output: false,
            json_schema: None,
            attempt_index: 0,
            exclusive_admission: false,
            transport_retries: Some(0),
        };
        let result = solstone_core_generate_wire::respond(
            &request,
            &runtime,
            &journal,
            |_| None,
            &cancellation,
        );
        let reason = match result {
            Ok(solstone_core_generate::GenerateResponse::Refused(refusal)) => refusal
                .reason_code
                .as_ref()
                .map(|code| code.as_wire().to_owned())
                .unwrap_or_else(|| "refused".to_owned()),
            Ok(solstone_core_generate::GenerateResponse::Generated(_)) => "generated".to_owned(),
            Ok(solstone_core_generate::GenerateResponse::Cancelled) => "cancelled".to_owned(),
            Err(_) => "dispatch_error".to_owned(),
        };
        println!("{RECEIPT}{{\"reason\":\"{reason}\"}}");
        return;
    }

    let journal = PathBuf::from("/var/tmp").join(format!(
        "solstone-binding-owner-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    let segment = journal.join("chronicle/20260812/capture/120000_1");
    std::fs::create_dir_all(journal.join("config")).expect("config");
    std::fs::create_dir_all(&segment).expect("segment");
    let source = segment.join("audio.flac");
    std::fs::write(&source, CANARY.as_bytes()).expect("source");
    std::fs::write(
        journal.join("config/journal.json"),
        serde_json::to_vec(&json!({
            "env": {"OPENAI_API_KEY": SECRET},
            "providers": {"active": {"provider": "openai", "model": "gpt-binding"}}
        }))
        .expect("config"),
    )
    .expect("config writes");
    let day = journal.join("chronicle/20260812");
    let identity = solstone_core_sense::batch::scan_unprocessed(&journal, &day, None, None, None)
        .expect("scan");
    assert_eq!(identity.len(), 1);
    let identity = identity.into_iter().next().expect("segment");
    assert_eq!(
        std::fs::read(&identity.path).expect("source bytes"),
        CANARY.as_bytes()
    );

    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    let port = listener.local_addr().expect("port").port();
    let base = format!("http://127.0.0.1:{port}");
    let spawn = |phase: &str, extra: &[(&str, &str)]| {
        let mut child = Command::new(std::env::current_exe().expect("test executable"));
        child
            .arg("--exact")
            .arg(OWNER_CASE)
            .arg("--nocapture")
            .env_clear()
            .env("PATH", "")
            .env(MARKER, OWNER_CASE)
            .env(JOURNAL_ENV, &journal)
            .env("SOLSTONE_BINDING_PHASE", phase)
            .env("SOLSTONE_BINDING_SOURCE", &source)
            .env(
                solstone_core_generate_wire::overrides::BASE_URL_OVERRIDE_ENV,
                &base,
            )
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, value) in extra {
            child.env(key, value);
        }
        child.spawn().expect("owner child")
    };
    let finish = |child: std::process::Child| {
        let output = child.wait_with_output().expect("child output");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{stdout}{stderr}");
        let line = stdout
            .lines()
            .find(|line| line.starts_with(RECEIPT))
            .expect("receipt");
        serde_json::from_str::<serde_json::Value>(&line[RECEIPT.len()..]).expect("receipt json")
    };
    let same_identity = || {
        let again = solstone_core_sense::batch::scan_unprocessed(&journal, &day, None, None, None)
            .expect("scan");
        assert_eq!(again, vec![identity.clone()]);
        assert_eq!(
            std::fs::read(&identity.path).expect("source bytes"),
            CANARY.as_bytes()
        );
    };

    let child = spawn("post", &[]);
    let (mut stream, body) = accept_request(&listener);
    assert!(body.contains(CANARY));
    assert!(!body.contains(SECRET));
    write_http(&mut stream, 401, "{}");
    let receipt = finish(child);
    assert_eq!(receipt["reason"], "provider_key_invalid");
    let config = solstone_core_thinking::read_config(&journal).expect("config");
    assert!(
        config["providers"]["key_validation"]["openai"]["credential_hmac"]
            .as_str()
            .is_some()
    );
    assert!(!solstone_core_brain::brain_state_path(&journal).exists());
    let inspection =
        solstone_core_brain::inspect_brain_state(&journal, &config, chrono::Utc::now());
    assert_eq!(
        inspection.projection.reason_code.as_deref(),
        Some("provider_key_invalid")
    );
    assert_eq!(inspection.projection.aggregate_state, "unhealthy");
    assert!(inspection.projection.fingerprint_sha256.is_none());
    same_identity();
    assert!(!journal.join("health/work-retry").exists());

    let mut edited: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(journal.join("config/journal.json")).expect("text"),
    )
    .expect("json");
    let hmac = edited["providers"]["key_validation"]["openai"]["credential_hmac"].clone();
    edited["providers"]["active"]["model"] = json!("gpt-binding-other");
    std::fs::write(
        journal.join("config/journal.json"),
        serde_json::to_vec(&edited).expect("bytes"),
    )
    .expect("model write");
    same_identity();
    struct Unavailable;
    impl solstone_core_thinking::providers::ManagedKeyValidator for Unavailable {
        fn validate(&self, _provider: &str, _key: &str) -> Result<serde_json::Value, String> {
            Ok(json!({"valid": false, "reason_code": "validation_unavailable"}))
        }
    }
    solstone_core_thinking::providers::persist_key_validations(&journal, &Unavailable)
        .expect("unavailable persist");
    let kept = solstone_core_thinking::read_config(&journal).expect("config");
    assert_eq!(
        kept["providers"]["key_validation"]["openai"]["reason_code"],
        "provider_key_invalid"
    );
    assert_eq!(
        kept["providers"]["key_validation"]["openai"]["credential_hmac"],
        hmac
    );
    same_identity();

    let admitted = solstone_core_journal_io::JournalRoot::open(&journal).expect("root");
    let cancellation = Cancellation::never();
    let probe = spawn(
        "probe",
        &[(
            solstone_core_generate_wire::overrides::API_KEY_OVERRIDE_ENV,
            CORRECTED,
        )],
    );
    let (mut stream, body) = accept_request(&listener);
    assert!(body.contains("Reply with the single word OK."));
    write_http(
        &mut stream,
        200,
        r#"{"model":"gpt-binding","status":"completed","output":[{"content":[{"type":"output_text","text":"OK"}]}]}"#,
    );
    let probed = finish(probe);
    assert_eq!(probed["valid"], true);
    let validation = json!({"valid": probed["valid"]});
    let entry = solstone_core_thinking::read_config(&journal).expect("config")["providers"]
        ["key_validation"]["openai"]
        .clone();
    solstone_core_thinking::providers::save_key(
        &journal,
        &admitted,
        &cancellation,
        "OPENAI_API_KEY",
        "openai",
        CORRECTED,
        Some(validation),
        SECRET,
        Some(&entry),
    )
    .expect("correction from the canned probe");
    same_identity();

    let child = spawn("again", &[]);
    let (mut stream, body) = accept_request(&listener);
    assert!(body.contains(CANARY));
    assert!(!body.contains(CORRECTED));
    write_http(
        &mut stream,
        200,
        r#"{"model":"gpt-binding-other","status":"completed","output":[{"content":[{"type":"output_text","text":"OK"}]}]}"#,
    );
    let receipt = finish(child);
    assert_eq!(receipt["reason"], "generated");
    same_identity();
    assert_eq!(std::fs::read(&source).expect("source"), CANARY.as_bytes());
    assert!(!journal.join("health/work-retry").exists());
    let _ = std::fs::remove_dir_all(journal);
}

fn accept_request(listener: &TcpListener) -> (TcpStream, String) {
    listener.set_nonblocking(false).expect("blocking listener");
    let (mut stream, _) = listener.accept().expect("post");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("timeout");
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 2048];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(count) => {
                buffer.extend_from_slice(&chunk[..count]);
                if let Some(split) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
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
            Err(_) => break,
        }
    }
    (stream, String::from_utf8_lossy(&buffer).into_owned())
}

fn write_http(stream: &mut TcpStream, status: u16, body: &str) {
    let message = format!(
        "HTTP/1.1 {status} Binding\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(message.as_bytes());
}
