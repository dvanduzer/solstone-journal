// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Story source-word body regression cases, run through the native caller.
//!
//! Each case writes one segment of invented source files, runs the shipped
//! Story talent through `execute_request` with one scripted completion, and
//! checks what was stored and what was returned.

use std::fs;
use std::path::Path;
use std::sync::Mutex;

use serde_json::{Map, Value, json};
use solstone_core_generate::{
    ClientError, GenerateExecutor, GenerateRequest, GenerateResponse, GeneratedResponse,
};

use crate::contract::CommitDisposition;
use crate::prepare::RuntimePaths;
use crate::{ExecutionContext, RuntimeOutcome, execute_request};

const CASES: &str = include_str!("story_source_conformance.json");
const DAY: &str = "20260101";
const SEGMENT: &str = "090000_300";
const FACET: &str = "work";
const ACTIVITY: &str = "activity_090000_300";

struct Scripted {
    text: String,
    input_budget: Option<Value>,
    requests: Mutex<Vec<GenerateRequest>>,
}

impl GenerateExecutor for Scripted {
    fn execute(&self, request: &GenerateRequest) -> Result<GenerateResponse, ClientError> {
        self.requests.lock().unwrap().push(request.clone());
        Ok(GenerateResponse::Generated(Box::new(GeneratedResponse {
            id: None,
            text: self.text.clone(),
            model: "invented-model".to_owned(),
            usage: json!({"input_tokens": 900, "output_tokens": 120, "total_tokens": 1020}),
            finish_reason: "stop".to_owned(),
            thinking: None,
            schema_validation: Some(json!({"valid": true, "errors": []})),
            input_budget: self.input_budget.clone(),
            request_budget: None,
            inference: None,
            hints_applied: Vec::new(),
        })))
    }
}

/// A journal holding one activity over one segment of the case's files, and
/// the shipped talent it names; `notes` is a custom copy of the Work Story.
fn journal(root: &Path, case: &Value) -> (RuntimePaths, ExecutionContext) {
    let journal = root.join("journal");
    let segment = journal
        .join("chronicle")
        .join(DAY)
        .join(case["stream"].as_str().unwrap())
        .join(SEGMENT);
    fs::create_dir_all(&segment).unwrap();
    for (name, rows) in case["files"].as_object().unwrap() {
        let lines = rows
            .as_array()
            .unwrap()
            .iter()
            .map(|row| match row {
                Value::String(line) => format!("{line}\n"),
                row => format!("{row}\n"),
            })
            .collect::<String>();
        fs::write(segment.join(name), lines).unwrap();
    }
    fs::create_dir_all(journal.join("config")).unwrap();
    fs::write(
        journal.join("config/journal.json"),
        r#"{"identity":{"name":"Rowan Vale"},"providers":{"active":{"provider":"test","model":"test-model"}}}"#,
    )
    .unwrap();
    fs::create_dir_all(journal.join(format!("facets/{FACET}/activities"))).unwrap();
    fs::write(
        journal.join(format!("facets/{FACET}/facet.json")),
        r#"{"name":"work","description":"Work"}"#,
    )
    .unwrap();
    fs::write(
        journal.join(format!("facets/{FACET}/activities/{DAY}.jsonl")),
        format!("{}\n", activity(case)),
    )
    .unwrap();
    let payload = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../payload/solstone");
    let talent_root = root.join("talent");
    fs::create_dir_all(&talent_root).unwrap();
    let talent = case["talent"].as_str().unwrap();
    let (source, schema) = match talent {
        "conversation" => ("conversation", "conversation.schema.json"),
        _ => ("work", "story.schema.json"),
    };
    fs::copy(
        payload.join(format!("talent/{source}.md")),
        talent_root.join(format!("{talent}.md")),
    )
    .unwrap();
    fs::copy(
        payload.join("talent").join(schema),
        talent_root.join(schema),
    )
    .unwrap();
    fs::create_dir_all(root.join("apps")).unwrap();
    (
        RuntimePaths {
            talent_root,
            apps_root: root.join("apps"),
            templates_dir: payload.join("think/templates"),
        },
        ExecutionContext { journal },
    )
}

fn activity(case: &Value) -> Value {
    json!({
        "id": ACTIVITY,
        "activity": case["activity_kind"],
        "segments": [SEGMENT],
        "stream": case["stream"],
        "description": "An invented activity",
    })
}

struct Run {
    outcome: RuntimeOutcome,
    record: Map<String, Value>,
    requests: Vec<GenerateRequest>,
    response: String,
}

fn run(case: &Value) -> Run {
    let root = tempfile::tempdir().unwrap();
    let (paths, context) = journal(root.path(), case);
    let response = case["response"].to_string();
    let executor = Scripted {
        text: response.clone(),
        input_budget: case.get("input_budget").cloned(),
        requests: Mutex::new(Vec::new()),
    };
    let mut request = json!({
        "name": case["talent"],
        "day": DAY,
        "facet": FACET,
        "activity": activity(case),
        "span": [SEGMENT],
    });
    if let Some(extra) = case.get("request").and_then(Value::as_object) {
        request.as_object_mut().unwrap().extend(extra.clone());
    }
    let outcome = execute_request(
        request.as_object().unwrap().clone(),
        &paths,
        &context,
        &executor,
        &mut Vec::new(),
        &solstone_core_generate::Cancellation::never(),
    );
    let record = solstone_core_facets::get_activity_record(&context.journal, FACET, DAY, ACTIVITY)
        .unwrap()
        .unwrap();
    Run {
        outcome,
        record,
        requests: executor.requests.into_inner().unwrap(),
        response,
    }
}

fn cases() -> Vec<Value> {
    serde_json::from_str::<Value>(CASES).unwrap()["cases"]
        .as_array()
        .unwrap()
        .clone()
}

#[test]
fn every_story_source_case_stores_and_returns_one_story() {
    for case in cases() {
        let name = case["name"].as_str().unwrap();
        let run = run(&case);
        let expect = &case["expect"];
        let model = &case["response"];
        // The ordinary request, once: the body adds no inference.
        assert_eq!(
            run.requests.len(),
            1,
            "{name}: one generate call: {:?}",
            run.outcome
        );
        let RuntimeOutcome::Finished {
            output,
            disposition,
            ..
        } = run.outcome
        else {
            panic!("{name}: the story run did not finish: {:?}", run.outcome);
        };
        if expect["rejected"] == true {
            assert_eq!(disposition, CommitDisposition::RejectedNoMutation, "{name}");
            assert_eq!(
                output, run.response,
                "{name}: a rejected run returns the response"
            );
            assert!(
                !run.record.contains_key("story"),
                "{name}: nothing is stored"
            );
            continue;
        }
        assert_eq!(disposition, CommitDisposition::CommittedNoOutput, "{name}");
        let story = &run.record["story"];
        let quoted = expect.get("body").and_then(Value::as_str);
        let body = quoted.unwrap_or_else(|| model["body"].as_str().unwrap().trim());
        assert_eq!(story["body"], body, "{name}: stored body");
        // Topics, confidence and relations are always the model's.
        let mut topics = Vec::<String>::new();
        for topic in model["topics"].as_array().unwrap() {
            let topic = topic.as_str().unwrap().trim().to_lowercase();
            if !topic.is_empty() && !topics.contains(&topic) {
                topics.push(topic);
            }
        }
        assert_eq!(story["topics"], json!(topics), "{name}: topics");
        let confidence = model["confidence"].as_f64().unwrap().clamp(0.0, 1.0);
        assert_eq!(story["confidence"], json!(confidence), "{name}: confidence");
        let relations = run.record["relations"].as_array().unwrap();
        let expected = model["relations"].as_array().unwrap();
        assert_eq!(relations.len(), expected.len(), "{name}: relations kept");
        for (stored, sent) in relations.iter().zip(expected) {
            for field in ["from", "to", "kind", "note", "quote"] {
                assert_eq!(stored[field], sent[field], "{name}: relation {field}");
            }
        }
        assert_eq!(
            story.get("partial_input").is_some(),
            expect["partial"] == true,
            "{name}: partial input"
        );
        if quoted.is_some() {
            // What the run returns is the Story it stored.
            let returned: Value = serde_json::from_str(&output).unwrap();
            assert_eq!(returned["body"], story["body"], "{name}: returned body");
            assert_eq!(
                returned["topics"], story["topics"],
                "{name}: returned topics"
            );
            assert_eq!(
                returned["confidence"], story["confidence"],
                "{name}: returned confidence"
            );
            assert_eq!(returned["relations"], model["relations"], "{name}");
        } else {
            assert_eq!(
                output, run.response,
                "{name}: the response is returned as is"
            );
        }
    }
}
