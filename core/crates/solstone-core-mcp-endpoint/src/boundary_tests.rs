// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

#![cfg(all(test, feature = "full-tests"))]

//! Purpose-built MCP read-boundary fixtures. These never use the development
//! journal fixture because each case needs isolated, adversarial journal state.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::params;
use serde_json::{Value, json};
use solstone_core_format::content::{Family, produce_chunks};
use solstone_core_indexer_query::{AdmittedCategory, ConnectionScope};

use crate::dispatch::{DispatchPrincipal, McpProbeError, run_mcp_probe};
use crate::permissions::{
    ConnectionReadSnapshot, PermissionDecision, PermissionStore, ReadPermission, ReadScope,
    evaluate_connection_read,
};
use crate::segment_links::{LinkSubject, TranscriptPageCheck, issue_segment_links};

const CONNECTION: &str = "bearer:boundary-test";
const FACET_A: &str = "123e4567-e89b-42d3-a456-426614174000";
const FACET_B: &str = "123e4567-e89b-42d3-a456-426614174001";
const DAY: &str = "20260914";
const STREAM: &str = "default";
const SEGMENT: &str = "090000_300";
const PATH: &str = "20260914/default/090000_300/talents/brief.md";

#[test]
fn ordinary_reads_keep_cross_source_memory_grants_and_own_recall_stays_scoped() {
    use crate::memory::{AppendResult, AuthenticatedMemorySource, append_connection_memory};
    use chrono::TimeZone;
    let journal = tempfile::Builder::new()
        .prefix("ordinary-memory-grant-")
        .tempdir_in(crate::test_scratch())
        .unwrap();
    let AppendResult::Stored(receipt) = append_connection_memory(
        journal.path(),
        AuthenticatedMemorySource {
            verified_id: "bearer:other-memory-writer",
            creation_label: "other writer",
        },
        "crossmemoryneedle other agent original",
        "ordinary-grant-operation",
        chrono::Utc.with_ymd_and_hms(2026, 1, 2, 12, 0, 0).unwrap(),
    )
    .unwrap() else {
        panic!("fixture writer must store");
    };
    solstone_core_indexer_store::scan::scan_journal(journal.path(), true).unwrap();
    PermissionStore::open(journal.path())
        .set_permission(
            CONNECTION,
            ReadPermission {
                categories: vec!["entities".into()],
                scope: ReadScope::WholeJournal,
            },
        )
        .unwrap();
    assert_eq!(
        probe(&journal, "search", json!({"query":"crossmemoryneedle"})),
        Err(McpProbeError::PermissionDenied)
    );
    PermissionStore::open(journal.path())
        .set_permission(CONNECTION, ReadPermission::default_whole_journal())
        .unwrap();
    let result = probe(&journal, "search", json!({"query":"crossmemoryneedle"})).unwrap();
    let results = result["results"].as_array().unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["origin"]["kind"], "agent_memory");
    assert_eq!(results[0]["origin"]["creation_label"], "other writer");
    assert!(results[0]["origin"].get("stream").is_none());
    assert!(results[0]["origin"].get("segment").is_none());
    let fetched = probe(
        &journal,
        "fetch",
        json!({"reference":results[0]["reference"]}),
    )
    .unwrap();
    assert_eq!(fetched["origin"], results[0]["origin"]);
    assert!(
        fetched["text"]
            .as_str()
            .unwrap()
            .contains("crossmemoryneedle other agent original")
    );
    let codec = crate::references::ReferenceCodec::new().unwrap();
    let own = crate::memory_recall::recall(
        journal.path(),
        &codec,
        CONNECTION,
        CONNECTION,
        "reader-credential",
        crate::memory_recall::RecallArgs::default(),
        chrono::NaiveDate::from_ymd_opt(2026, 1, 2).unwrap(),
    );
    assert!(own.complete);
    assert!(own.notes.is_empty());
    fs::remove_file(
        journal
            .path()
            .join("chronicle/20260102")
            .join(receipt.origin.stream)
            .join(receipt.origin.segment)
            .join("ready.json"),
    )
    .unwrap();
    assert!(
        probe(&journal, "search", json!({"query":"crossmemoryneedle"})).unwrap()["results"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        probe(
            &journal,
            "fetch",
            json!({"reference":results[0]["reference"]})
        ),
        Err(McpProbeError::Unavailable)
    );
}

fn fixture() -> tempfile::TempDir {
    let journal = tempfile::Builder::new()
        .prefix("solstone-mcp-boundary-")
        .tempdir_in(crate::test_scratch())
        .unwrap();
    for (name, id, title) in [("alpha", FACET_A, "Alpha"), ("beta", FACET_B, "Beta")] {
        let directory = journal.path().join("facets").join(name);
        fs::create_dir_all(&directory).unwrap();
        fs::write(
            directory.join("facet.json"),
            json!({"id": id, "title": title, "description": format!("{title} details")})
                .to_string(),
        )
        .unwrap();
    }
    let segment = journal
        .path()
        .join("chronicle")
        .join(DAY)
        .join(STREAM)
        .join(SEGMENT);
    fs::create_dir_all(segment.join("talents")).unwrap();
    fs::write(
        segment.join("talents/facets.json"),
        "[{\"facet\":\"alpha\"}]",
    )
    .unwrap();
    fs::write(
        segment.join("meeting_transcript.md"),
        "approved transcript\n",
    )
    .unwrap();
    fs::write(segment.join("audio.jsonl"), "percept material\n").unwrap();
    fs::write(segment.join("talents/brief.md"), "disk bytes\n").unwrap();
    for (directory, id, detached) in [
        ("primary", "entity-primary", false),
        ("same-name", "entity-same-name", false),
        ("detached", "entity-detached", true),
    ] {
        let entity = journal.path().join("entities").join(directory);
        fs::create_dir_all(&entity).unwrap();
        fs::write(
            entity.join("entity.json"),
            json!({
                "id": id,
                "name": "Same Name",
                "aka": ["forbidden aka"],
                "description": "out-of-scope description",
                "detached_facets": ["forbidden detached-facet list"],
                "decisions": ["forbidden cross-facet aggregate"],
            })
            .to_string(),
        )
        .unwrap();
        let link = journal.path().join("facets/alpha/entities").join(directory);
        fs::create_dir_all(&link).unwrap();
        fs::write(
            link.join("entity.json"),
            json!({"entity_id": id, "detached": detached}).to_string(),
        )
        .unwrap();
    }

    let index = solstone_core_indexer_store::db::open_index(journal.path()).unwrap();
    index
        .execute(
            "INSERT INTO chunks(content, path, day, facet, agent, stream, idx, time_bucket) \
             VALUES ('indexed bytes', ?1, ?2, '', 'fixture-stream', ?3, 0, '')",
            params![PATH, DAY, STREAM],
        )
        .unwrap();
    index
        .execute(
            "INSERT INTO chunk_classification(path, category, basis, eligible, unclassified) \
             VALUES (?1, 'transcripts', 'segment_assigned', 1, 0)",
            params![PATH],
        )
        .unwrap();
    index
        .execute(
            "INSERT INTO chunk_classification_facets(path, facet_id) VALUES (?1, ?2)",
            params![PATH, FACET_A],
        )
        .unwrap();
    index
        .execute(
            "REPLACE INTO chunk_classification_backfill(id, cursor, completed, stalled, stalled_path, resume_count) \
             VALUES (1, '', 1, 0, NULL, 0)",
            [],
        )
        .unwrap();
    index
        .execute(
            "REPLACE INTO index_build_state(id, schema_version, state, files_count, chunks_count) \
             VALUES (1, 1, 'complete', 1, 1)",
            [],
        )
        .unwrap();
    drop(index);

    PermissionStore::open(journal.path())
        .set_permission(
            CONNECTION,
            ReadPermission {
                categories: vec!["transcripts".to_owned()],
                scope: ReadScope::Facets {
                    ids: vec![FACET_A.to_owned()],
                },
            },
        )
        .unwrap();
    journal
}

fn probe(
    journal: &tempfile::TempDir,
    tool: &str,
    arguments: Value,
) -> Result<Value, McpProbeError> {
    run_mcp_probe(journal.path(), CONNECTION, tool, &arguments)
}

fn visible_search_page(result: &Value) -> Value {
    let results = result["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|result| {
            json!({
                "title": result["title"],
                "date": result["date"],
                "snippet": result["snippet"],
            })
        })
        .collect::<Vec<_>>();
    json!({"results": results, "coverage": result["coverage"]})
}

fn assert_no_coordinate_keys(value: &Value) {
    match value {
        Value::Array(values) => {
            for value in values {
                assert_no_coordinate_keys(value);
            }
        }
        Value::Object(values) => {
            for (key, value) in values {
                assert!(
                    !matches!(key.as_str(), "path" | "idx" | "row_id" | "rowid" | "stream"),
                    "agent result exposed internal key {key}"
                );
                assert_no_coordinate_keys(value);
            }
        }
        _ => {}
    }
}

#[test]
fn ac5_live_check_rejects_reassigned_segment_without_rescanning_the_index() {
    let journal = fixture();
    assert_eq!(
        probe(&journal, "search", json!({"query": "indexed"})).unwrap()["results"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        probe(&journal, "list_transcripts", json!({})).unwrap()["transcripts"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    let assignments = journal
        .path()
        .join("chronicle")
        .join(DAY)
        .join(STREAM)
        .join(SEGMENT)
        .join("talents/facets.json");
    fs::write(assignments, "[{\"facet\":\"beta\"}]").unwrap();

    assert!(
        probe(&journal, "search", json!({"query": "indexed"})).unwrap()["results"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        probe(&journal, "list_transcripts", json!({})).unwrap()["transcripts"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn ac10_fetch_returns_indexed_bytes_not_the_current_source_file() {
    let journal = fixture();
    let search = probe(&journal, "search", json!({"query": "indexed"})).unwrap();
    let reference = search["results"][0]["reference"].as_str().unwrap();
    let fetched = probe(&journal, "fetch", json!({"reference": reference})).unwrap();
    assert_eq!(fetched["text"], "indexed bytes");
    assert_ne!(fetched["text"], "disk bytes");
}

#[test]
fn ac23_chosen_scope_excludes_unassigned_segments_while_whole_journal_includes_them() {
    let journal = fixture();
    let assignments = journal
        .path()
        .join("chronicle")
        .join(DAY)
        .join(STREAM)
        .join(SEGMENT)
        .join("talents/facets.json");
    fs::write(&assignments, "[]").unwrap();
    assert!(
        probe(&journal, "list_transcripts", json!({})).unwrap()["transcripts"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    PermissionStore::open(journal.path())
        .set_permission(
            CONNECTION,
            ReadPermission {
                categories: vec!["transcripts".to_owned()],
                scope: ReadScope::WholeJournal,
            },
        )
        .unwrap();
    assert_eq!(
        probe(&journal, "list_transcripts", json!({})).unwrap()["transcripts"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn ac25_a_refusal_is_recorded_and_the_owner_can_tell_it_from_a_served_call() {
    // 🔑 This supersedes the three-field assertion: EVIDENCE.md § 5.3's gap was
    // that an owner asking "what did this connection try?" could not be told
    // "and it was refused". A denial and an admission differed only by
    // timestamp. They no longer do.
    let journal = fixture();
    let served = probe(&journal, "search", json!({"query": "indexed"}));
    assert!(served.is_ok());
    PermissionStore::open(journal.path())
        .clear_permission(CONNECTION)
        .unwrap();
    assert_eq!(
        probe(&journal, "search", json!({"query": "indexed"})),
        Err(McpProbeError::PermissionDenied)
    );

    let page = crate::activity::read_activity(
        journal.path(),
        &crate::activity::ActivityQuery {
            limit: 10,
            ..crate::activity::ActivityQuery::default()
        },
    )
    .unwrap();
    assert_eq!(page.entries.len(), 2);
    let outcomes = page
        .entries
        .iter()
        .map(|entry| entry.outcome)
        .collect::<Vec<_>>();
    assert!(outcomes.contains(&crate::activity::RecordedOutcome::Refused));
    assert!(outcomes.contains(&crate::activity::RecordedOutcome::Served));
    let refused = page
        .entries
        .iter()
        .find(|entry| entry.outcome == crate::activity::RecordedOutcome::Refused)
        .unwrap();
    assert_eq!(refused.tool_name, solstone_core_mcp_audit::ToolName::Search);
    assert_eq!(refused.connection.as_deref(), Some(CONNECTION));
    assert_eq!(
        refused.reason.as_deref(),
        Some("this connection has no read permission")
    );
    // 🔑 And the wire keeps the other half of the asymmetry: both refusals
    // — no permission at all, and a permission that lacks the category —
    // render identically to the agent.
    PermissionStore::open(journal.path())
        .set_permission(
            CONNECTION,
            ReadPermission {
                categories: vec!["entities".to_owned()],
                scope: ReadScope::Facets {
                    ids: vec![FACET_A.to_owned()],
                },
            },
        )
        .unwrap();
    assert_eq!(
        probe(&journal, "search", json!({"query": "indexed"})),
        Err(McpProbeError::PermissionDenied)
    );
    let page = crate::activity::read_activity(
        journal.path(),
        &crate::activity::ActivityQuery {
            limit: 10,
            outcome: Some(crate::activity::RecordedOutcome::Refused),
            ..crate::activity::ActivityQuery::default()
        },
    )
    .unwrap();
    // ⚠ Two refusals, indistinguishable on the wire, distinguishable here.
    assert_eq!(page.entries.len(), 2);
    let mut reasons = page
        .entries
        .iter()
        .filter_map(|entry| entry.reason.clone())
        .collect::<Vec<_>>();
    reasons.sort();
    assert_eq!(
        reasons,
        [
            "this connection does not have the transcripts category",
            "this connection has no read permission",
        ]
    );
}

#[test]
fn ac25b_a_search_that_matches_nothing_is_recorded_empty_rather_than_served() {
    let journal = fixture();
    assert!(
        probe(&journal, "search", json!({"query": "zzzznotpresent"})).unwrap()["results"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let page = crate::activity::read_activity(
        journal.path(),
        &crate::activity::ActivityQuery {
            limit: 10,
            ..crate::activity::ActivityQuery::default()
        },
    )
    .unwrap();
    assert_eq!(page.entries.len(), 1);
    assert_eq!(
        page.entries[0].outcome,
        crate::activity::RecordedOutcome::Empty
    );
    assert_eq!(
        page.entries[0].request.as_ref().unwrap().arguments["query"],
        "zzzznotpresent"
    );
}

fn dispatch_output(
    journal: &tempfile::TempDir,
    tool: &str,
    arguments: Value,
) -> crate::dispatch::ToolOutput {
    let entry = crate::registry::tool_by_wire_name(tool).unwrap();
    crate::dispatch::dispatch_authenticated_tool_call(
        journal.path(),
        crate::dispatch::DispatchPrincipal {
            connection: CONNECTION,
            agent_identity: CONNECTION,
        },
        entry.tool_name,
        Some(&arguments),
        chrono::Utc::now(),
    )
    .unwrap()
}

#[test]
fn an_empty_result_says_so_in_words_and_a_served_one_does_not() {
    let journal = fixture();
    let empty = dispatch_output(&journal, "search", json!({"query": "zzzznotpresent"}));
    assert!(empty.value["results"].as_array().unwrap().is_empty());
    let note = empty
        .empty_note
        .clone()
        .expect("an empty search carries a plain note");
    assert!(note.starts_with("No results"), "{note}");

    let rendered = crate::jsonrpc::tool_result(empty.value, empty.empty_note.as_deref());
    assert_eq!(rendered["content"][1]["text"], note);
    assert!(rendered["structuredContent"].get("note").is_none());

    let served = dispatch_output(&journal, "list_facets", json!({}));
    assert!(!served.value["facets"].as_array().unwrap().is_empty());
    assert!(served.empty_note.is_none());
}

#[test]
fn a_journal_that_has_recorded_nothing_lists_no_transcripts_rather_than_failing() {
    let journal = fixture();
    fs::remove_dir_all(journal.path().join("chronicle")).unwrap();
    let output = dispatch_output(&journal, "list_transcripts", json!({}));
    assert!(output.value["transcripts"].as_array().unwrap().is_empty());
    assert!(output.empty_note.is_some());
}

#[test]
fn ac26_the_owners_activity_log_is_unreachable_through_the_boundary_it_audits() {
    // 🔴 The recursion, measured rather than argued. A connection with the
    // widest grant this build can express searches for a term that exists only
    // inside its own interaction records.
    let journal = fixture();
    probe(
        &journal,
        "search",
        json!({"query": "supercalifragilisticexpialidocious"}),
    )
    .unwrap();
    PermissionStore::open(journal.path())
        .set_permission(
            CONNECTION,
            ReadPermission {
                categories: vec![
                    "transcripts".to_owned(),
                    "entities".to_owned(),
                    "facets".to_owned(),
                ],
                scope: ReadScope::WholeJournal,
            },
        )
        .unwrap();

    // Control: the term really is on disk, in this journal, right now.
    let recorded = crate::activity::read_activity(
        journal.path(),
        &crate::activity::ActivityQuery {
            limit: 10,
            ..crate::activity::ActivityQuery::default()
        },
    )
    .unwrap();
    assert!(
        recorded.entries.iter().any(|entry| {
            entry.request.as_ref().is_some_and(|request| {
                request.arguments["query"] == "supercalifragilisticexpialidocious"
            })
        }),
        "control failed: the term was never recorded, so the negative below proves nothing"
    );
    // Second control, on the coordinate that could be wrong: the same query
    // shape does find the fixture's indexed material.
    assert_eq!(
        probe(&journal, "search", json!({"query": "indexed"})).unwrap()["results"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    assert!(
        probe(
            &journal,
            "search",
            json!({"query": "supercalifragilisticexpialidocious"})
        )
        .unwrap()["results"]
            .as_array()
            .unwrap()
            .is_empty(),
        "an agent reached the owner's log of its own queries"
    );
    assert!(
        probe(&journal, "list_transcripts", json!({})).unwrap()["transcripts"]
            .as_array()
            .unwrap()
            .iter()
            .all(|segment| !segment.to_string().contains("mcp.agent"))
    );
}

#[test]
fn ac4_entities_without_facets_can_list_only_facet_id_and_name() {
    let journal = fixture();
    let result = probe(&journal, "list_facets", json!({})).unwrap();
    let facet = &result["facets"][0];
    assert_eq!(facet["id"], FACET_A);
    assert_eq!(facet["name"], "Alpha");
    assert!(facet.get("description").is_none());
    assert!(facet.get("summary").is_none());
}

#[test]
fn a_malformed_or_undeclared_facet_directory_is_skipped_not_fatal() {
    let journal = fixture();
    let malformed = journal.path().join("facets").join("gamma");
    fs::create_dir_all(&malformed).unwrap();
    fs::write(malformed.join("facet.json"), "not json").unwrap();
    let undeclared = journal.path().join("facets").join("delta");
    fs::create_dir_all(&undeclared).unwrap();
    PermissionStore::open(journal.path())
        .set_permission(
            CONNECTION,
            ReadPermission {
                categories: vec!["transcripts".to_owned()],
                scope: ReadScope::WholeJournal,
            },
        )
        .unwrap();
    let result = probe(&journal, "list_facets", json!({})).unwrap();
    let ids = result["facets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|facet| facet["id"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(ids, vec![FACET_A.to_owned(), FACET_B.to_owned()]);
}

#[test]
fn ac3_search_and_fetch_require_the_transcripts_category() {
    let journal = fixture();
    PermissionStore::open(journal.path())
        .set_permission(
            CONNECTION,
            ReadPermission {
                categories: vec!["entities".to_owned()],
                scope: ReadScope::Facets {
                    ids: vec![FACET_A.to_owned()],
                },
            },
        )
        .unwrap();
    assert_eq!(
        probe(&journal, "search", json!({"query": "indexed"})),
        Err(McpProbeError::PermissionDenied)
    );
    assert_eq!(
        probe(&journal, "fetch", json!({"reference": "not-a-reference"})),
        Err(McpProbeError::PermissionDenied)
    );
}

#[test]
fn ac15_ac16_exam_bound_is_unquantified_when_live_drops_cannot_fill_the_page() {
    let journal = fixture();
    let index = solstone_core_indexer_store::db::open_index(journal.path()).unwrap();
    for idx in 1..=crate::dispatch::MAX_SEARCH_EXAMINED_ROWS as i64 {
        index
            .execute(
                "INSERT INTO chunks(content, path, day, facet, agent, stream, idx, time_bucket) \
                 VALUES ('indexed', ?1, ?2, '', 'fixture-stream', ?3, ?4, '')",
                params![PATH, DAY, STREAM, idx],
            )
            .unwrap();
    }
    drop(index);
    let assignments = journal
        .path()
        .join("chronicle")
        .join(DAY)
        .join(STREAM)
        .join(SEGMENT)
        .join("talents/facets.json");
    fs::write(assignments, "[{\"facet\":\"beta\"}]").unwrap();

    let result = probe(&journal, "search", json!({"query": "indexed", "limit": 1})).unwrap();
    assert_eq!(result["coverage"]["live_examination_complete"], false);
    let serialized = result.to_string();
    for forbidden in ["count", "percent", "range", "remaining", "dropped"] {
        assert!(
            !serialized.contains(forbidden),
            "coverage leaked {forbidden}"
        );
    }
}

#[test]
fn ac14_unauthorized_chunks_do_not_change_visible_search_page_or_coverage() {
    let journal = fixture();
    let before = probe(&journal, "search", json!({"query": "indexed"})).unwrap();
    let before = visible_search_page(&before);

    let unauthorized_segment = journal
        .path()
        .join("chronicle")
        .join(DAY)
        .join(STREAM)
        .join("100000_300");
    fs::create_dir_all(unauthorized_segment.join("talents")).unwrap();
    fs::write(
        unauthorized_segment.join("talents/facets.json"),
        "[{\"facet\":\"beta\"}]",
    )
    .unwrap();
    let unauthorized_path = "20260914/default/100000_300/talents/brief.md";
    let index = solstone_core_indexer_store::db::open_index(journal.path()).unwrap();
    index
        .execute(
            "INSERT INTO chunks(content, path, day, facet, agent, stream, idx, time_bucket) \
             VALUES ('indexed unauthorized', ?1, ?2, '', 'fixture-stream', ?3, 0, '')",
            params![unauthorized_path, DAY, STREAM],
        )
        .unwrap();
    index
        .execute(
            "INSERT INTO chunk_classification(path, category, basis, eligible, unclassified) \
             VALUES (?1, 'transcripts', 'segment_assigned', 1, 0)",
            params![unauthorized_path],
        )
        .unwrap();
    index
        .execute(
            "INSERT INTO chunk_classification_facets(path, facet_id) VALUES (?1, ?2)",
            params![unauthorized_path, FACET_B],
        )
        .unwrap();
    drop(index);

    let after = probe(&journal, "search", json!({"query": "indexed"})).unwrap();
    assert_eq!(visible_search_page(&after), before);
}

#[test]
fn ac13_cursor_rejects_a_replaced_anchor_and_valid_cursor_continues_without_repeats() {
    let journal = fixture();
    let index = solstone_core_indexer_store::db::open_index(journal.path()).unwrap();
    index
        .execute(
            "INSERT INTO chunks(content, path, day, facet, agent, stream, idx, time_bucket) \
             VALUES ('indexed next', ?1, ?2, '', 'fixture-stream', ?3, 1, '')",
            params![PATH, DAY, STREAM],
        )
        .unwrap();
    drop(index);

    let first = probe(&journal, "search", json!({"query": "indexed", "limit": 1})).unwrap();
    let cursor = first["next_cursor"].as_str().unwrap().to_owned();
    let second = probe(
        &journal,
        "search",
        json!({"query": "indexed", "limit": 1, "cursor": cursor}),
    )
    .unwrap();
    assert_ne!(
        first["results"][0]["snippet"],
        second["results"][0]["snippet"]
    );

    let index = solstone_core_indexer_store::db::open_index(journal.path()).unwrap();
    index.execute("DELETE FROM chunks", []).unwrap();
    index
        .execute(
            "INSERT INTO chunks(rowid, content, path, day, facet, agent, stream, idx, time_bucket) \
             VALUES (2, 'replacement indexed bytes', ?1, ?2, '', 'fixture-stream', ?3, 1, '')",
            params![PATH, DAY, STREAM],
        )
        .unwrap();
    drop(index);
    assert_eq!(
        probe(
            &journal,
            "search",
            json!({"query": "indexed", "limit": 1, "cursor": cursor}),
        ),
        Err(McpProbeError::Unavailable)
    );
}

#[test]
fn ac13_cursor_rejects_a_reset_rebuild_with_rowids_restarted_at_one() {
    let journal = fixture();
    let index = solstone_core_indexer_store::db::open_index(journal.path()).unwrap();
    index
        .execute(
            "INSERT INTO chunks(content, path, day, facet, agent, stream, idx, time_bucket) \
             VALUES ('indexed next', ?1, ?2, '', 'fixture-stream', ?3, 1, '')",
            params![PATH, DAY, STREAM],
        )
        .unwrap();
    drop(index);
    let first = probe(&journal, "search", json!({"query": "indexed", "limit": 1})).unwrap();
    let cursor = first["next_cursor"].as_str().unwrap().to_owned();

    let index = solstone_core_indexer_store::db::open_index(journal.path()).unwrap();
    index.execute("DELETE FROM chunks", []).unwrap();
    index
        .execute(
            "INSERT INTO chunks(content, path, day, facet, agent, stream, idx, time_bucket) \
             VALUES ('indexed next', ?1, ?2, '', 'fixture-stream', ?3, 1, '')",
            params![PATH, DAY, STREAM],
        )
        .unwrap();
    index
        .execute(
            "INSERT INTO chunks(content, path, day, facet, agent, stream, idx, time_bucket) \
             VALUES ('indexed bytes', ?1, ?2, '', 'fixture-stream', ?3, 0, '')",
            params![PATH, DAY, STREAM],
        )
        .unwrap();
    let row_ids = index
        .prepare("SELECT rowid FROM chunks ORDER BY rowid")
        .unwrap()
        .query_map([], |row| row.get::<_, i64>(0))
        .unwrap()
        .map(Result::unwrap)
        .collect::<Vec<_>>();
    assert_eq!(row_ids, [1, 2]);
    drop(index);

    assert_eq!(
        probe(
            &journal,
            "search",
            json!({"query": "indexed", "limit": 1, "cursor": cursor}),
        ),
        Err(McpProbeError::Unavailable)
    );
}

#[test]
fn ac28_generation_recheck_refuses_a_narrowed_permission_before_release() {
    let journal = fixture();
    let PermissionDecision::Snapshot(snapshot) =
        evaluate_connection_read(journal.path(), CONNECTION)
    else {
        panic!("fixture permission is enforceable");
    };
    PermissionStore::open(journal.path())
        .set_permission(
            CONNECTION,
            ReadPermission {
                categories: vec!["entities".to_owned()],
                scope: ReadScope::Facets {
                    ids: vec![FACET_A.to_owned()],
                },
            },
        )
        .unwrap();
    assert!(!crate::dispatch::permission_generation_is_current(
        journal.path(),
        CONNECTION,
        &snapshot,
    ));
}

#[test]
fn ac24_get_transcript_pages_a_huge_segment_without_returning_the_whole_file() {
    let journal = fixture();
    let transcript = journal
        .path()
        .join("chronicle")
        .join(DAY)
        .join(STREAM)
        .join(SEGMENT)
        .join("meeting_transcript.md");
    fs::write(&transcript, "pageable transcript\n".repeat(10_000)).unwrap();
    let listed = probe(&journal, "list_transcripts", json!({})).unwrap();
    let reference = listed["transcripts"][0]["reference"].as_str().unwrap();
    let page = probe(&journal, "get_transcript", json!({"reference": reference})).unwrap();
    assert!(page["entries"].as_array().unwrap().len() <= 100);
    assert!(page["next_cursor"].is_string());
}

#[test]
fn get_transcript_reads_recorded_speech_from_audio_jsonl() {
    let journal = fixture();
    let segment = journal
        .path()
        .join("chronicle")
        .join(DAY)
        .join(STREAM)
        .join(SEGMENT);
    fs::remove_file(segment.join("meeting_transcript.md")).unwrap();
    fs::write(
        segment.join("audio.jsonl"),
        "{\"source\":\"recording\"}\n{\"start\":\"00:00:02\",\"text\":\"spoken words\"}\n",
    )
    .unwrap();

    let listed = probe(&journal, "list_transcripts", json!({})).unwrap();
    let reference = listed["transcripts"][0]["reference"].as_str().unwrap();
    let page = probe(&journal, "get_transcript", json!({"reference": reference})).unwrap();
    assert_eq!(page["entries"], json!(["[00:00:02] spoken words"]));
    assert!(page["next_cursor"].is_null());
}

#[test]
fn ac7_ac21_ac22_entities_are_scoped_stable_and_omit_forbidden_fields() {
    let journal = fixture();
    PermissionStore::open(journal.path())
        .set_permission(
            CONNECTION,
            ReadPermission {
                categories: vec!["entities".to_owned()],
                scope: ReadScope::Facets {
                    ids: vec![FACET_A.to_owned()],
                },
            },
        )
        .unwrap();
    let listed = probe(&journal, "list_entities", json!({})).unwrap();
    assert_eq!(listed["entities"].as_array().unwrap().len(), 2);
    let reference = listed["entities"][0]["reference"].as_str().unwrap();
    let entity = probe(&journal, "get_entity", json!({"reference": reference})).unwrap();
    assert_eq!(entity, json!({"name": "Same Name", "notes": []}));
    let serialized = entity.to_string();
    for forbidden in ["aka", "out-of-scope", "detached", "aggregate", "decision"] {
        assert!(!serialized.contains(forbidden));
    }
}

#[test]
fn ac19_ac20_all_agent_results_omit_coordinates_and_keep_index_coverage_statement() {
    let journal = fixture();
    PermissionStore::open(journal.path())
        .set_permission(CONNECTION, ReadPermission::default_whole_journal())
        .unwrap();

    let facets = probe(&journal, "list_facets", json!({})).unwrap();
    let search = probe(&journal, "search", json!({"query": "indexed"})).unwrap();
    let fetch = probe(
        &journal,
        "fetch",
        json!({"reference": search["results"][0]["reference"]}),
    )
    .unwrap();
    let transcripts = probe(&journal, "list_transcripts", json!({})).unwrap();
    let transcript = probe(
        &journal,
        "get_transcript",
        json!({"reference": transcripts["transcripts"][0]["reference"]}),
    )
    .unwrap();
    let entities = probe(&journal, "list_entities", json!({})).unwrap();
    let entity = probe(
        &journal,
        "get_entity",
        json!({"reference": entities["entities"][0]["reference"]}),
    )
    .unwrap();

    for result in [
        &facets,
        &search,
        &fetch,
        &transcripts,
        &transcript,
        &entities,
        &entity,
    ] {
        assert!(!result.to_string().contains("20260914/default/090000_300"));
        assert_no_coordinate_keys(result);
    }
    assert!(search["coverage"]["transcript_index"].is_string());
}

#[test]
fn ac6_delete_recreate_and_ac9_rename_keep_stable_facet_ids_real() {
    let journal = fixture();
    let alpha = journal.path().join("facets/alpha");
    fs::rename(&alpha, journal.path().join("facets/alpha-renamed")).unwrap();
    assert_eq!(
        probe(&journal, "list_entities", json!({})),
        Err(McpProbeError::PermissionDenied)
    );
    PermissionStore::open(journal.path())
        .set_permission(
            CONNECTION,
            ReadPermission {
                categories: vec!["entities".to_owned(), "transcripts".to_owned()],
                scope: ReadScope::Facets {
                    ids: vec![FACET_A.to_owned()],
                },
            },
        )
        .unwrap();
    assert_eq!(
        probe(&journal, "list_entities", json!({})).unwrap()["entities"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert!(
        probe(&journal, "search", json!({"query":"indexed"})).unwrap()["results"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        probe(&journal, "list_transcripts", json!({})).unwrap()["transcripts"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    fs::remove_dir_all(journal.path().join("facets/alpha-renamed")).unwrap();
    fs::create_dir_all(&alpha).unwrap();
    fs::write(
        alpha.join("facet.json"),
        json!({"id": FACET_B, "title":"Alpha"}).to_string(),
    )
    .unwrap();
    assert!(
        probe(&journal, "list_facets", json!({})).unwrap()["facets"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

fn grant(journal: &tempfile::TempDir, scope: ReadScope) {
    PermissionStore::open(journal.path())
        .set_permission(
            CONNECTION,
            ReadPermission {
                categories: vec!["entities".to_owned(), "transcripts".to_owned()],
                scope,
            },
        )
        .unwrap();
}

fn reachable(journal: &tempfile::TempDir) -> (usize, usize) {
    let search = probe(journal, "search", json!({"query":"indexed"}))
        .map(|value| value["results"].as_array().map_or(0, Vec::len))
        .unwrap_or(0);
    let transcripts = probe(journal, "list_transcripts", json!({}))
        .map(|value| value["transcripts"].as_array().map_or(0, Vec::len))
        .unwrap_or(0);
    (search, transcripts)
}

fn reconcile(journal: &tempfile::TempDir) {
    let mut snapshot = || {
        solstone_core_indexer_store::classification::FacetDeclarationSet::from_journal(
            journal.path(),
        )
    };
    solstone_core_indexer_store::reconcile::reconcile_stale_classifications(
        journal.path(),
        &mut snapshot,
    )
    .unwrap();
}

#[test]
fn a_merged_facet_name_reaches_the_survivor_in_search_and_transcripts_and_never_its_old_grant() {
    let journal = fixture();
    // What `journal facet merge alpha --into beta` leaves: alpha's folder is
    // gone and its name resolves to beta. The index was built before it.
    fs::remove_dir_all(journal.path().join("facets/alpha")).unwrap();
    fs::write(
        journal.path().join("facets/retired.json"),
        json!({"names": {"alpha": {"state": "merged", "id": FACET_A, "successor": FACET_B}}})
            .to_string(),
    )
    .unwrap();
    grant(
        &journal,
        ReadScope::Facets {
            ids: vec![FACET_B.to_owned()],
        },
    );
    // Transcripts re-check live, so they follow at once; search needs the
    // stored classification brought up to date.
    assert_eq!(reachable(&journal), (0, 1));
    reconcile(&journal);
    assert_eq!(reachable(&journal), (1, 1));
    grant(
        &journal,
        ReadScope::Facets {
            ids: vec![FACET_A.to_owned()],
        },
    );
    assert_eq!(reachable(&journal), (0, 0));
    grant(
        &journal,
        ReadScope::Facets {
            ids: vec!["123e4567-e89b-42d3-a456-426614174009".to_owned()],
        },
    );
    assert_eq!(reachable(&journal), (0, 0));
}

#[test]
fn a_deleted_facet_name_is_never_given_to_a_new_facet_and_a_forced_reuse_stays_closed() {
    let journal = fixture();
    assert!(solstone_core_facets::delete_facet(journal.path(), "alpha").unwrap());
    assert!(matches!(
        solstone_core_facets::create_facet(journal.path(), "alpha", "Alpha", "", "", "", None),
        Err(solstone_core_facets::FacetWriteError::NameRetired { .. })
    ));
    // A hand edit reuses the name anyway, with a new id.
    let alpha = journal.path().join("facets/alpha");
    fs::create_dir_all(&alpha).unwrap();
    const FACET_C: &str = "123e4567-e89b-42d3-a456-426614174002";
    fs::write(
        alpha.join("facet.json"),
        json!({"id": FACET_C, "title": "Alpha"}).to_string(),
    )
    .unwrap();
    reconcile(&journal);
    grant(
        &journal,
        ReadScope::Facets {
            ids: vec![FACET_C.to_owned()],
        },
    );
    assert_eq!(reachable(&journal), (0, 0));
    grant(&journal, ReadScope::WholeJournal);
    assert_eq!(reachable(&journal), (1, 1));
}

#[test]
fn ac8_ac11_ac12_ac30_bad_live_scope_and_references_fail_closed() {
    let journal = fixture();
    let search = probe(&journal, "search", json!({"query":"indexed"})).unwrap();
    let reference = search["results"][0]["reference"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        probe(&journal, "fetch", json!({"reference":"notes/a:b.txt:42"})),
        Err(McpProbeError::Unavailable)
    );
    let tampered = probe(&journal, "fetch", json!({"reference":"tampered"}));
    assert_eq!(tampered, Err(McpProbeError::Unavailable));
    PermissionStore::open(journal.path())
        .set_permission("bearer:other", ReadPermission::default_whole_journal())
        .unwrap();
    assert_eq!(
        run_mcp_probe(
            journal.path(),
            "bearer:other",
            "fetch",
            &json!({"reference":reference})
        ),
        Err(McpProbeError::Unavailable)
    );
    PermissionStore::open(journal.path())
        .set_permission(
            CONNECTION,
            ReadPermission {
                categories: vec!["transcripts".to_owned()],
                scope: ReadScope::Facets {
                    ids: vec![FACET_A.to_owned()],
                },
            },
        )
        .unwrap();
    assert_eq!(
        probe(&journal, "fetch", json!({"reference": reference})),
        tampered
    );
    let facets = journal
        .path()
        .join("chronicle")
        .join(DAY)
        .join(STREAM)
        .join(SEGMENT)
        .join("talents/facets.json");
    fs::write(facets, "not json").unwrap();
    assert!(
        probe(&journal, "list_transcripts", json!({})).unwrap()["transcripts"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        probe(&journal, "search", json!({"query":"indexed"})).unwrap()["results"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    PermissionStore::open(journal.path())
        .clear_permission(CONNECTION)
        .unwrap();
    assert_eq!(
        probe(&journal, "search", json!({"query":"indexed"})),
        Err(McpProbeError::PermissionDenied)
    );
}

fn result_count(result: &Value, key: &str) -> usize {
    result[key].as_array().unwrap().len()
}

#[test]
fn an_iso_day_finds_what_the_journal_day_key_finds() {
    // Hosted agents send ISO dates. Before this, `2026-09-14` reached the index
    // as a string comparison and every entry fell outside it: a full journal
    // read back as "nothing matched".
    let journal = fixture();
    for (day_key, iso) in [
        ("day", "day"),
        ("day_from", "day_from"),
        ("day_to", "day_to"),
    ] {
        let keyed = probe(
            &journal,
            "search",
            json!({"query": "indexed", day_key: DAY}),
        )
        .unwrap();
        let dashed = probe(
            &journal,
            "search",
            json!({"query": "indexed", iso: "2026-09-14"}),
        )
        .unwrap();
        assert_eq!(result_count(&keyed, "results"), 1, "{day_key}");
        assert_eq!(
            visible_search_page(&keyed),
            visible_search_page(&dashed),
            "{iso}"
        );
    }
    assert_eq!(
        result_count(
            &probe(&journal, "list_transcripts", json!({"day": "2026-09-14"})).unwrap(),
            "transcripts"
        ),
        1
    );
}

#[test]
fn a_day_that_is_not_a_calendar_day_is_refused_rather_than_matching_nothing() {
    let journal = fixture();
    for day in [
        "Sept 14",
        "2026-9-14",
        "20261340",
        "2026-02-30",
        "14/09/2026",
    ] {
        assert_eq!(
            probe(&journal, "search", json!({"query": "indexed", "day": day})),
            Err(McpProbeError::InvalidInput),
            "{day}"
        );
        assert_eq!(
            probe(&journal, "list_transcripts", json!({"day": day})),
            Err(McpProbeError::InvalidInput),
            "{day}"
        );
    }
}

#[test]
fn a_sentence_with_no_whole_match_falls_back_to_its_words_and_says_so() {
    let journal = fixture();
    let strict = probe(&journal, "search", json!({"query": "indexed"})).unwrap();
    assert_eq!(strict["relaxed"], false);
    let sentence = probe(
        &journal,
        "search",
        json!({"query": "what indexed notes did I write about the launch"}),
    )
    .unwrap();
    assert_eq!(result_count(&sentence, "results"), 1);
    assert_eq!(sentence["relaxed"], true);
    let absent = probe(
        &journal,
        "search",
        json!({"query": "zzzznotpresent qqqqmissing"}),
    )
    .unwrap();
    assert_eq!(result_count(&absent, "results"), 0);
}

#[test]
fn get_entity_says_who_someone_is_from_the_granted_facet_only() {
    // An agent asked "who is X" needs the facet's description and notes, not
    // just a name. ⛔ The same entity's notes in another facet, and its
    // journal-level description and aliases, stay out.
    let journal = fixture();
    let root = journal.path();
    for (facet, description, note) in [
        ("alpha", "Advisor in alpha", "alpha note about coffee"),
        ("beta", "Something only beta knows", "beta-only secret note"),
    ] {
        let link = root.join("facets").join(facet).join("entities/primary");
        fs::create_dir_all(&link).unwrap();
        fs::write(
            link.join("entity.json"),
            json!({"entity_id": "entity-primary", "description": description, "last_seen": "20260913"})
                .to_string(),
        )
        .unwrap();
        fs::write(
            link.join("observations.jsonl"),
            format!("{{\"content\":\"{note}\"}}\n"),
        )
        .unwrap();
    }
    fs::write(
        root.join("entities/primary/entity.json"),
        json!({"id": "entity-primary", "name": "Same Name", "type": "Person",
               "aka": ["forbidden aka"], "description": "out-of-scope description"})
        .to_string(),
    )
    .unwrap();
    solstone_core_indexer_store::scan::scan_journal(root, true).unwrap();
    PermissionStore::open(root)
        .set_permission(
            CONNECTION,
            ReadPermission {
                categories: vec!["entities".to_owned()],
                scope: ReadScope::Facets {
                    ids: vec![FACET_A.to_owned()],
                },
            },
        )
        .unwrap();
    let listed = probe(&journal, "list_entities", json!({})).unwrap();
    let primary = listed["entities"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entity| {
            probe(
                &journal,
                "get_entity",
                json!({"reference": entity["reference"]}),
            )
            .unwrap()
        })
        .find(|entity| entity["description"] == "Advisor in alpha")
        .expect("the alpha relationship is served");
    assert_eq!(primary["type"], "Person");
    assert_eq!(primary["last_seen"], "20260913");
    assert_eq!(primary["notes"], json!(["alpha note about coffee"]));
    let serialized = primary.to_string();
    for forbidden in ["beta", "secret", "aka", "out-of-scope"] {
        assert!(
            !serialized.contains(forbidden),
            "{forbidden} leaked: {serialized}"
        );
    }
}

fn setup_facets(journal: &Path) {
    for (name, id, title) in [("alpha", FACET_A, "Alpha"), ("beta", FACET_B, "Beta")] {
        let directory = journal.join("facets").join(name);
        fs::create_dir_all(&directory).unwrap();
        fs::write(
            directory.join("facet.json"),
            json!({"id": id, "title": title, "description": format!("{title} details")})
                .to_string(),
        )
        .unwrap();
    }
}

fn plant_chunk(
    journal: &Path,
    path: &str,
    day: &str,
    stream: &str,
    idx: i64,
    content: &str,
    category: &str,
    basis: &str,
    facet_ids: &[&str],
) {
    let index = solstone_core_indexer_store::db::open_index(journal).unwrap();
    index
        .execute(
            "INSERT INTO chunks(content, path, day, facet, agent, stream, idx, time_bucket) \
             VALUES (?1, ?2, ?3, '', 'fixture-stream', ?4, ?5, '')",
            params![content, path, day, stream, idx],
        )
        .unwrap();
    index
        .execute(
            "INSERT OR REPLACE INTO chunk_classification(path, category, basis, eligible, unclassified) \
             VALUES (?1, ?2, ?3, 1, 0)",
            params![path, category, basis],
        )
        .unwrap();
    for facet_id in facet_ids {
        index
            .execute(
                "INSERT OR REPLACE INTO chunk_classification_facets(path, facet_id) VALUES (?1, ?2)",
                params![path, facet_id],
            )
            .unwrap();
    }
    index
        .execute(
            "REPLACE INTO chunk_classification_backfill(id, cursor, completed, stalled, stalled_path, resume_count) \
             VALUES (1, '', 1, 0, NULL, 0)",
            [],
        )
        .unwrap();
    index
        .execute(
            "REPLACE INTO index_build_state(id, schema_version, state, files_count, chunks_count) \
             VALUES (1, 1, 'complete', 1, 1)",
            [],
        )
        .unwrap();
}

fn setup_shared_link_journal() -> tempfile::TempDir {
    let journal = tempfile::Builder::new()
        .prefix("link-test-")
        .tempdir_in(crate::test_scratch())
        .unwrap();
    setup_facets(journal.path());
    let chronicle = journal.path().join("chronicle").join("20261010");

    let alpha_seg = chronicle.join("alpha").join("090000_300");
    fs::create_dir_all(alpha_seg.join("talents")).unwrap();
    fs::write(
        alpha_seg.join("talents/facets.json"),
        "[{\"facet\":\"alpha\"}]",
    )
    .unwrap();
    fs::write(
        alpha_seg.join("meeting_transcript.md"),
        "alpha_transcript_token\n",
    )
    .unwrap();
    fs::write(alpha_seg.join("talents/brief.md"), "brief bytes\n").unwrap();
    fs::write(
        alpha_seg.join("stream.json"),
        json!({"stream": "not-alpha"}).to_string(),
    )
    .unwrap();

    let beta_seg = chronicle.join("beta").join("090000_300");
    fs::create_dir_all(beta_seg.join("talents")).unwrap();
    fs::write(
        beta_seg.join("talents/facets.json"),
        "[{\"facet\":\"alpha\"}]",
    )
    .unwrap();
    fs::write(
        beta_seg.join("meeting_transcript.md"),
        "beta_transcript_token\n",
    )
    .unwrap();
    fs::write(beta_seg.join("talents/brief.md"), "brief bytes\n").unwrap();

    let flat_dup = chronicle.join("090000_300");
    fs::create_dir_all(flat_dup.join("talents")).unwrap();
    fs::write(
        flat_dup.join("talents/facets.json"),
        "[{\"facet\":\"alpha\"}]",
    )
    .unwrap();
    fs::write(
        flat_dup.join("meeting_transcript.md"),
        "flat_dup_transcript_token\n",
    )
    .unwrap();

    let flat_only = chronicle.join("100000_300");
    fs::create_dir_all(flat_only.join("talents")).unwrap();
    fs::write(
        flat_only.join("talents/facets.json"),
        "[{\"facet\":\"alpha\"}]",
    )
    .unwrap();
    fs::write(
        flat_only.join("meeting_transcript.md"),
        "flat_only_transcript_token\n",
    )
    .unwrap();

    let x_seg = chronicle.join("alpha").join("x-120000_300");
    fs::create_dir_all(x_seg.join("talents")).unwrap();
    fs::write(x_seg.join("talents/facets.json"), "[{\"facet\":\"alpha\"}]").unwrap();
    fs::write(x_seg.join("meeting_transcript.md"), "x_transcript_token\n").unwrap();

    journal
}

fn find_audit_files(journal: &Path, filename: &str) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let chronicle = journal.join("chronicle");
    if let Ok(days) = fs::read_dir(chronicle) {
        for day in days.filter_map(Result::ok) {
            let stream_dir = day.path().join("mcp.agent");
            if let Ok(segs) = fs::read_dir(stream_dir) {
                for seg in segs.filter_map(Result::ok) {
                    let file_path = seg.path().join(filename);
                    if file_path.is_file() {
                        files.push(file_path);
                    }
                }
            }
        }
    }
    files.sort();
    files
}

struct CountingChecker {
    count: usize,
}

impl TranscriptPageCheck for CountingChecker {
    fn check_first_page(
        &mut self,
        _segment: &solstone_core_journal_io::paths::Segment,
    ) -> Result<Option<u64>, ()> {
        self.count += 1;
        Ok(Some(12345))
    }
}

#[test]
fn test_1_search_hit_on_beta_links_only_to_beta() {
    let journal = setup_shared_link_journal();
    let path = "20261010/beta/090000_300/talents/brief.md";
    plant_chunk(
        journal.path(),
        path,
        "20261010",
        "",
        0,
        "sharedneedle1 test 1 beta hit",
        "transcripts",
        "segment_assigned",
        &[FACET_A],
    );
    PermissionStore::open(journal.path())
        .set_permission(CONNECTION, ReadPermission::default_whole_journal())
        .unwrap();

    let search = probe(
        &journal,
        "search",
        json!({"query": "sharedneedle1", "limit": 10}),
    )
    .unwrap();
    let results = search["results"].as_array().unwrap();
    assert_eq!(results.len(), 1);
    let hit = &results[0];
    let segments = hit["segments"].as_array().expect("hit has segments");
    assert_eq!(segments.len(), 1);
    let link = &segments[0];
    assert_eq!(link["time"], "09:00-09:05");
    assert!(link["reference"].is_string());
    assert_eq!(link.as_object().unwrap().len(), 2);

    let transcript = probe(
        &journal,
        "get_transcript",
        json!({"reference": link["reference"]}),
    )
    .unwrap();
    let entries = transcript["entries"].as_array().unwrap();
    let text: String = entries
        .iter()
        .map(|e| e.as_str().unwrap())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(text.contains("beta_transcript_token"));
    assert!(!text.contains("alpha_transcript_token"));
}

#[test]
fn test_2_activity_stream_links_and_three_candidate_limit() {
    let journal = setup_shared_link_journal();
    let chronicle = journal.path().join("chronicle/20261010");
    for (stream, name, token) in [
        ("alpha", "110000_300", "alpha_11_token"),
        ("beta", "110000_300", "beta_11_token"),
    ] {
        let seg = chronicle.join(stream).join(name);
        fs::create_dir_all(seg.join("talents")).unwrap();
        fs::write(seg.join("talents/facets.json"), "[{\"facet\":\"alpha\"}]").unwrap();
        fs::write(seg.join("meeting_transcript.md"), format!("{token}\n")).unwrap();
    }

    let act_path = "facets/alpha/activities/20261010.jsonl";
    let line0 =
        json!({"id": "act-stream-beta", "stream": "beta", "segments": ["090000_300"]}).to_string();
    let line1 = json!({
        "id": "act-no-stream",
        "segments": ["090000_300", "100000_300", "110000_300", "120000_300"]
    })
    .to_string();
    fs::create_dir_all(journal.path().join("facets/alpha/activities")).unwrap();
    fs::write(journal.path().join(act_path), format!("{line0}\n{line1}\n")).unwrap();

    let produced0 = produce_chunks(Family::Activity, act_path, &line0);
    let produced1 = produce_chunks(Family::Activity, act_path, &line1);
    let content0 = format!("{}\nsharedneedle2", produced0.chunks[0].content);
    let content1 = format!("{}\nsharedneedle2", produced1.chunks[0].content);

    plant_chunk(
        journal.path(),
        act_path,
        "20261010",
        "",
        0,
        &content0,
        "facets",
        "facet_owned",
        &[FACET_A],
    );
    plant_chunk(
        journal.path(),
        act_path,
        "20261010",
        "",
        1,
        &content1,
        "facets",
        "facet_owned",
        &[FACET_A],
    );

    PermissionStore::open(journal.path())
        .set_permission(CONNECTION, ReadPermission::default_whole_journal())
        .unwrap();

    let search = probe(
        &journal,
        "search",
        json!({"query": "sharedneedle2", "limit": 10}),
    )
    .unwrap();
    let results = search["results"].as_array().unwrap();
    assert_eq!(results.len(), 2);

    let hit0 = results
        .iter()
        .find(|h| h["snippet"].as_str().unwrap().contains("Act-stream-beta"))
        .unwrap();
    let segments0 = hit0["segments"].as_array().expect("hit0 has segments");
    assert_eq!(segments0.len(), 1);
    assert!(hit0.get("more_segments").is_none());
    let t0 = probe(
        &journal,
        "get_transcript",
        json!({"reference": segments0[0]["reference"]}),
    )
    .unwrap();
    let entries0: String = t0["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e.as_str().unwrap())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(entries0.contains("beta_transcript_token"));
    assert!(!entries0.contains("alpha_transcript_token"));

    let hit1 = results
        .iter()
        .find(|h| h["snippet"].as_str().unwrap().contains("Act-no-stream"))
        .unwrap();
    let segments1 = hit1["segments"].as_array().expect("hit1 has segments");
    assert_eq!(segments1.len(), 3);
    assert_eq!(hit1["more_segments"], true);

    let mut all_texts = Vec::new();
    for seg in segments1 {
        let t = probe(
            &journal,
            "get_transcript",
            json!({"reference": seg["reference"]}),
        )
        .unwrap();
        all_texts.push(
            t["entries"]
                .as_array()
                .unwrap()
                .iter()
                .map(|e| e.as_str().unwrap())
                .collect::<Vec<_>>()
                .join(" "),
        );
    }
    let combined = all_texts.join(" ");
    assert!(combined.contains("alpha_transcript_token"));
    assert!(combined.contains("beta_transcript_token"));
    assert!(combined.contains("flat_only_transcript_token"));
    assert!(!combined.contains("flat_dup_transcript_token"));
    assert!(!combined.contains("alpha_11_token"));
}

#[test]
fn test_3_activity_identity_time_and_title_case_fold() {
    let journal = setup_shared_link_journal();
    let act_path = "facets/alpha/activities/20261010.jsonl";
    fs::create_dir_all(journal.path().join("facets/alpha/activities")).unwrap();

    let line0 = json!({"id": "launch-sync", "segments": ["090000_300"]}).to_string();
    let line1 =
        json!({"id": "twin-different", "title": "Actual Title", "segments": ["090000_300"]})
            .to_string();
    let line2 =
        json!({"id": "twin-unrewritten", "title": "Twin", "segments": ["090000_300"]}).to_string();

    let file_content = format!("\nnot a json object\n{line0}\n{line1}\n{line2}\n");
    fs::write(journal.path().join(act_path), &file_content).unwrap();

    let p0 = produce_chunks(Family::Activity, act_path, &line0);
    let time_line = p0.chunks[0]
        .content
        .lines()
        .find(|l| l.starts_with("- Time:"))
        .unwrap();
    let content0 =
        format!("### Launch-Sync\n{time_line}\n- Level: high\nSaved 2 items\nsharedneedle3");
    let content1 = format!("### Mismatched Title\n{time_line}\nsharedneedle3");
    let content2 = format!("### Twin\n{time_line}\nsharedneedle3");

    plant_chunk(
        journal.path(),
        act_path,
        "20261010",
        "",
        0,
        &content0,
        "facets",
        "facet_owned",
        &[FACET_A],
    );
    plant_chunk(
        journal.path(),
        act_path,
        "20261010",
        "",
        1,
        &content1,
        "facets",
        "facet_owned",
        &[FACET_A],
    );
    plant_chunk(
        journal.path(),
        act_path,
        "20261010",
        "",
        2,
        &content2,
        "facets",
        "facet_owned",
        &[FACET_A],
    );

    PermissionStore::open(journal.path())
        .set_permission(CONNECTION, ReadPermission::default_whole_journal())
        .unwrap();

    let search = probe(
        &journal,
        "search",
        json!({"query": "sharedneedle3", "limit": 10}),
    )
    .unwrap();
    let results = search["results"].as_array().unwrap();
    let hit_launch = results
        .iter()
        .find(|h| h["snippet"].as_str().unwrap().contains("Launch-Sync"))
        .unwrap();
    let hit_mismatch = results
        .iter()
        .find(|h| h["snippet"].as_str().unwrap().contains("Mismatched Title"))
        .unwrap();
    let hit_twin = results
        .iter()
        .find(|h| h["snippet"].as_str().unwrap().contains("Twin"))
        .unwrap();
    assert!(hit_launch.get("segments").is_some());
    assert!(hit_mismatch.get("segments").is_none());
    assert!(hit_twin.get("segments").is_some());

    let line0_new =
        json!({"id": "completely-different-id", "segments": ["090000_300"]}).to_string();
    let rewritten_content = format!("\nnot a json object\n{line0_new}\n{line1}\n{line2}\n");
    fs::write(journal.path().join(act_path), &rewritten_content).unwrap();

    let search2 = probe(
        &journal,
        "search",
        json!({"query": "sharedneedle3", "limit": 10}),
    )
    .unwrap();
    let results2 = search2["results"].as_array().unwrap();
    assert_eq!(results2.len(), 3);
    let hit_launch2 = results2
        .iter()
        .find(|h| h["snippet"].as_str().unwrap().contains("Launch-Sync"))
        .unwrap();
    let hit_mismatch2 = results2
        .iter()
        .find(|h| h["snippet"].as_str().unwrap().contains("Mismatched Title"))
        .unwrap();
    let hit_twin2 = results2
        .iter()
        .find(|h| h["snippet"].as_str().unwrap().contains("Twin"))
        .unwrap();
    assert!(hit_launch2.get("segments").is_none());
    assert!(hit_mismatch2.get("segments").is_none());
    assert!(hit_twin2.get("segments").is_some());
}

#[test]
fn test_4_whole_journal_search_category_and_facet_keeps_links() {
    let journal = setup_shared_link_journal();
    let act_path = "facets/alpha/activities/20261010.jsonl";
    fs::create_dir_all(journal.path().join("facets/alpha/activities")).unwrap();
    let line0 = json!({"id": "launch-sync", "segments": ["090000_300"]}).to_string();
    fs::write(journal.path().join(act_path), format!("{line0}\n")).unwrap();
    let p0 = produce_chunks(Family::Activity, act_path, &line0);
    let content0 = format!("{}\nsharedneedle4", p0.chunks[0].content);
    plant_chunk(
        journal.path(),
        act_path,
        "20261010",
        "",
        0,
        &content0,
        "facets",
        "facet_owned",
        &[FACET_A],
    );

    PermissionStore::open(journal.path())
        .set_permission(CONNECTION, ReadPermission::default_whole_journal())
        .unwrap();

    let s1 = probe(
        &journal,
        "search",
        json!({"query": "sharedneedle4", "category": "facets", "limit": 10}),
    )
    .unwrap();
    let hit1 = &s1["results"].as_array().unwrap()[0];
    let segs1 = hit1["segments"].as_array().expect("hit1 has segments");
    assert!(!segs1.is_empty());

    let s2 = probe(
        &journal,
        "search",
        json!({"query": "sharedneedle4", "facet": FACET_A, "limit": 10}),
    )
    .unwrap();
    let hit2 = &s2["results"].as_array().unwrap()[0];
    let segs2 = hit2["segments"].as_array().expect("hit2 has segments");
    assert!(!segs2.is_empty());

    let t = probe(
        &journal,
        "get_transcript",
        json!({"reference": segs2[0]["reference"]}),
    )
    .unwrap();
    assert!(!t["entries"].as_array().unwrap().is_empty());
}

#[test]
fn test_5_chosen_facets_excludes_multi_facet_segment() {
    let journal = setup_shared_link_journal();
    fs::write(
        journal
            .path()
            .join("chronicle/20261010/beta/090000_300/talents/facets.json"),
        "[{\"facet\":\"alpha\"},{\"facet\":\"beta\"}]",
    )
    .unwrap();
    fs::write(
        journal
            .path()
            .join("chronicle/20261010/beta/090000_300/meeting_transcript.md"),
        "token_x_and_y\n",
    )
    .unwrap();

    let talent_path = "20261010/alpha/090000_300/talents/alpha/brief.md";
    fs::create_dir_all(
        journal
            .path()
            .join("chronicle/20261010/alpha/090000_300/talents/alpha"),
    )
    .unwrap();
    fs::write(
        journal.path().join("chronicle").join(talent_path),
        "talent bytes\n",
    )
    .unwrap();
    plant_chunk(
        journal.path(),
        talent_path,
        "20261010",
        "alpha",
        0,
        "sharedneedle5 talent hit",
        "transcripts",
        "segment_assigned",
        &[FACET_A],
    );

    let act_path = "facets/alpha/activities/20261010.jsonl";
    fs::create_dir_all(journal.path().join("facets/alpha/activities")).unwrap();
    let line0 = json!({"id": "launch-sync", "segments": ["090000_300"]}).to_string();
    fs::write(journal.path().join(act_path), format!("{line0}\n")).unwrap();
    let p0 = produce_chunks(Family::Activity, act_path, &line0);
    let content0 = format!("{}\nsharedneedle5", p0.chunks[0].content);
    plant_chunk(
        journal.path(),
        act_path,
        "20261010",
        "",
        0,
        &content0,
        "facets",
        "facet_owned",
        &[FACET_A],
    );

    PermissionStore::open(journal.path())
        .set_permission(
            CONNECTION,
            ReadPermission {
                categories: vec!["transcripts".to_owned(), "facets".to_owned()],
                scope: ReadScope::Facets {
                    ids: vec![FACET_A.to_owned()],
                },
            },
        )
        .unwrap();

    let search = probe(
        &journal,
        "search",
        json!({"query": "sharedneedle5", "limit": 10}),
    )
    .unwrap();
    let search_str = search.to_string();
    assert!(
        !search_str.contains("token_x_and_y"),
        "token_x_and_y leaked: {search_str}"
    );

    let results = search["results"].as_array().unwrap();
    assert_eq!(results.len(), 2);
    for hit in results {
        let segs = hit["segments"].as_array().expect("hit has segments");
        assert_eq!(segs.len(), 1);
        let t = probe(
            &journal,
            "get_transcript",
            json!({"reference": segs[0]["reference"]}),
        )
        .unwrap();
        let entries: String = t["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e.as_str().unwrap())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(entries.contains("alpha_transcript_token"));
    }
}

#[test]
fn test_6_readable_segment_links_while_unreadable_omitted_and_post_grant_fails() {
    let journal = setup_shared_link_journal();
    let chronicle = journal.path().join("chronicle/20261010/alpha");

    let seg_screen = chronicle.join("110000_300");
    fs::create_dir_all(seg_screen.join("talents")).unwrap();
    fs::write(
        seg_screen.join("talents/facets.json"),
        "[{\"facet\":\"alpha\"}]",
    )
    .unwrap();
    fs::write(seg_screen.join("screen.png"), b"fake image bytes").unwrap();

    let seg_header = chronicle.join("120000_300");
    fs::create_dir_all(seg_header.join("talents")).unwrap();
    fs::write(
        seg_header.join("talents/facets.json"),
        "[{\"facet\":\"alpha\"}]",
    )
    .unwrap();
    fs::write(
        seg_header.join("conversation_transcript.jsonl"),
        "{\"imported\":{}}\n",
    )
    .unwrap();

    let seg_huge = chronicle.join("130000_300");
    fs::create_dir_all(seg_huge.join("talents")).unwrap();
    fs::write(
        seg_huge.join("talents/facets.json"),
        "[{\"facet\":\"alpha\"}]",
    )
    .unwrap();
    let huge_line = "a".repeat(70_000);
    fs::write(
        seg_huge.join("meeting_transcript.md"),
        format!("{huge_line}\n"),
    )
    .unwrap();

    let paths = [
        "20261010/alpha/090000_300/talents/brief.md",
        "20261010/alpha/100000_300/talents/brief.md",
        "20261010/alpha/110000_300/talents/brief.md",
        "20261010/alpha/120000_300/talents/brief.md",
        "20261010/alpha/130000_300/talents/brief.md",
    ];
    for (i, p) in paths.iter().enumerate() {
        plant_chunk(
            journal.path(),
            p,
            "20261010",
            "alpha",
            i as i64,
            "sharedneedle6 hit",
            "transcripts",
            "segment_assigned",
            &[FACET_A],
        );
    }

    PermissionStore::open(journal.path())
        .set_permission(CONNECTION, ReadPermission::default_whole_journal())
        .unwrap();

    let search = probe(
        &journal,
        "search",
        json!({"query": "sharedneedle6", "limit": 10}),
    )
    .unwrap();
    let results = search["results"].as_array().unwrap();
    assert_eq!(results.len(), 5);
    let (readable_hits, unreadable_hits): (Vec<_>, Vec<_>) =
        results.iter().partition(|h| h.get("segments").is_some());
    assert_eq!(readable_hits.len(), 1);
    assert_eq!(unreadable_hits.len(), 4);

    let old_link = readable_hits[0]["segments"][0]["reference"]
        .as_str()
        .unwrap()
        .to_owned();
    PermissionStore::open(journal.path())
        .set_permission(CONNECTION, ReadPermission::default_whole_journal())
        .unwrap();
    assert_eq!(
        probe(&journal, "get_transcript", json!({"reference": old_link})),
        Err(McpProbeError::Unavailable)
    );

    let search2 = probe(
        &journal,
        "search",
        json!({"query": "sharedneedle6", "limit": 10}),
    )
    .unwrap();
    let results2 = search2["results"].as_array().unwrap();
    let readable_hits2: Vec<_> = results2
        .iter()
        .filter(|h| h.get("segments").is_some())
        .collect();
    assert_eq!(readable_hits2.len(), 1);
    let new_link = readable_hits2[0]["segments"][0]["reference"]
        .as_str()
        .unwrap()
        .to_owned();
    fs::remove_dir_all(journal.path().join("chronicle/20261010/alpha/090000_300")).unwrap();
    assert_eq!(
        probe(&journal, "get_transcript", json!({"reference": new_link})),
        Err(McpProbeError::Unavailable)
    );
}

#[test]
fn test_7_two_searches_digest_stable_and_targets_keep_coordinates() {
    let journal = setup_shared_link_journal();
    plant_chunk(
        journal.path(),
        "20261010/beta/090000_300/talents/brief.md",
        "20261010",
        "",
        0,
        "sharedneedle7 hit",
        "transcripts",
        "segment_assigned",
        &[FACET_A],
    );
    PermissionStore::open(journal.path())
        .set_permission(CONNECTION, ReadPermission::default_whole_journal())
        .unwrap();

    let search1 = probe(
        &journal,
        "search",
        json!({"query": "sharedneedle7", "limit": 10}),
    )
    .unwrap();
    let _search2 = probe(
        &journal,
        "search",
        json!({"query": "sharedneedle7", "limit": 10}),
    )
    .unwrap();

    let outcomes = find_audit_files(journal.path(), solstone_core_mcp_audit::OUTCOME_FILE);
    assert_eq!(outcomes.len(), 2);
    let o1: Value = serde_json::from_str(&fs::read_to_string(&outcomes[0]).unwrap()).unwrap();
    let o2: Value = serde_json::from_str(&fs::read_to_string(&outcomes[1]).unwrap()).unwrap();

    assert_eq!(o1["result"]["digest"], o2["result"]["digest"]);
    let targets = o1["result"]["targets"].as_array().unwrap();
    assert!(!targets.is_empty());
    for t in targets {
        let t_str = t.as_str().unwrap();
        assert!(t_str.contains('#'));
        assert!(!t_str.contains("/transcript.md"));
    }

    for hit in search1["results"].as_array().unwrap() {
        if let Some(segments) = hit.get("segments").and_then(Value::as_array) {
            for seg in segments {
                assert!(seg.get("time").is_some());
                assert!(seg.get("reference").is_some());
                assert_eq!(seg.as_object().unwrap().len(), 2);
            }
        }
    }
    assert_no_coordinate_keys(&search1);
}

#[test]
fn test_9_list_transcripts_time_filtering_cursor_pagination_and_audit() {
    let journal = tempfile::Builder::new()
        .prefix("list-test-")
        .tempdir_in(crate::test_scratch())
        .unwrap();
    setup_facets(journal.path());
    PermissionStore::open(journal.path())
        .set_permission(CONNECTION, ReadPermission::default_whole_journal())
        .unwrap();

    let chronicle = journal.path().join("chronicle");
    for (name, start, end) in [
        ("100000_300", "10:00", "10:05"),
        ("100200_300", "10:02", "10:07"),
        ("110000_300", "11:00", "11:05"),
    ] {
        let seg = chronicle.join("20261010/default").join(name);
        fs::create_dir_all(&seg).unwrap();
        fs::write(
            seg.join("meeting_transcript.md"),
            format!("text for {start}-{end}\n"),
        )
        .unwrap();
    }

    let r_range = probe(
        &journal,
        "list_transcripts",
        json!({"day": "20261010", "time_from": "10:05", "time_to": "11:00"}),
    )
    .unwrap();
    let tr = r_range["transcripts"].as_array().unwrap();
    assert_eq!(tr.len(), 1);
    assert_eq!(tr[0]["time"], "10:02-10:07");

    let r_empty = probe(
        &journal,
        "list_transcripts",
        json!({"day": "20261010", "time_from": "10:05", "time_to": "10:05"}),
    )
    .unwrap();
    assert!(r_empty["transcripts"].as_array().unwrap().is_empty());

    for stream in ["alpha", "beta"] {
        let seg = chronicle.join("20261010").join(stream).join("090000_300");
        fs::create_dir_all(&seg).unwrap();
        fs::write(
            seg.join("meeting_transcript.md"),
            format!("{stream} token\n"),
        )
        .unwrap();
    }

    let p1 = probe(
        &journal,
        "list_transcripts",
        json!({"day": "20261010", "limit": 1}),
    )
    .unwrap();
    assert_eq!(p1["transcripts"].as_array().unwrap().len(), 1);
    let cur1 = p1["next_cursor"].as_str().expect("has cursor").to_owned();

    let p2 = probe(
        &journal,
        "list_transcripts",
        json!({"day": "20261010", "limit": 1, "cursor": cur1}),
    )
    .unwrap();
    assert_eq!(p2["transcripts"].as_array().unwrap().len(), 1);
    assert_ne!(
        p1["transcripts"][0]["reference"],
        p2["transcripts"][0]["reference"]
    );

    for day in ["20261005", "20261012", "20261001"] {
        let seg = chronicle.join(day).join("default/090000_300");
        fs::create_dir_all(&seg).unwrap();
        fs::write(seg.join("meeting_transcript.md"), "words\n").unwrap();
    }
    let all_days = probe(&journal, "list_transcripts", json!({"limit": 10})).unwrap();
    let dates: Vec<&str> = all_days["transcripts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["date"].as_str().unwrap())
        .collect();
    let mut sorted_dates = dates.clone();
    sorted_dates.sort_by(|a, b| b.cmp(a));
    assert_eq!(dates, sorted_dates);

    let seg_prior = chronicle.join("20261010/default/080000_300");
    fs::create_dir_all(&seg_prior).unwrap();
    let p2_again = probe(
        &journal,
        "list_transcripts",
        json!({"day": "20261010", "limit": 1, "cursor": cur1}),
    )
    .unwrap();
    assert_eq!(
        p2_again["transcripts"][0]["date"],
        p2["transcripts"][0]["date"]
    );
    assert_eq!(
        p2_again["transcripts"][0]["time"],
        p2["transcripts"][0]["time"]
    );
    let t1 = probe(
        &journal,
        "get_transcript",
        json!({"reference": p2["transcripts"][0]["reference"]}),
    )
    .unwrap();
    let t2 = probe(
        &journal,
        "get_transcript",
        json!({"reference": p2_again["transcripts"][0]["reference"]}),
    )
    .unwrap();
    assert_eq!(t1, t2);

    assert_eq!(
        probe(&journal, "list_transcripts", json!({"time_from": "10:05"})),
        Err(McpProbeError::InvalidInput)
    );
    assert_eq!(
        probe(
            &journal,
            "list_transcripts",
            json!({"day": "20261010", "time_from": "11:00", "time_to": "10:00"})
        ),
        Err(McpProbeError::InvalidInput)
    );
    assert_eq!(
        probe(
            &journal,
            "list_transcripts",
            json!({"day": "20261012", "cursor": cur1})
        ),
        Err(McpProbeError::Unavailable)
    );
    assert_eq!(
        probe(
            &journal,
            "list_transcripts",
            json!({"day": "20261010", "time_from": "10:00", "cursor": cur1})
        ),
        Err(McpProbeError::Unavailable)
    );
    assert_eq!(
        probe(
            &journal,
            "list_transcripts",
            json!({"day": "20261010", "facet": FACET_B, "cursor": cur1})
        ),
        Err(McpProbeError::Unavailable)
    );
    assert_eq!(
        probe(&journal, "search", json!({"query": "test", "cursor": cur1})),
        Err(McpProbeError::Unavailable)
    );

    let huge_line = "b".repeat(70_000);
    let seg_huge_in = chronicle.join("20261020/alpha/090000_300");
    fs::create_dir_all(seg_huge_in.join("talents")).unwrap();
    fs::write(
        seg_huge_in.join("talents/facets.json"),
        "[{\"facet\":\"alpha\"}]",
    )
    .unwrap();
    fs::write(
        seg_huge_in.join("meeting_transcript.md"),
        format!("{huge_line}\n"),
    )
    .unwrap();

    let seg_huge_out = chronicle.join("20261020/beta/090000_300");
    fs::create_dir_all(seg_huge_out.join("talents")).unwrap();
    fs::write(
        seg_huge_out.join("talents/facets.json"),
        "[{\"facet\":\"beta\"}]",
    )
    .unwrap();
    fs::write(
        seg_huge_out.join("meeting_transcript.md"),
        format!("{huge_line}\n"),
    )
    .unwrap();

    let seg_ok = chronicle.join("20261020/alpha/090500_300");
    fs::create_dir_all(seg_ok.join("talents")).unwrap();
    fs::write(
        seg_ok.join("talents/facets.json"),
        "[{\"facet\":\"alpha\"}]",
    )
    .unwrap();
    fs::write(seg_ok.join("meeting_transcript.md"), "ok words\n").unwrap();

    PermissionStore::open(journal.path())
        .set_permission(
            CONNECTION,
            ReadPermission {
                categories: vec!["transcripts".to_owned()],
                scope: ReadScope::Facets {
                    ids: vec![FACET_A.to_owned()],
                },
            },
        )
        .unwrap();

    let chosen_resp = probe(&journal, "list_transcripts", json!({"day": "20261020"})).unwrap();
    assert_eq!(chosen_resp["unreadable"], 1);
    assert_eq!(chosen_resp["transcripts"].as_array().unwrap().len(), 1);

    PermissionStore::open(journal.path())
        .set_permission(CONNECTION, ReadPermission::default_whole_journal())
        .unwrap();
    let p_time1 = probe(
        &journal,
        "list_transcripts",
        json!({"day": "20261010", "time_from": "08:00", "time_to": "11:30", "limit": 1}),
    )
    .unwrap();
    let cur_time = p_time1["next_cursor"]
        .as_str()
        .expect("has cursor")
        .to_owned();

    let p_audit = probe(
        &journal,
        "list_transcripts",
        json!({"day": "20261010", "time_from": "08:00", "time_to": "11:30", "cursor": cur_time}),
    )
    .unwrap();
    assert!(p_audit["transcripts"].is_array());

    let interactions = find_audit_files(journal.path(), solstone_core_mcp_audit::INTERACTION_FILE);
    let matched_interaction = interactions
        .iter()
        .filter_map(|p| {
            let val: Value = serde_json::from_str(&fs::read_to_string(p).ok()?).ok()?;
            if val["request"]["arguments"].get("time_from") == Some(&json!("08:00"))
                && val["request"]["arguments"].get("cursor").is_some()
            {
                Some(val)
            } else {
                None
            }
        })
        .next()
        .expect("found interaction with time_from 08:00 and cursor");
    assert_eq!(
        matched_interaction["request"]["arguments"]["time_from"],
        "08:00"
    );
    assert_eq!(
        matched_interaction["request"]["arguments"]["time_to"],
        "11:30"
    );
    let recorded_cursor = matched_interaction["request"]["arguments"]["cursor"]
        .as_str()
        .unwrap();
    assert_eq!(recorded_cursor.len(), 16);
    assert!(recorded_cursor.chars().all(|c| c.is_ascii_hexdigit()));
    assert_ne!(recorded_cursor, cur_time);
}

#[test]
fn test_11_tokens_in_raw_audio_never_appear_in_search_or_fetch() {
    let journal = setup_shared_link_journal();
    let seg = journal.path().join("chronicle/20261010/alpha/090000_300");
    fs::write(
        seg.join("audio.jsonl"),
        "{\"start\":\"00:00:02\",\"text\":\"tokenonlyinaudio\"}\n",
    )
    .unwrap();
    fs::write(
        seg.join("imported_audio.jsonl"),
        "{\"start\":\"00:00:05\",\"text\":\"tokenonlyinimportedaudio\"}\n",
    )
    .unwrap();

    plant_chunk(
        journal.path(),
        "20261010/alpha/090000_300/talents/brief.md",
        "20261010",
        "alpha",
        0,
        "brief text",
        "transcripts",
        "segment_assigned",
        &[FACET_A],
    );
    PermissionStore::open(journal.path())
        .set_permission(CONNECTION, ReadPermission::default_whole_journal())
        .unwrap();

    let s1 = probe(&journal, "search", json!({"query": "tokenonlyinaudio"})).unwrap();
    assert!(s1["results"].as_array().unwrap().is_empty());
    let s2 = probe(
        &journal,
        "search",
        json!({"query": "tokenonlyinimportedaudio"}),
    )
    .unwrap();
    assert!(s2["results"].as_array().unwrap().is_empty());

    let s_brief = probe(&journal, "search", json!({"query": "brief text"})).unwrap();
    let results = s_brief["results"].as_array().unwrap();
    assert_eq!(results.len(), 1);
    let ref_brief = &results[0]["reference"];
    let fetched = probe(&journal, "fetch", json!({"reference": ref_brief})).unwrap();
    let fetched_str = fetched.to_string();
    assert!(!fetched_str.contains("tokenonlyinaudio"));
    assert!(!fetched_str.contains("tokenonlyinimportedaudio"));
}

#[test]
fn test_12_issuer_counting_checker_budgets_and_more_segments() {
    let journal = tempfile::Builder::new()
        .prefix("issuer-test-")
        .tempdir_in(crate::test_scratch())
        .unwrap();
    setup_facets(journal.path());

    let mut keys = Vec::new();
    for i in 0..288 {
        let sec = i * 300;
        let h = sec / 3600;
        let m = (sec % 3600) / 60;
        let s = sec % 60;
        let key = format!("{h:02}{m:02}{s:02}_300");
        fs::create_dir_all(journal.path().join("chronicle/20261010").join(&key)).unwrap();
        keys.push(key);
    }

    let act_path = "facets/alpha/activities/20261010.jsonl";
    fs::create_dir_all(journal.path().join("facets/alpha/activities")).unwrap();
    let record_line = json!({"id": "act-288", "segments": keys}).to_string();
    fs::write(journal.path().join(act_path), format!("{record_line}\n")).unwrap();

    let chunks = produce_chunks(Family::Activity, act_path, &record_line);
    let text = chunks.chunks[0].content.clone();

    let principal = DispatchPrincipal {
        connection: CONNECTION,
        agent_identity: CONNECTION,
    };
    let mut categories = BTreeSet::new();
    categories.insert(AdmittedCategory::Transcripts);
    let snapshot = ConnectionReadSnapshot {
        categories,
        scope: ConnectionScope::WholeJournal,
        generation: 1,
    };

    let subject = LinkSubject {
        path: act_path,
        idx: 0,
        text: &text,
        segments: Vec::new(),
        more_segments: false,
    };
    let mut checker = CountingChecker { count: 0 };
    let mut subjects = [subject];
    issue_segment_links(
        journal.path(),
        principal,
        &snapshot,
        &mut subjects,
        48,
        48,
        &mut checker,
    );
    assert_eq!(checker.count, 48);
    assert_eq!(subjects[0].segments.len(), 48);
    assert!(subjects[0].more_segments);

    let subject30 = LinkSubject {
        path: act_path,
        idx: 0,
        text: &text,
        segments: Vec::new(),
        more_segments: false,
    };
    let mut checker30 = CountingChecker { count: 0 };
    let mut subjects30 = [subject30];
    issue_segment_links(
        journal.path(),
        principal,
        &snapshot,
        &mut subjects30,
        48,
        30,
        &mut checker30,
    );
    assert_eq!(checker30.count, 30);
    assert_eq!(subjects30[0].segments.len(), 30);
    assert!(subjects30[0].more_segments);

    let chronicle2 = journal.path().join("chronicle/20261011");
    let seg_in = chronicle2.join("alpha/000000_300");
    fs::create_dir_all(seg_in.join("talents")).unwrap();
    fs::write(
        seg_in.join("talents/facets.json"),
        "[{\"facet\":\"alpha\"}]",
    )
    .unwrap();

    let seg_out = chronicle2.join("beta/000500_300");
    fs::create_dir_all(seg_out.join("talents")).unwrap();
    fs::write(
        seg_out.join("talents/facets.json"),
        "[{\"facet\":\"beta\"}]",
    )
    .unwrap();

    let act_path2 = "facets/alpha/activities/20261011.jsonl";
    let record_line2 =
        json!({"id": "act-two", "segments": ["000000_300", "000500_300"]}).to_string();
    fs::create_dir_all(journal.path().join("facets/alpha/activities")).unwrap();
    fs::write(journal.path().join(act_path2), format!("{record_line2}\n")).unwrap();
    let chunks2 = produce_chunks(Family::Activity, act_path2, &record_line2);
    let text2 = chunks2.chunks[0].content.clone();

    let mut cats_chosen = BTreeSet::new();
    cats_chosen.insert(AdmittedCategory::Transcripts);
    let snapshot_chosen = ConnectionReadSnapshot {
        categories: cats_chosen,
        scope: ConnectionScope::ChosenFacets {
            ids: BTreeSet::from([FACET_A.to_owned()]),
        },
        generation: 1,
    };
    let subject_budget1 = LinkSubject {
        path: act_path2,
        idx: 0,
        text: &text2,
        segments: Vec::new(),
        more_segments: false,
    };
    let mut checker_budget1 = CountingChecker { count: 0 };
    let mut subjects_budget1 = [subject_budget1];
    issue_segment_links(
        journal.path(),
        principal,
        &snapshot_chosen,
        &mut subjects_budget1,
        48,
        1,
        &mut checker_budget1,
    );
    assert_eq!(checker_budget1.count, 1);
    assert_eq!(subjects_budget1[0].segments.len(), 1);
    assert!(subjects_budget1[0].more_segments);
}

#[test]
fn test_12a_issuer_omits_transcripts_when_category_excluded() {
    let journal = setup_shared_link_journal();
    let act_path = "facets/alpha/activities/20261010.jsonl";
    fs::create_dir_all(journal.path().join("facets/alpha/activities")).unwrap();
    let line0 = json!({"id": "launch-sync", "segments": ["090000_300"]}).to_string();
    fs::write(journal.path().join(act_path), format!("{line0}\n")).unwrap();
    let chunks = produce_chunks(Family::Activity, act_path, &line0);
    let text = chunks.chunks[0].content.clone();

    let principal = DispatchPrincipal {
        connection: CONNECTION,
        agent_identity: CONNECTION,
    };

    let mut cats_with_tr = BTreeSet::new();
    cats_with_tr.insert(AdmittedCategory::Transcripts);
    let snap_tr = ConnectionReadSnapshot {
        categories: cats_with_tr,
        scope: ConnectionScope::WholeJournal,
        generation: 1,
    };
    let sub_tr = LinkSubject {
        path: act_path,
        idx: 0,
        text: &text,
        segments: Vec::new(),
        more_segments: false,
    };
    let mut chk_tr = CountingChecker { count: 0 };
    let mut subs_tr = [sub_tr];
    issue_segment_links(
        journal.path(),
        principal,
        &snap_tr,
        &mut subs_tr,
        48,
        48,
        &mut chk_tr,
    );
    assert_eq!(chk_tr.count, 2);
    assert_eq!(subs_tr[0].segments.len(), 2);

    let mut cats_no_tr = BTreeSet::new();
    cats_no_tr.insert(AdmittedCategory::Entities);
    let snap_no_tr = ConnectionReadSnapshot {
        categories: cats_no_tr,
        scope: ConnectionScope::WholeJournal,
        generation: 1,
    };
    let sub_no_tr = LinkSubject {
        path: act_path,
        idx: 0,
        text: &text,
        segments: Vec::new(),
        more_segments: false,
    };
    let mut chk_no_tr = CountingChecker { count: 0 };
    let mut subs_no_tr = [sub_no_tr];
    issue_segment_links(
        journal.path(),
        principal,
        &snap_no_tr,
        &mut subs_no_tr,
        48,
        48,
        &mut chk_no_tr,
    );
    assert_eq!(chk_no_tr.count, 0);
    assert!(subs_no_tr[0].segments.is_empty());
    assert!(!subs_no_tr[0].more_segments);
}
