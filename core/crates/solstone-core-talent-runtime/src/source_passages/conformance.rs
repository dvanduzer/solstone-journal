// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! The source-passage conformance harness and its regression cases.
//!
//! The harness checks an extraction against its units: every passage must be
//! reproduced from its unit at its offsets and hash within the budget, every
//! protected reference must survive whole inside one passage, a qualified
//! claim may never appear without its qualifiers, and the calls are counted
//! physically and logically with missing usage kept missing.

use std::fs;
use std::path::Path;
use std::sync::Mutex;

use serde_json::{Value, json};
use solstone_core_generate::{
    ClientError, ContentPart, GenerateRequest, GenerateResponse, GeneratedResponse, RefusalReason,
    RefusedResponse,
};
use solstone_core_talent_cli::preview::PreviewRequest;

use super::*;
use crate::ExecutionContext;
use crate::prepare::RuntimePaths;

const CASES: &str = include_str!("conformance.json");
const DAY: &str = "20260101";
const SEGMENT: &str = "090000_900";
const FACET: &str = "work";
const ACTIVITY: &str = "meeting_090000_900";

/// What the harness found wrong. Empty in every field is a pass.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Findings {
    /// References whose support is not verbatim in one unit; a fixture fault.
    pub unsupported_references: Vec<String>,
    /// Protected references no single passage holds whole.
    pub lost: Vec<String>,
    /// Passages not reproduced from their unit, or out of bounds.
    pub unreproduced: Vec<String>,
    /// Passages asserting a claim without its qualifiers, or forbidden text.
    pub unsupported_assertions: Vec<String>,
}

impl Findings {
    fn clean(&self) -> bool {
        *self == Self::default()
    }
}

pub(crate) struct Reference {
    pub id: String,
    pub useful: bool,
    pub support: Vec<String>,
}

pub(crate) struct Qualified {
    pub claim: String,
    pub with: Vec<String>,
}

fn passages_of(outcome: &UnitOutcome) -> &[Passage] {
    match outcome {
        UnitOutcome::Whole { passage } => std::slice::from_ref(passage),
        UnitOutcome::Selected { passages, .. } => passages,
        _ => &[],
    }
}

pub(crate) fn evaluate(
    units: &[SourceUnit],
    extraction: &Extraction,
    references: &[Reference],
    qualified: &[Qualified],
    forbidden: &[String],
) -> Findings {
    let mut findings = Findings::default();
    for (index, (unit, outcome)) in units.iter().zip(&extraction.outcomes).enumerate() {
        let passages = passages_of(outcome);
        if rendered_chars(passages) > MAX_RENDERED_CHARS {
            findings
                .unreproduced
                .push(format!("unit {index}: over the rendered budget"));
        }
        for passage in passages {
            if unit.text.get(passage.start..passage.end) != Some(passage.text.as_str()) {
                findings.unreproduced.push(format!(
                    "unit {index}: {}..{} is not its source",
                    passage.start, passage.end
                ));
            }
        }
    }
    let document = document(units, extraction, Map::new());
    for (index, row) in document["units"].as_array().unwrap().iter().enumerate() {
        if row["source_sha256"] != json!(sha256(&units[index].text)) {
            findings
                .unreproduced
                .push(format!("unit {index}: source hash disagrees"));
        }
    }
    let all = extraction
        .outcomes
        .iter()
        .flat_map(passages_of)
        .collect::<Vec<_>>();
    for reference in references {
        let supported = units.iter().any(|unit| {
            reference
                .support
                .iter()
                .all(|fragment| unit.text.contains(fragment))
        });
        if !supported {
            findings.unsupported_references.push(reference.id.clone());
            continue;
        }
        if reference.useful
            && !all.iter().any(|passage| {
                reference
                    .support
                    .iter()
                    .all(|fragment| passage.text.contains(fragment))
            })
        {
            findings.lost.push(reference.id.clone());
        }
    }
    for passage in &all {
        for claim in qualified {
            if passage.text.contains(&claim.claim)
                && let Some(missing) = claim
                    .with
                    .iter()
                    .find(|qualifier| !passage.text.contains(*qualifier))
            {
                findings
                    .unsupported_assertions
                    .push(format!("'{}' without '{missing}'", claim.claim));
            }
        }
        for text in forbidden {
            if passage.text.contains(text) {
                findings
                    .unsupported_assertions
                    .push(format!("forbidden '{text}'"));
            }
        }
    }
    findings
}

/// Completions answered in order, with every request kept.
struct Scripted {
    completions: Mutex<Vec<Result<GenerateResponse, String>>>,
    requests: Mutex<Vec<GenerateRequest>>,
}

impl Scripted {
    fn new(mut completions: Vec<Result<GenerateResponse, String>>) -> Self {
        completions.reverse();
        Self {
            completions: Mutex::new(completions),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn requests(&self) -> Vec<GenerateRequest> {
        self.requests.lock().unwrap().clone()
    }
}

impl solstone_core_generate::GenerateExecutor for Scripted {
    fn execute(&self, request: &GenerateRequest) -> Result<GenerateResponse, ClientError> {
        self.requests.lock().unwrap().push(request.clone());
        self.completions
            .lock()
            .unwrap()
            .pop()
            .expect("an unscripted completion was requested")
            .map_err(ClientError::Resolve)
    }
}

fn generated(text: &str, usage: Value) -> GenerateResponse {
    GenerateResponse::Generated(Box::new(GeneratedResponse {
        id: None,
        text: text.to_owned(),
        model: "invented-model".to_owned(),
        usage,
        finish_reason: "stop".to_owned(),
        thinking: None,
        schema_validation: Some(json!({"valid": true, "errors": []})),
        input_budget: None,
        request_budget: None,
        inference: None,
        hints_applied: Vec::new(),
    }))
}

fn usage() -> Value {
    json!({"input_tokens": 900, "output_tokens": 120, "total_tokens": 1020})
}

fn refused(blocking: bool) -> GenerateResponse {
    GenerateResponse::Refused(RefusedResponse {
        id: None,
        reason: RefusalReason::ProviderResponseInvalid,
        reason_code: None,
        retryable: !blocking,
        blocking,
        reset_at_ms: None,
        provider: None,
        detail: "invented refusal".to_owned(),
    })
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .map(|item| item.as_str().unwrap().to_owned())
                .collect()
        })
        .unwrap_or_default()
}

fn expand_filler(items: &[Value]) -> Vec<Value> {
    let mut output = Vec::new();
    for item in items {
        if let Some(count) = item.get("filler").and_then(Value::as_u64) {
            let template = item["text"].as_str().unwrap();
            for index in 0..count {
                output.push(json!(template.replace("{i}", &index.to_string())));
            }
        } else {
            output.push(item.clone());
        }
    }
    output
}

/// A journal holding one meeting whose audio is the case's rows, and the
/// shipped talent the case names.
fn journal(root: &Path, case: &Value) -> (RuntimePaths, ExecutionContext) {
    let journal = root.join("journal");
    let day = journal.join("chronicle").join(DAY);
    let stream = case.get("stream").and_then(Value::as_str);
    let segment = stream
        .map_or_else(|| day.clone(), |stream| day.join(stream))
        .join(SEGMENT);
    fs::create_dir_all(&segment).unwrap();
    // Another stream holding the same segment key, which the activity does not name.
    if let Some(other) = case.get("other_stream") {
        let directory = day.join(other["stream"].as_str().unwrap()).join(SEGMENT);
        fs::create_dir_all(&directory).unwrap();
        fs::write(
            directory.join("capture_audio.jsonl"),
            format!("{}\n", json!({"start": "00:00:03", "text": other["text"]})),
        )
        .unwrap();
    }
    fs::create_dir_all(journal.join("config")).unwrap();
    fs::create_dir_all(journal.join(format!("facets/{FACET}/activities"))).unwrap();
    fs::write(
        journal.join(format!("facets/{FACET}/facet.json")),
        r#"{"name":"work","description":"Work"}"#,
    )
    .unwrap();
    let kind = case["activity_kind"].as_str().unwrap();
    fs::write(
        journal.join(format!("facets/{FACET}/activities/{DAY}.jsonl")),
        format!(
            "{}\n",
            match stream {
                Some(stream) => {
                    json!({"id": ACTIVITY, "activity": kind, "segments": [SEGMENT], "stream": stream})
                }
                None => json!({"id": ACTIVITY, "activity": kind, "segments": [SEGMENT]}),
            }
        ),
    )
    .unwrap();
    let mut rows = String::new();
    let mut second = 0;
    for item in expand_filler(case["audio"].as_array().unwrap()) {
        let (start, text) = match &item {
            Value::String(text) => {
                second += 7;
                (
                    format!("00:{:02}:{:02}", second / 60, second % 60),
                    text.clone(),
                )
            }
            row => (
                row["start"].as_str().unwrap().to_owned(),
                row["text"].as_str().unwrap().to_owned(),
            ),
        };
        rows.push_str(&json!({"start": start, "text": text}).to_string());
        rows.push('\n');
    }
    fs::write(segment.join("capture_audio.jsonl"), rows).unwrap();
    let payload = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../payload/solstone");
    let talent_root = root.join("talent");
    fs::create_dir_all(&talent_root).unwrap();
    let talent = case["talent"].as_str().unwrap();
    let schema = if talent == "work" {
        "story.schema.json"
    } else {
        "conversation.schema.json"
    };
    for name in [format!("{talent}.md"), schema.to_owned()] {
        fs::copy(payload.join("talent").join(&name), talent_root.join(&name)).unwrap();
    }
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

struct Run {
    units: Vec<SourceUnit>,
    extraction: Extraction,
    requests: Vec<GenerateRequest>,
}

fn run_case(case: &Value) -> Run {
    let executor = Scripted::new(
        case["completions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|completion| Ok(generated(&completion.to_string(), usage())))
            .collect(),
    );
    let talent = case["talent"].as_str().unwrap();
    let (units, extraction) = if case.get("audio").is_some() {
        let root = tempfile::tempdir().unwrap();
        let (paths, context) = journal(root.path(), case);
        let request = PreviewRequest {
            name: talent.to_owned(),
            day: Some(DAY.to_owned()),
            segment: None,
            facet: Some(FACET.to_owned()),
            activity: Some(ACTIVITY.to_owned()),
        };
        let result = activity_passages(&request, &paths, &context, &executor)
            .unwrap_or_else(|preview| panic!("activity did not prepare: {preview:?}"));
        (result.units, result.extraction)
    } else {
        let transcript = expand_filler(case["transcript"].as_array().unwrap())
            .iter()
            .map(|part| part.as_str().unwrap())
            .collect::<String>();
        let units = source_units(&transcript, case["activity_kind"].as_str().unwrap());
        let extraction = extract(&units, talent, &executor);
        (units, extraction)
    };
    Run {
        units,
        extraction,
        requests: executor.requests(),
    }
}

fn references(case: &Value) -> Vec<Reference> {
    case["references"]
        .as_array()
        .unwrap()
        .iter()
        .map(|reference| Reference {
            id: reference["id"].as_str().unwrap().to_owned(),
            useful: reference["useful"].as_bool().unwrap(),
            support: strings(&reference["support"]),
        })
        .collect()
}

fn qualified(case: &Value) -> Vec<Qualified> {
    case["qualified"]
        .as_array()
        .unwrap()
        .iter()
        .map(|claim| Qualified {
            claim: claim["claim"].as_str().unwrap().to_owned(),
            with: strings(&claim["with"]),
        })
        .collect()
}

fn outcome_name(outcome: &UnitOutcome) -> &'static str {
    match outcome {
        UnitOutcome::Whole { .. } => "whole",
        UnitOutcome::Selected { .. } => "selected",
        UnitOutcome::Failed { .. } => "failed",
        UnitOutcome::NotAttempted => "not_attempted",
    }
}

fn cases() -> Vec<Value> {
    serde_json::from_str::<Value>(CASES).unwrap()["cases"]
        .as_array()
        .unwrap()
        .clone()
}

#[test]
fn every_regression_case_keeps_its_references_and_asserts_nothing_unsupported() {
    for case in cases() {
        let name = case["name"].as_str().unwrap();
        let run = run_case(&case);
        let findings = evaluate(
            &run.units,
            &run.extraction,
            &references(&case),
            &qualified(&case),
            &strings(&case["forbidden"]),
        );
        assert!(findings.clean(), "{name}: {findings:#?}");
        let expect = &case["expect"];
        assert_eq!(
            run.extraction.calls.len() as u64,
            expect["physical_calls"].as_u64().unwrap(),
            "{name}"
        );
        assert_eq!(run.requests.len(), run.extraction.calls.len(), "{name}");
        assert_eq!(
            run.extraction.logical_calls() as u64,
            expect["logical_calls"].as_u64().unwrap(),
            "{name}"
        );
        assert_eq!(
            run.extraction.complete(),
            expect["complete"].as_bool().unwrap(),
            "{name}"
        );
        assert_eq!(
            run.extraction
                .outcomes
                .iter()
                .map(outcome_name)
                .collect::<Vec<_>>(),
            strings(&expect["outcomes"]),
            "{name}"
        );
        for request in &run.requests {
            assert_eq!(request.temperature, NOMINATION_TEMPERATURE);
            assert_eq!(request.max_output_tokens, NOMINATION_MAX_OUTPUT_TOKENS);
            assert_eq!(request.context, NOMINATION_CONTEXT);
            assert!(request.json_output && request.json_schema.is_some());
            let ContentPart::Text { text } = &request.contents[0] else {
                panic!("source is text");
            };
            assert!(run.units.iter().any(|unit| unit.text == *text), "{name}");
        }
    }
}

#[test]
fn the_harness_detects_a_lost_reference_a_clipped_qualifier_and_altered_text() {
    let case = cases()
        .into_iter()
        .find(|case| case["name"] == "long_meeting_copies_source_not_the_strengthened_draft")
        .unwrap();
    let run = run_case(&case);
    let check = |extraction: &Extraction| {
        evaluate(
            &run.units,
            extraction,
            &references(&case),
            &qualified(&case),
            &strings(&case["forbidden"]),
        )
    };
    assert!(check(&run.extraction).clean());
    let unit_text = &run.units[0].text;
    let edit = |change: &dyn Fn(&mut Vec<Passage>)| {
        let mut extraction = run.extraction.clone();
        if let UnitOutcome::Selected { passages, .. } = &mut extraction.outcomes[0] {
            change(passages);
        }
        extraction
    };

    // A passage that used to hold a reference is dropped.
    let dropped = edit(&|passages| {
        passages.retain(|passage| !passage.text.contains("billing table"));
    });
    assert_eq!(check(&dropped).lost, vec!["negation".to_owned()]);

    // A passage is cut so its action survives without its condition.
    let clipped = edit(&|passages| {
        for passage in passages.iter_mut() {
            if let Some(at) = passage.text.find(" only after both suites pass") {
                passage.text.truncate(at);
                passage.end = passage.start + at;
            }
        }
    });
    let findings = check(&clipped);
    assert_eq!(findings.lost, vec!["condition".to_owned()]);
    assert!(
        findings
            .unsupported_assertions
            .iter()
            .any(|finding| finding.contains("only after both suites pass"))
    );
    assert!(findings.unreproduced.is_empty());

    // A passage is reworded into the stronger draft the model offered.
    let reworded = edit(&|passages| {
        for passage in passages.iter_mut() {
            passage.text = passage.text.replace("could deploy", "deployed");
        }
    });
    let findings = check(&reworded);
    assert!(!findings.unreproduced.is_empty());
    assert!(findings.lost.contains(&"condition".to_owned()));
    assert!(unit_text.contains("could deploy"));

    // A reference with no source support is a fixture fault, never protected.
    let invented = [Reference {
        id: "invented".to_owned(),
        useful: true,
        support: vec!["Lee deployed v14".to_owned()],
    }];
    let findings = evaluate(&run.units, &run.extraction, &invented, &[], &[]);
    assert_eq!(findings.unsupported_references, vec!["invented".to_owned()]);
    assert!(findings.lost.is_empty());
}

fn long_unit(text: &str) -> SourceUnit {
    SourceUnit {
        text: format!(
            "Activity type: meeting\nSource channel: Transcript\n{}{text}\n",
            "Ari: Lunch chatter about weather, parking garages and coffee.\n".repeat(70)
        ),
        kind: SourceKind::Audio,
        timestamp: Some("2026-01-01 09:00:00 - 09:15:00".to_owned()),
        untimed: false,
        segment: 1,
        channel: 1,
        chunk: 1,
    }
}

#[test]
fn a_fitting_unit_never_reaches_the_model() {
    let unit = SourceUnit {
        text: "x".repeat(MAX_RENDERED_CHARS - PASSAGE_PREFIX.chars().count()),
        ..long_unit("")
    };
    assert!(nomination_request(&unit, "conversation").is_none());
    let one_more = SourceUnit {
        text: format!("{}x", unit.text),
        ..unit.clone()
    };
    assert!(nomination_request(&one_more, "conversation").is_some());
    let executor = Scripted::new(Vec::new());
    let extraction = extract(std::slice::from_ref(&unit), "conversation", &executor);
    assert!(executor.requests().is_empty());
    assert_eq!(
        extraction.outcomes,
        vec![UnitOutcome::Whole {
            passage: Passage {
                start: 0,
                end: unit.text.len(),
                text: unit.text.clone()
            }
        }]
    );
}

#[test]
fn a_completion_that_is_not_a_finished_valid_nomination_selects_nothing() {
    let unit = long_unit("Mira: Lee could deploy v14 only after both suites pass.");
    let finished = |text: &str| generated(text, usage());
    let with = |change: &dyn Fn(&mut GeneratedResponse)| {
        let GenerateResponse::Generated(mut response) = finished(r#"{"facts":["Lee deploy v14"]}"#)
        else {
            unreachable!()
        };
        change(&mut response);
        GenerateResponse::Generated(response)
    };
    let cases: Vec<(GenerateResponse, &str)> = vec![
        (finished("not json"), "malformed"),
        (finished(r#"[]"#), "malformed"),
        (finished(r#"{"facts":[1]}"#), "malformed"),
        (finished(r#"{"facts":[""]}"#), "malformed"),
        (
            finished(&json!({"facts": ["x".repeat(321)]}).to_string()),
            "malformed",
        ),
        (
            finished(&json!({"facts": vec!["Lee deploy v14"; 13]}).to_string()),
            "malformed",
        ),
        (finished(r#"{"facts":[],"extra":1}"#), "malformed"),
        (
            with(&|response| response.finish_reason = "max_tokens".to_owned()),
            "incomplete",
        ),
        (
            with(&|response| response.input_budget = Some(json!({"clipped": true}))),
            "clipped_input",
        ),
        (
            with(&|response| {
                response.schema_validation = Some(json!({"valid": false, "errors": ["x"]}))
            }),
            "schema_invalid",
        ),
        (refused(false), "refused"),
    ];
    for (response, kind) in cases {
        let executor = Scripted::new(vec![Ok(response)]);
        let extraction = extract(std::slice::from_ref(&unit), "conversation", &executor);
        let row = document(std::slice::from_ref(&unit), &extraction, Map::new());
        assert_eq!(row["units"][0]["outcome"], "failed", "{kind}");
        assert_eq!(row["units"][0]["failure"]["kind"], kind);
        assert_eq!(row["units"][0]["passages"], json!([]));
        assert_eq!(row["complete"], false);
        assert_eq!(executor.requests().len(), 1, "no retry for {kind}");
    }
}

#[test]
fn a_blocking_refusal_or_unavailable_boundary_stops_further_requests() {
    let units = [
        long_unit("Mira: first."),
        long_unit("Mira: second."),
        SourceUnit {
            text: "short".to_owned(),
            ..long_unit("")
        },
    ];
    for first in [Ok(refused(true)), Err("no generate child".to_owned())] {
        let executor = Scripted::new(vec![first]);
        let extraction = extract(&units, "conversation", &executor);
        assert_eq!(executor.requests().len(), 1);
        assert!(matches!(extraction.outcomes[0], UnitOutcome::Failed { .. }));
        assert_eq!(extraction.outcomes[1], UnitOutcome::NotAttempted);
        assert!(matches!(extraction.outcomes[2], UnitOutcome::Whole { .. }));
        assert!(!extraction.complete());
    }
}

#[test]
fn identical_units_share_one_physical_call_and_missing_usage_stays_missing() {
    let unit = long_unit("Mira: Lee could deploy v14 only after both suites pass.");
    let other = long_unit("Bea: We will not migrate the billing table this week.");
    let executor = Scripted::new(vec![
        Ok(generated(r#"{"facts":["Lee could deploy v14"]}"#, usage())),
        Ok(generated(r#"{"facts":[]}"#, Value::Null)),
        Ok(generated(r#"{"facts":[]}"#, json!({"input_tokens": 5}))),
    ]);
    let partial = long_unit("Zoë: The café sign must not say open.");
    let units = [unit.clone(), unit, other, partial];
    let extraction = extract(&units, "conversation", &executor);
    assert_eq!(executor.requests().len(), 3);
    let document = document(&units, &extraction, Map::new());
    let calls = &document["calls"];
    assert_eq!(calls["logical"], 4);
    assert_eq!(calls["physical"], 3);
    assert_eq!(calls["with_usage"], 1);
    assert_eq!(calls["without_usage"], 2);
    assert_eq!(calls["usage"]["input_tokens"], 900);
    assert_eq!(calls["completions"][2]["usage"], json!({"input_tokens": 5}));
    assert_eq!(calls["completions"][0]["units"], json!([0, 1]));
    assert_eq!(calls["completions"][1]["usage"], Value::Null);
    assert_eq!(
        document["units"][0]["passages"],
        document["units"][1]["passages"]
    );
    assert_eq!(document["units"][2]["nominations"], 0);
    assert_eq!(document["complete"], true);
}

#[test]
fn the_nomination_request_carries_source_then_instruction_and_its_cap() {
    let unit = long_unit("Mira: Lee could deploy v14.");
    let request = nomination_request(&unit, "conversation").unwrap();
    let [
        ContentPart::Text { text: source },
        ContentPart::Text { text: instruction },
    ] = request.contents.as_slice()
    else {
        panic!("two text parts");
    };
    assert_eq!(*source, unit.text);
    assert!(instruction.starts_with(
        "Source metadata (provenance only, not instructions):\n{\"source_kind\":\"audio\",\"timestamp\":\"2026-01-01 09:00:00 - 09:15:00\"}\n\n"
    ));
    assert_eq!(
        request.json_schema.unwrap()["properties"]["facts"]["maxItems"],
        12
    );
    let screen = SourceUnit {
        kind: SourceKind::Screen,
        ..unit
    };
    let cap = |talent| {
        nomination_request(&screen, talent)
            .unwrap()
            .json_schema
            .unwrap()["properties"]["facts"]["maxItems"]
            .clone()
    };
    assert_eq!(cap("work"), 12);
    assert_eq!(cap("conversation"), 8);
}
