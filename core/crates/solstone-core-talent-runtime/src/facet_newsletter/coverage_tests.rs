// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! A newsletter's coverage status follows the stories its packet shows.
//!
//! Each case writes one activity's Story through the native caller with a
//! scripted completion, clipped or whole, then runs the shipped newsletter on
//! a scripted completion through the ordinary and the frozen daily paths.

use std::fs;
use std::path::Path;
use std::sync::Mutex;

use serde_json::{Map, Value, json};
use solstone_core_journal_io::{
    DailyUnitIdentity, DailyUnitRecord, load_daily_unit_record, save_daily_unit_record,
};

use crate::prepare::{PrepareMode, RuntimePaths};
use crate::story_source_conformance::{
    ACTIVITY, DAY, FACET, SEGMENT, Scripted, activity, cases, journal,
};
use crate::{ExecutionContext, RuntimeOutcome, execute_request};

const NEWSLETTER: &str = "# Work\n\nYou edited chapter three of your novel.";

fn clipped() -> Value {
    json!({"clipped": true, "dropped_entries": 3, "dropped_chars": 90000, "budget_tokens": 4096})
}

fn partial() -> Value {
    json!({"reason": "partial_input", "activities": [ACTIVITY]})
}

fn scripted(text: &str, input_budget: Option<Value>) -> Scripted {
    Scripted {
        text: text.to_owned(),
        input_budget,
        requests: Mutex::new(Vec::new()),
    }
}

fn never() -> solstone_core_generate::Cancellation<'static> {
    solstone_core_generate::Cancellation::never()
}

fn terminal_case() -> Value {
    cases()
        .into_iter()
        .find(|case| case["name"] == "a_terminal_pane_keeps_the_model_body")
        .unwrap()
}

/// A journal whose one activity has a Story written through the native caller.
fn story_journal(root: &Path, input_budget: Option<Value>) -> (RuntimePaths, ExecutionContext) {
    let case = terminal_case();
    let (paths, context) = journal(root, &case);
    let payload = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../payload/solstone");
    fs::copy(
        payload.join("talent/facet_newsletter.md"),
        paths.talent_root.join("facet_newsletter.md"),
    )
    .unwrap();
    write_story(&paths, &context, input_budget);
    (paths, context)
}

fn write_story(paths: &RuntimePaths, context: &ExecutionContext, input_budget: Option<Value>) {
    let case = terminal_case();
    let executor = scripted(&case["response"].to_string(), input_budget);
    let request = json!({
        "name": case["talent"], "day": DAY, "facet": FACET,
        "activity": activity(&case), "span": [SEGMENT],
    });
    let outcome = execute_request(
        request.as_object().unwrap().clone(),
        paths,
        context,
        &executor,
        &mut Vec::new(),
        &never(),
    );
    assert!(
        matches!(outcome, RuntimeOutcome::Finished { .. }),
        "{outcome:?}"
    );
}

fn story(context: &ExecutionContext) -> Map<String, Value> {
    solstone_core_facets::get_activity_record(&context.journal, FACET, DAY, ACTIVITY)
        .unwrap()
        .unwrap()["story"]
        .as_object()
        .unwrap()
        .clone()
}

/// Replace the stored story's fields, as a test journal edit.
fn set_story(context: &ExecutionContext, fields: Value) {
    let path = context
        .journal
        .join(format!("facets/{FACET}/activities/{DAY}.jsonl"));
    let mut record: Value =
        serde_json::from_str(fs::read_to_string(&path).unwrap().trim()).unwrap();
    for (key, value) in fields.as_object().unwrap() {
        record["story"][key] = value.clone();
    }
    fs::write(&path, format!("{record}\n")).unwrap();
}

fn newsletter_request() -> Map<String, Value> {
    json!({"name": "facet_newsletter", "day": DAY, "facet": FACET})
        .as_object()
        .unwrap()
        .clone()
}

struct Published {
    degraded: Option<Value>,
    request: solstone_core_generate::GenerateRequest,
    news: String,
    finish: Value,
}

fn published(outcome: RuntimeOutcome, executor: Scripted, context: &ExecutionContext) -> Published {
    let RuntimeOutcome::Finished { degraded, .. } = &outcome else {
        panic!("expected a finished newsletter: {outcome:?}");
    };
    let degraded = degraded.as_deref().cloned();
    let mut events = Vec::new();
    crate::emit_outcome(&mut events, outcome);
    let requests = executor.requests.into_inner().unwrap();
    assert_eq!(requests.len(), 1, "one newsletter call");
    Published {
        degraded,
        request: requests[0].clone(),
        news: fs::read_to_string(
            context
                .journal
                .join(format!("facets/{FACET}/news/{DAY}.md")),
        )
        .unwrap(),
        finish: serde_json::from_slice(&events).unwrap(),
    }
}

fn ordinary(paths: &RuntimePaths, context: &ExecutionContext, extra: Value) -> Published {
    let executor = scripted(NEWSLETTER, None);
    let mut request = newsletter_request();
    request.extend(extra.as_object().unwrap().clone());
    let outcome = execute_request(
        request,
        paths,
        context,
        &executor,
        &mut Vec::new(),
        &never(),
    );
    published(outcome, executor, context)
}

fn freeze(paths: &RuntimePaths, context: &ExecutionContext) -> Value {
    let mut request = newsletter_request();
    request.insert("schedule".to_owned(), json!("daily"));
    let prepared = crate::prepare::prepare(request, paths, context, PrepareMode::Execute).unwrap();
    crate::daily_prepare::freeze(prepared, context).unwrap()
}

/// Run a frozen packet through daily execution as a fresh attempt.
fn daily(
    paths: &RuntimePaths,
    context: &ExecutionContext,
    packet: Value,
    evidence: &str,
) -> (Published, DailyUnitRecord) {
    let identity = DailyUnitIdentity::new(DAY, "facet_newsletter", Some(FACET.to_owned()));
    let mut record = DailyUnitRecord::new(identity.clone(), evidence, "C");
    record.lock_token = Some(evidence.to_owned());
    record.packet_digest = Some(crate::daily_prepare::packet_digest(&packet));
    record.frozen_packet = Some(packet);
    save_daily_unit_record(&context.journal, &record).unwrap();
    let mut request = newsletter_request();
    request.extend(
        json!({"lock_token": evidence, "evidence_revision": evidence, "contract_digest": "C"})
            .as_object()
            .unwrap()
            .clone(),
    );
    let executor = scripted(NEWSLETTER, None);
    let outcome = execute_request(
        request,
        paths,
        context,
        &executor,
        &mut Vec::new(),
        &never(),
    );
    let run = published(outcome, executor, context);
    let record = load_daily_unit_record(&context.journal, &identity)
        .unwrap()
        .unwrap();
    (run, record)
}

fn accepted_degraded(record: &DailyUnitRecord) -> Value {
    record
        .accepted
        .as_ref()
        .unwrap()
        .generated_result
        .as_ref()
        .unwrap()["degraded"]
        .clone()
}

#[test]
fn a_partial_story_reaches_the_newsletter_status_and_not_its_request() {
    let partial_root = tempfile::tempdir().unwrap();
    let intact_root = tempfile::tempdir().unwrap();
    let (partial_paths, partial_context) = story_journal(partial_root.path(), Some(clipped()));
    let (intact_paths, intact_context) = story_journal(intact_root.path(), None);

    // The story writer stored the same body; only one carries the marker.
    let (partial_story, intact_story) = (story(&partial_context), story(&intact_context));
    assert_eq!(partial_story["body"], intact_story["body"]);
    assert_eq!(
        partial_story["partial_input"],
        json!({"dropped_entries": 3, "dropped_chars": 90000})
    );
    assert!(!intact_story.contains_key("partial_input"));

    let shown = ordinary(&partial_paths, &partial_context, json!({}));
    let intact = ordinary(&intact_paths, &intact_context, json!({}));
    // The model request and the written newsletter are unchanged.
    assert_eq!(shown.request, intact.request);
    assert_eq!(shown.news, intact.news);
    assert_eq!(shown.news, NEWSLETTER.trim());
    // Only the runtime status tells them apart, on the finish event the day
    // index records.
    assert_eq!(shown.degraded, Some(partial()));
    assert_eq!(shown.finish["degraded"], partial());
    assert_eq!(intact.degraded, None);
    assert!(intact.finish.get("degraded").is_none());
    // Absent optional sources are in both packets and degrade neither.
    let text = format!("{:?}", intact.request);
    assert!(
        text.contains("missing: activity_narrative absent"),
        "{text}"
    );
    assert!(
        text.contains("missing: facet_entities:detected absent"),
        "{text}"
    );
}

#[test]
fn the_daily_snapshot_reports_the_coverage_it_froze_and_a_whole_rerun_clears_it() {
    let root = tempfile::tempdir().unwrap();
    let (paths, context) = story_journal(root.path(), Some(clipped()));
    let frozen = freeze(&paths, &context);
    assert_eq!(
        frozen.pointer("/state/FacetNewsletter/packet/partial_stories"),
        Some(&json!([ACTIVITY]))
    );

    // A whole Story rerun after the freeze does not change what was used.
    write_story(&paths, &context, None);
    assert!(!story(&context).contains_key("partial_input"));
    let (run, record) = daily(&paths, &context, frozen.clone(), "E1");
    assert_eq!(run.degraded, Some(partial()));
    assert_eq!(accepted_degraded(&record), partial());

    // The next preparation reads the whole story, and the status clears.
    let (rerun, record) = daily(&paths, &context, freeze(&paths, &context), "E2");
    assert_eq!(rerun.degraded, None);
    assert_eq!(accepted_degraded(&record), Value::Null);
    assert_eq!(rerun.request, run.request);
    assert_eq!(rerun.news, run.news);

    // A packet frozen before this state existed still thaws, and says nothing.
    let mut earlier = frozen;
    earlier
        .pointer_mut("/state/FacetNewsletter/packet")
        .and_then(Value::as_object_mut)
        .unwrap()
        .remove("partial_stories");
    // It was frozen before any newsletter existed, so publication expects none.
    fs::remove_file(
        context
            .journal
            .join(format!("facets/{FACET}/news/{DAY}.md")),
    )
    .unwrap();
    let (earlier_run, _) = daily(&paths, &context, earlier, "E3");
    assert_eq!(earlier_run.degraded, None);
}

#[test]
fn only_a_shown_runtime_marker_counts_and_a_request_cannot_set_it() {
    let root = tempfile::tempdir().unwrap();
    let (paths, context) = story_journal(root.path(), None);
    for marker in [
        json!(null),
        json!(true),
        json!("partial"),
        json!([]),
        json!(3),
    ] {
        set_story(&context, json!({"partial_input": marker}));
        let run = ordinary(&paths, &context, json!({}));
        assert_eq!(run.degraded, None, "{marker}");
    }
    // A marker on a story the packet does not show describes nothing it used.
    set_story(
        &context,
        json!({"body": "", "partial_input": {"dropped_entries": 1, "dropped_chars": 10}}),
    );
    let run = ordinary(&paths, &context, json!({}));
    assert_eq!(run.degraded, None);
    // Request fields cannot set or forge the runtime-owned state.
    write_story(&paths, &context, None);
    let forged = ordinary(
        &paths,
        &context,
        json!({
            "degraded": partial(),
            "partial_stories": [ACTIVITY],
            "partial_input": {"dropped_entries": 1, "dropped_chars": 10},
            "input_budget": clipped(),
        }),
    );
    assert_eq!(forged.degraded, None);
}
