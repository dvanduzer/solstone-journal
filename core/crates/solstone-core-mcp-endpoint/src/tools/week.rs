// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::path::Path;

use chrono::Datelike;
use serde::Deserialize;
use serde_json::{Value, json};
use solstone_core_home::weekly::week_day;

use super::ToolError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ValidatedGetWeek {
    pub(crate) week: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WeekParams {
    #[serde(default)]
    week: Option<Value>,
}

pub(crate) fn validate(params: Option<&Value>) -> Result<ValidatedGetWeek, ToolError> {
    let params = match params {
        None | Some(Value::Null) => return Ok(ValidatedGetWeek { week: None }),
        Some(val) => val,
    };
    let params = serde_json::from_value::<WeekParams>(params.clone())
        .map_err(|_| ToolError::InvalidInput)?;

    let week = match params.week {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => {
            if s.len() != 8 || !s.bytes().all(|b| b.is_ascii_digit()) || week_day(&s).is_none() {
                return Err(ToolError::InvalidInput);
            }
            Some(s)
        }
        Some(_) => return Err(ToolError::InvalidInput),
    };

    Ok(ValidatedGetWeek { week })
}

pub(crate) struct WeekRead {
    pub(crate) value: Value,
    pub(crate) count: usize,
    pub(crate) targets: Vec<String>,
}

pub(crate) fn read_week(journal: &Path, week: Option<&str>) -> Result<WeekRead, ToolError> {
    if let Some(day) = week {
        let Some(date) = week_day(day) else {
            return Err(ToolError::InvalidInput);
        };
        if date.weekday() != chrono::Weekday::Sun {
            return Ok(empty_week());
        }
        return project_stem(journal, day);
    }

    match solstone_core_home::weekly::newest_week_candidate(journal) {
        solstone_core_home::weekly::WeeklyDirectoryCandidate::Absent
        | solstone_core_home::weekly::WeeklyDirectoryCandidate::Empty => Ok(empty_week()),
        solstone_core_home::weekly::WeeklyDirectoryCandidate::Unreadable => {
            Err(ToolError::FileUnreadable)
        }
        solstone_core_home::weekly::WeeklyDirectoryCandidate::Candidate { stem, kind } => {
            if kind != solstone_core_journal_io::JournalEntryKind::RegularFile {
                return Err(ToolError::FileUnreadable);
            }
            project_stem(journal, &stem)
        }
    }
}

fn empty_week() -> WeekRead {
    WeekRead {
        value: json!({"week": null}),
        count: 0,
        targets: Vec::new(),
    }
}

fn project_stem(journal: &Path, stem: &str) -> Result<WeekRead, ToolError> {
    use solstone_core_home::weekly::{
        LeftOut, Said, WeekJudgment, checked_neighbors, format_week_intro, judge, read_left_out,
        resolve_day_cell_state,
    };

    let trusted = match judge(journal, stem) {
        WeekJudgment::Absent | WeekJudgment::CantShow => return Ok(empty_week()),
        WeekJudgment::CouldntCheck | WeekJudgment::Untrusted => {
            return Err(ToolError::FileUnreadable);
        }
        WeekJudgment::Page(trusted) => trusted,
    };
    let Said::Entries(said_entries) = &trusted.said else {
        return Err(ToolError::FileUnreadable);
    };
    let left_out_keys = match read_left_out(journal) {
        LeftOut::Unreadable => return Err(ToolError::FileUnreadable),
        LeftOut::None => std::collections::BTreeSet::new(),
        LeftOut::Keys(keys) => keys,
    };
    let navigation = checked_neighbors(journal, stem);
    if navigation.failed {
        return Err(ToolError::FileUnreadable);
    }

    let memory_by_id = trusted
        .memories
        .iter()
        .map(|memory| (memory.id.as_str(), memory))
        .collect::<std::collections::BTreeMap<_, _>>();
    let mut days = Vec::with_capacity(trusted.days.len());
    let mut memories = Vec::new();
    for day in &trusted.days {
        let memory = day
            .memory_id
            .as_deref()
            .and_then(|id| memory_by_id.get(id).copied());
        let memory_key = memory.map(|memory| memory.key.as_str());
        let state = resolve_day_cell_state(day.state.as_str(), memory_key, &left_out_keys);
        days.push(json!({"day": day.day, "state": state}));
        if day.state == "memory"
            && let Some(memory) = memory
            && !left_out_keys.contains(&memory.key)
        {
            memories.push(json!({
                "day": memory.day,
                "text": memory.text,
                "source": memory.source.uri,
                "refs": memory.source.refs,
            }));
        }
    }
    let mut said = Vec::new();
    for entry in said_entries {
        if left_out_keys.contains(&entry.key) {
            continue;
        }
        said.push(json!({
            "day": entry.day,
            "quote": entry.quote,
            "uri": entry.uri,
        }));
    }
    let intro = format_week_intro(memories.len(), said.len());
    Ok(WeekRead {
        value: json!({
            "week": {"start": trusted.start_day, "end": trusted.end_day},
            "intro": intro,
            "days": days,
            "memories": memories,
            "said": said,
            "previous": navigation.previous,
            "next": navigation.next,
        }),
        count: 1,
        targets: vec![format!("week:{stem}")],
    })
}

#[cfg(all(test, not(feature = "full-tests")))]
mod tests {
    use std::collections::BTreeSet;
    use std::fs;
    use std::path::Path;

    use chrono::{Duration, NaiveDate, Utc};
    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};
    use solstone_core_home::weekly::{left_out_bytes, page_model};
    use solstone_core_mcp_audit::ToolName;

    use super::validate;
    use crate::activity::{ActivityQuery, RecordedOutcome, read_activity};
    use crate::dispatch::{
        DispatchError, DispatchPrincipal, McpProbeError, dispatch_authenticated_tool_call,
        run_mcp_probe,
    };
    use crate::jsonrpc::{JsonRpcResponse, ToolName as JsonTool, tool_result};
    use crate::permissions::{
        PermissionStore, ReadPermission, ReadScope, evaluate_connection_read,
    };
    use crate::registry::advertised_tools_list;
    use crate::tools::ToolError;

    const CONNECTION: &str = "bearer:week";
    const EMPTY_NOTE: &str = "no week to read. this is a complete answer, not an error.";
    const WHOLE_JOURNAL: &str =
        "this connection can read a week only with access to the whole journal.";

    fn journal(prefix: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(prefix)
            .tempdir_in(crate::test_scratch())
            .unwrap()
    }

    fn grant(root: &Path, categories: &[&str], scope: ReadScope) {
        PermissionStore::open(root)
            .set_permission(
                CONNECTION,
                ReadPermission {
                    categories: categories
                        .iter()
                        .map(|category| (*category).to_owned())
                        .collect(),
                    scope,
                },
            )
            .unwrap();
    }

    fn whole(root: &Path) {
        grant(root, &["transcripts", "facets"], ReadScope::WholeJournal);
    }

    fn probe(root: &Path, arguments: Value) -> Result<Value, McpProbeError> {
        run_mcp_probe(root, CONNECTION, "get_week", &arguments)
    }

    fn dispatch_week(
        root: &Path,
        arguments: Value,
    ) -> Result<crate::dispatch::ToolOutput, DispatchError> {
        dispatch_authenticated_tool_call(
            root,
            DispatchPrincipal {
                connection: CONNECTION,
                agent_identity: CONNECTION,
            },
            JsonTool::GetWeek,
            Some(&arguments),
            Utc::now(),
        )
    }

    fn page(root: &Path) -> crate::activity::ActivityPage {
        read_activity(
            root,
            &ActivityQuery {
                limit: 10,
                ..ActivityQuery::default()
            },
        )
        .unwrap()
    }

    fn names(root: &Path) -> Vec<String> {
        let decision = evaluate_connection_read(root, CONNECTION);
        advertised_tools_list(root, &decision)["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| {
                assert_eq!(
                    tool["annotations"]["readOnlyHint"],
                    tool["name"] != "save_memory"
                );
                tool["name"].as_str().unwrap().to_owned()
            })
            .collect()
    }

    fn shift(sunday: &str, days: i64) -> String {
        let date = NaiveDate::parse_from_str(sunday, "%Y%m%d").unwrap();
        (date + Duration::days(days)).format("%Y%m%d").to_string()
    }

    fn write_raw(root: &Path, relative: &str, body: &str) {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    #[derive(Clone, Copy)]
    struct Memory<'a> {
        id: &'a str,
        key: &'a str,
        day_offset: i64,
        text: &'a str,
        uri: &'a str,
        refs: &'a [&'a str],
    }

    fn document(sunday: &str, memories: &[Memory<'_>], said: &str) -> String {
        let end = shift(sunday, 6);
        let days = (0..7)
            .map(|offset| {
                let day = shift(sunday, offset);
                memories
                    .iter()
                    .find(|memory| memory.day_offset == offset)
                    .map(|memory| {
                        format!(
                            r#"{{"day":"{day}","state":"memory","memory_id":"{}"}}"#,
                            memory.id
                        )
                    })
                    .unwrap_or_else(|| format!(r#"{{"day":"{day}","state":"nothing_shared"}}"#))
            })
            .collect::<Vec<_>>()
            .join(",");
        let memories_json = memories
            .iter()
            .map(|memory| {
                let day = shift(sunday, memory.day_offset);
                let refs = memory
                    .refs
                    .iter()
                    .map(|reference| format!("\"{reference}\""))
                    .collect::<Vec<_>>()
                    .join(",");
                format!(
                    r#"{{"id":"{}","key":"{}","day":"{day}","text":"{}","source":{{"kind":"briefing","uri":"{}","briefing_day":"{day}","refs":[{refs}]}}}}"#,
                    memory.id, memory.key, memory.text, memory.uri
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        format!(
            r#"{{"version":1,"week":{{"start":"{sunday}","end":"{end}"}},"days":[{days}],"memories":[{memories_json}],"said":{said}}}"#
        )
    }

    fn said_item(id: &str, key: &str, day: &str, quote: &str, uri: &str) -> String {
        format!(
            r#"{{"id":"{id}","key":"{key}","day":"{day}","quote":"{quote}","source":{{"kind":"activity","uri":"{uri}"}}}}"#
        )
    }

    fn said_key(day: &str, facet: &str, record_id: &str, quote: &str) -> String {
        let hex = format!(
            "{:x}",
            Sha256::digest(format!("said\n{day}\n{facet}\n{record_id}\n{quote}").as_bytes())
        );
        hex[..16].to_owned()
    }

    fn write_left_out(root: &Path, keys: &[&str]) {
        let keys = keys
            .iter()
            .map(|key| (*key).to_owned())
            .collect::<BTreeSet<_>>();
        write_raw(
            root,
            "health/week-left-out.json",
            &String::from_utf8(left_out_bytes(&keys)).unwrap(),
        );
    }

    fn assert_closed(value: &Value) {
        let keys = value
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>();
        assert_eq!(
            keys,
            BTreeSet::from([
                "days".to_owned(),
                "intro".to_owned(),
                "memories".to_owned(),
                "next".to_owned(),
                "previous".to_owned(),
                "said".to_owned(),
                "week".to_owned(),
            ])
        );
        let week_keys = value["week"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>();
        assert_eq!(
            week_keys,
            BTreeSet::from(["end".to_owned(), "start".to_owned()])
        );
        for day in value["days"].as_array().unwrap() {
            assert_eq!(
                day.as_object()
                    .unwrap()
                    .keys()
                    .cloned()
                    .collect::<BTreeSet<_>>(),
                BTreeSet::from(["day".to_owned(), "state".to_owned()])
            );
        }
        for memory in value["memories"].as_array().unwrap() {
            assert_eq!(
                memory
                    .as_object()
                    .unwrap()
                    .keys()
                    .cloned()
                    .collect::<BTreeSet<_>>(),
                BTreeSet::from([
                    "day".to_owned(),
                    "refs".to_owned(),
                    "source".to_owned(),
                    "text".to_owned(),
                ])
            );
            assert!(memory["source"].is_string());
            assert!(memory["refs"].is_array());
        }
        for quote in value["said"].as_array().unwrap() {
            assert_eq!(
                quote
                    .as_object()
                    .unwrap()
                    .keys()
                    .cloned()
                    .collect::<BTreeSet<_>>(),
                BTreeSet::from(["day".to_owned(), "quote".to_owned(), "uri".to_owned()])
            );
        }
    }

    fn assert_matches_page(root: &Path, sunday: &str, value: &Value) {
        let model = page_model(root, sunday, 2026, &|_| false).unwrap();
        assert_eq!(value["intro"], model["intro"]);
        let cells = model["cells"].as_array().unwrap();
        let days = value["days"].as_array().unwrap();
        assert_eq!(days.len(), 7);
        for (cell, day) in cells.iter().zip(days) {
            assert_eq!(day["day"], cell["day"]);
            assert_eq!(day["state"], cell["state"]);
        }
        let row_text = model["rows"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|row| row.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>();
        let memory_text = value["memories"]
            .as_array()
            .unwrap()
            .iter()
            .map(|memory| memory["text"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(memory_text, row_text);
        assert_eq!(value["previous"].as_str(), href_stem(&model["prev_href"]));
        assert_eq!(value["next"].as_str(), href_stem(&model["next_href"]));
    }

    fn href_stem(href: &Value) -> Option<&str> {
        href.as_str().and_then(|href| href.rsplit('/').next())
    }

    fn one_memory<'a>(text: &'a str, uri: &'a str) -> Memory<'a> {
        Memory {
            id: "m0",
            key: "memory-key",
            day_offset: 0,
            text,
            uri,
            refs: &["sol://chronicle/20260308/room/120000_1"],
        }
    }

    #[test]
    fn whole_journal_grant_advertises_and_calls_get_week() {
        let root = journal("solstone-week-advertised-");
        whole(root.path());
        let decision = evaluate_connection_read(root.path(), CONNECTION);
        let listed = advertised_tools_list(root.path(), &decision);
        let tool = listed["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == "get_week")
            .unwrap();
        assert_eq!(tool["annotations"]["readOnlyHint"], true);
        write_raw(
            root.path(),
            "reflections/weekly/20260308.md",
            "# only markdown",
        );
        let value = probe(root.path(), json!({})).unwrap();
        assert_eq!(value, json!({"week": null}));
    }

    #[test]
    fn missing_categories_keep_the_existing_refusal() {
        let cases = [
            (
                &["facets"][..],
                "this connection does not have the transcripts category",
            ),
            (
                &["transcripts"][..],
                "this connection does not have the facets category",
            ),
            (
                &["entities"][..],
                "this connection does not have the transcripts and facets category",
            ),
        ];
        for (categories, reason) in cases {
            let root = journal("solstone-week-missing-");
            grant(root.path(), categories, ReadScope::WholeJournal);
            assert!(!names(root.path()).iter().any(|name| name == "get_week"));
            assert_eq!(
                probe(root.path(), json!({})),
                Err(McpProbeError::PermissionDenied)
            );
            let entry = &page(root.path()).entries[0];
            assert_eq!(entry.outcome, RecordedOutcome::Refused);
            assert_eq!(entry.tool_name, ToolName::GetWeek);
            assert_eq!(entry.reason.as_deref(), Some(reason));
        }
    }

    #[test]
    fn category_failure_wins_over_a_chosen_facet_grant() {
        let root = journal("solstone-week-category-wins-");
        grant(
            root.path(),
            &["entities"],
            ReadScope::Facets {
                ids: vec!["facet-a".to_owned(), "facet-b".to_owned()],
            },
        );
        assert_eq!(
            probe(root.path(), json!({})),
            Err(McpProbeError::PermissionDenied)
        );
        assert_eq!(
            page(root.path()).entries[0].reason.as_deref(),
            Some("this connection does not have the transcripts and facets category")
        );
    }

    #[test]
    fn chosen_facets_do_not_see_a_week_even_with_every_facet() {
        let root = journal("solstone-week-chosen-");
        for (name, id) in [("alpha", "facet-a"), ("beta", "facet-b")] {
            write_raw(
                root.path(),
                &format!("facets/{name}/facet.json"),
                &json!({"id": id, "title": name, "description": name}).to_string(),
            );
        }
        let mut ids = solstone_core_facets::list_declared_facet_names(root.path())
            .unwrap()
            .into_iter()
            .map(|name| {
                solstone_core_facets::read_facet_declaration(root.path(), &name)
                    .unwrap()
                    .unwrap()
                    .value()["id"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect::<Vec<_>>();
        ids.sort();
        assert_eq!(ids, vec!["facet-a".to_owned(), "facet-b".to_owned()]);
        grant(
            root.path(),
            &["transcripts", "facets"],
            ReadScope::Facets { ids },
        );
        assert!(!names(root.path()).iter().any(|name| name == "get_week"));
        assert_eq!(
            probe(root.path(), json!({})),
            Err(McpProbeError::PermissionDenied)
        );
        let reason = page(root.path()).entries[0].reason.clone().unwrap();
        assert_eq!(reason, WHOLE_JOURNAL);
        assert!(!reason.contains("does not have the"));
    }

    #[test]
    fn revoked_and_absent_grants_stay_on_the_denied_path() {
        let absent = journal("solstone-week-absent-grant-");
        assert!(!names(absent.path()).iter().any(|name| name == "get_week"));
        assert_eq!(
            probe(absent.path(), json!({})),
            Err(McpProbeError::PermissionDenied)
        );
        let absent_entry = &page(absent.path()).entries[0];
        assert_eq!(absent_entry.outcome, RecordedOutcome::Refused);
        assert_eq!(
            absent_entry.reason.as_deref(),
            Some("this connection has no read permission")
        );

        let revoked = journal("solstone-week-revoked-");
        whole(revoked.path());
        PermissionStore::open(revoked.path())
            .clear_permission(CONNECTION)
            .unwrap();
        assert!(!names(revoked.path()).iter().any(|name| name == "get_week"));
        assert_eq!(
            probe(revoked.path(), json!({})),
            Err(McpProbeError::PermissionDenied)
        );
        assert_eq!(
            page(revoked.path()).entries[0].reason.as_deref(),
            Some("this connection has no read permission")
        );
    }

    #[test]
    fn invalid_week_shapes_fail_before_any_journal_lookup() {
        let missing =
            crate::test_scratch().join(format!("solstone-week-no-journal-{}", std::process::id()));
        let _ = fs::remove_dir_all(&missing);
        assert!(!missing.exists());
        for week in [
            "2026-03-08",
            "..",
            r"C:\20260308",
            "20260308/../..",
            "20260231",
        ] {
            assert_eq!(
                validate(Some(&json!({"week": week}))),
                Err(ToolError::InvalidInput),
                "{week}"
            );
            assert_eq!(
                run_mcp_probe(&missing, CONNECTION, "get_week", &json!({"week": week})),
                Err(McpProbeError::InvalidInput),
                "{week}"
            );
        }
        assert!(!missing.exists());
    }

    #[test]
    fn absent_weeks_are_empty_answers() {
        let cases = [
            ("newest-missing", None, false),
            ("sunday-missing-dir", Some("20260308"), false),
            ("sunday-missing-file", Some("20260308"), true),
            ("markdown-only", None, true),
        ];
        for (name, week, create_weekly) in cases {
            let root = journal(&format!("solstone-week-empty-{name}-"));
            whole(root.path());
            if create_weekly {
                write_raw(
                    root.path(),
                    "reflections/weekly/20260308.md",
                    "# pre-format",
                );
            }
            let arguments = match week {
                Some(day) => json!({"week": day}),
                None => json!({}),
            };
            assert_eq!(
                probe(root.path(), arguments.clone()).unwrap(),
                json!({"week": null})
            );
            let output = dispatch_week(root.path(), arguments).unwrap();
            assert_eq!(output.value, json!({"week": null}));
            assert_eq!(output.empty_note.as_deref(), Some(EMPTY_NOTE));
            let rendered = tool_result(output.value, output.empty_note.as_deref());
            assert_eq!(rendered["content"][1]["text"], EMPTY_NOTE);
            for entry in page(root.path()).entries {
                assert_eq!(entry.outcome, RecordedOutcome::Empty);
                let shape = entry.result.unwrap();
                assert_eq!(shape.count, 0);
                assert!(
                    shape
                        .targets
                        .iter()
                        .all(|target| !target.starts_with("week:"))
                );
            }
        }
        assert_eq!(validate(None).unwrap().week, None);
        assert_eq!(validate(Some(&Value::Null)).unwrap().week, None);
        assert_eq!(validate(Some(&json!({"week": null}))).unwrap().week, None);
    }

    #[test]
    fn a_real_non_sunday_does_not_read_the_file() {
        let root = journal("solstone-week-monday-");
        whole(root.path());
        write_raw(
            root.path(),
            "reflections/weekly/20260309.json",
            r#"{"version":1,"sentinel":"SENTINEL_MONDAY"}"#,
        );
        let value = probe(root.path(), json!({"week": "20260309"})).unwrap();
        assert_eq!(value, json!({"week": null}));
        assert!(!value.to_string().contains("SENTINEL_MONDAY"));
        assert_eq!(page(root.path()).entries[0].outcome, RecordedOutcome::Empty);
    }

    #[test]
    fn an_untrusted_newest_week_does_not_fall_through() {
        let root = journal("solstone-week-untrusted-newest-");
        whole(root.path());
        let older = document(
            "20260308",
            &[one_memory("OLDER_WEEK_MEMORY", "sol://older")],
            "[]",
        );
        write_raw(root.path(), "reflections/weekly/20260308.json", &older);
        write_raw(
            root.path(),
            "reflections/weekly/20260315.json",
            r#"{"version":2,"days":[]}"#,
        );
        assert_eq!(
            probe(root.path(), json!({})),
            Err(McpProbeError::Unavailable)
        );
        let entry = &page(root.path()).entries[0];
        assert_eq!(entry.outcome, RecordedOutcome::Error);
        assert!(entry.result.is_none());
        assert!(
            !entry
                .reason
                .as_deref()
                .unwrap_or("")
                .contains("OLDER_WEEK_MEMORY")
        );
    }

    #[test]
    fn a_served_week_matches_the_page_and_keeps_neighbors() {
        let root = journal("solstone-week-parity-");
        whole(root.path());
        let fixture =
            include_str!("../../../../../tests/fixtures/journal/reflections/weekly/20260308.json");
        write_raw(
            root.path(),
            "reflections/weekly/20260301.json",
            &document("20260301", &[one_memory("earlier", "sol://earlier")], "[]"),
        );
        write_raw(root.path(), "reflections/weekly/20260308.json", fixture);
        write_raw(
            root.path(),
            "reflections/weekly/20260315.json",
            &document("20260315", &[one_memory("later", "sol://later")], "[]"),
        );
        write_raw(root.path(), "reflections/weekly/notadate.json", "{}");
        write_raw(root.path(), "reflections/weekly/20260399.json", "{}");
        write_raw(
            root.path(),
            "reflections/weekly/20260322.json",
            r#"{"version":2,"days":[]}"#,
        );
        let value = probe(root.path(), json!({"week": "20260308"})).unwrap();
        assert_closed(&value);
        assert_eq!(value["week"]["start"], "20260308");
        assert_eq!(value["week"]["end"], "20260314");
        assert_matches_page(root.path(), "20260308", &value);
        assert_eq!(value["said"].as_array().unwrap().len(), 0);
        let shape = page(root.path()).entries[0].result.clone().unwrap();
        assert_eq!(
            page(root.path()).entries[0].outcome,
            RecordedOutcome::Served
        );
        assert_eq!(shape.count, 1);
        assert_eq!(shape.targets, vec!["week:20260308".to_owned()]);

        let middle = probe(root.path(), json!({"week": "20260315"})).unwrap();
        assert_eq!(middle["previous"], "20260308");
        assert!(middle["next"].is_null());
    }

    #[test]
    fn quote_only_and_fully_left_out_weeks_are_still_served() {
        let sunday = "20260308";
        let quote_uri = "sol://facets/work/activities/20260308#meeting_1";
        let quote = said_item("s0", "quote-key", sunday, "the owner's words", quote_uri);

        let quote_only = journal("solstone-week-quote-only-");
        whole(quote_only.path());
        write_raw(
            quote_only.path(),
            "reflections/weekly/20260308.json",
            &document(sunday, &[], &format!("[{quote}]")),
        );
        let value = probe(quote_only.path(), json!({})).unwrap();
        assert_closed(&value);
        assert!(value["intro"].is_null());
        assert!(value["memories"].as_array().unwrap().is_empty());
        assert_eq!(value["said"][0]["quote"], "the owner's words");
        assert_matches_page(quote_only.path(), sunday, &value);
        assert_eq!(
            page(quote_only.path()).entries[0].outcome,
            RecordedOutcome::Served
        );
        assert_eq!(
            page(quote_only.path()).entries[0]
                .result
                .as_ref()
                .unwrap()
                .count,
            1
        );

        let hidden = journal("solstone-week-all-left-out-");
        whole(hidden.path());
        write_raw(
            hidden.path(),
            "reflections/weekly/20260308.json",
            &document(sunday, &[one_memory("HIDDEN_MEMORY", "sol://hidden")], "[]"),
        );
        write_left_out(hidden.path(), &["memory-key"]);
        let value = probe(hidden.path(), json!({"week": sunday})).unwrap();
        assert_closed(&value);
        assert_eq!(value["intro"], "nothing from this week is on this page.");
        assert!(value["memories"].as_array().unwrap().is_empty());
        assert!(!value.to_string().contains("HIDDEN_MEMORY"));
        assert_matches_page(hidden.path(), sunday, &value);
        let served = page(hidden.path()).entries[0].result.clone().unwrap();
        assert_eq!(
            page(hidden.path()).entries[0].outcome,
            RecordedOutcome::Served
        );
        assert_eq!(served.count, 1);
        assert_eq!(served.targets, vec!["week:20260308".to_owned()]);

        let kept_quote = journal("solstone-week-quote-kept-");
        whole(kept_quote.path());
        write_raw(
            kept_quote.path(),
            "reflections/weekly/20260308.json",
            &document(
                sunday,
                &[one_memory("HIDDEN_MEMORY", "sol://hidden")],
                &format!("[{quote}]"),
            ),
        );
        write_left_out(kept_quote.path(), &["memory-key"]);
        let value = probe(kept_quote.path(), json!({"week": sunday})).unwrap();
        assert!(value["intro"].is_null());
        assert!(value["memories"].as_array().unwrap().is_empty());
        assert_eq!(value["said"].as_array().unwrap().len(), 1);
        assert!(!value.to_string().contains("HIDDEN_MEMORY"));
        assert_matches_page(kept_quote.path(), sunday, &value);
        assert_eq!(
            page(kept_quote.path()).entries[0]
                .result
                .as_ref()
                .unwrap()
                .count,
            1
        );
    }

    #[test]
    fn memory_and_quote_leave_outs_are_independent() {
        let sunday = "20260308";
        let facet = "work";
        let record_id = "meeting_1";
        let uri = "sol://facets/work/activities/20260308#meeting_1";
        let omitted = "OMITTED_QUOTE_SENTINEL";
        let restored = "RESTORED_QUOTE";
        let old_key = said_key(sunday, facet, record_id, omitted);
        let new_key = said_key(sunday, facet, record_id, restored);
        assert_ne!(old_key, new_key);
        let root = journal("solstone-week-keys-");
        whole(root.path());
        let memory = Memory {
            id: "m0",
            key: "memory-key",
            day_offset: 0,
            text: "KEPT_MEMORY",
            uri,
            refs: &[],
        };
        let write_quote = |id: &str, key: &str, quote: &str| {
            let said = said_item(id, key, sunday, quote, uri);
            write_raw(
                root.path(),
                "reflections/weekly/20260308.json",
                &document(sunday, &[memory], &format!("[{said}]")),
            );
        };
        write_left_out(root.path(), &[&old_key]);
        write_quote("quote-1", &old_key, omitted);
        let hidden = probe(root.path(), json!({"week": sunday})).unwrap();
        assert_closed(&hidden);
        assert!(hidden["said"].as_array().unwrap().is_empty());
        assert!(!hidden.to_string().contains(omitted));
        assert_eq!(hidden["memories"][0]["text"], "KEPT_MEMORY");
        assert_eq!(hidden["memories"][0]["source"], uri);
        assert_eq!(hidden["days"][0]["state"], "memory");

        write_quote("quote-2", &old_key, omitted);
        let rewritten = probe(root.path(), json!({"week": sunday})).unwrap();
        assert!(rewritten["said"].as_array().unwrap().is_empty());
        assert!(!rewritten.to_string().contains(omitted));
        assert_eq!(rewritten["memories"], hidden["memories"]);

        write_quote("quote-2", &new_key, restored);
        let back = probe(root.path(), json!({"week": sunday})).unwrap();
        assert_eq!(back["said"][0]["quote"], restored);
        assert_eq!(back["said"][0]["uri"], uri);
        assert!(!back.to_string().contains(omitted));
        assert_eq!(back["memories"], hidden["memories"]);
        assert_matches_page(root.path(), sunday, &back);
    }

    #[test]
    fn an_unreadable_said_list_releases_nothing() {
        let root = journal("solstone-week-said-unreadable-");
        whole(root.path());
        // A said value that is not an array stays a page and is unreadable.
        write_raw(
            root.path(),
            "reflections/weekly/20260308.json",
            &document(
                "20260308",
                &[one_memory("SENTINEL_MEMORY", "sol://memory")],
                r#"{"quote":"SENTINEL_QUOTE"}"#,
            ),
        );
        let model = page_model(root.path(), "20260308", 2026, &|_| false).unwrap();
        assert_eq!(
            model["said"]["unreadable"],
            "the \"said by you\" list couldn't be read."
        );
        assert!(model.to_string().contains("SENTINEL_MEMORY"));
        match dispatch_week(root.path(), json!({"week": "20260308"})) {
            Err(DispatchError::Tool(error)) => {
                let rendered =
                    serde_json::to_string(&JsonRpcResponse::tool_failed(None, error)).unwrap();
                assert!(!rendered.contains("SENTINEL_MEMORY"));
                assert!(!rendered.contains("SENTINEL_QUOTE"));
            }
            Ok(_) => panic!("expected an unreadable file, got a tool result"),
            Err(error) => panic!("expected an unreadable file, got {error:?}"),
        }
        assert_eq!(
            probe(root.path(), json!({"week": "20260308"})),
            Err(McpProbeError::Unavailable)
        );
    }

    #[test]
    fn navigation_read_failures_refuse_but_malformed_candidates_are_skipped() {
        let root = journal("week-navigation-read-");
        whole(root.path());
        for sunday in ["20260308", "20260322"] {
            write_raw(
                root.path(),
                &format!("reflections/weekly/{sunday}.json"),
                &document(
                    sunday,
                    &[one_memory("VISIBLE_NAVIGATION_MEMORY", "sol://memory")],
                    "[]",
                ),
            );
        }
        let candidate = root.path().join("reflections/weekly/20260315.json");
        fs::create_dir(&candidate).unwrap();
        assert!(matches!(
            dispatch_week(root.path(), json!({"week":"20260308"})),
            Err(DispatchError::Tool(ToolError::FileUnreadable))
        ));
        assert_eq!(page(root.path()).entries[0].outcome, RecordedOutcome::Error);
        let fallback = page_model(root.path(), "20260308", 2026, &|_| false).unwrap();
        assert_eq!(fallback["next_href"], "/app/home/week/20260322");
        fs::remove_dir(&candidate).unwrap();
        fs::write(candidate, "not valid JSON").unwrap();
        let value = dispatch_week(root.path(), json!({"week":"20260308"}))
            .unwrap()
            .value;
        assert_eq!(value["next"], "20260322");
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escapes_and_a_non_regular_newest_week_fail_closed() {
        use std::os::unix::fs::symlink;

        let outside = journal("solstone-week-outside-");
        let sentinel = "OUTSIDE_WEEK_SENTINEL";
        write_raw(
            outside.path(),
            "secret.json",
            &format!(r#"{{"text":"{sentinel}"}}"#),
        );
        write_raw(
            outside.path(),
            "reflections/weekly/20260315.json",
            &document("20260315", &[one_memory(sentinel, "sol://outside")], "[]"),
        );

        let linked_file = journal("solstone-week-linked-file-");
        whole(linked_file.path());
        fs::create_dir_all(linked_file.path().join("reflections/weekly")).unwrap();
        symlink(
            outside.path().join("secret.json"),
            linked_file.path().join("reflections/weekly/20260308.json"),
        )
        .unwrap();
        write_raw(
            linked_file.path(),
            "reflections/weekly/20260301.json",
            &document(
                "20260301",
                &[one_memory("OLDER_REAL_WEEK", "sol://older")],
                "[]",
            ),
        );
        for arguments in [json!({"week": "20260308"}), json!({})] {
            assert_eq!(
                probe(linked_file.path(), arguments),
                Err(McpProbeError::Unavailable)
            );
            let entry = &page(linked_file.path()).entries[0];
            assert_eq!(entry.outcome, RecordedOutcome::Error);
            assert!(!entry.reason.as_deref().unwrap_or("").contains(sentinel));
            assert!(
                !entry
                    .reason
                    .as_deref()
                    .unwrap_or("")
                    .contains("OLDER_REAL_WEEK")
            );
        }

        let linked_leave_out = journal("solstone-week-linked-left-");
        whole(linked_leave_out.path());
        write_raw(
            linked_leave_out.path(),
            "reflections/weekly/20260308.json",
            &document(
                "20260308",
                &[one_memory("VISIBLE_MEMORY", "sol://memory")],
                "[]",
            ),
        );
        fs::create_dir_all(linked_leave_out.path().join("health")).unwrap();
        symlink(
            outside.path().join("secret.json"),
            linked_leave_out.path().join("health/week-left-out.json"),
        )
        .unwrap();
        assert_eq!(
            probe(linked_leave_out.path(), json!({"week": "20260308"})),
            Err(McpProbeError::Unavailable)
        );
        assert!(
            !page(linked_leave_out.path()).entries[0]
                .reason
                .as_deref()
                .unwrap_or("")
                .contains(sentinel)
        );
        assert!(
            !page(linked_leave_out.path()).entries[0]
                .reason
                .as_deref()
                .unwrap_or("")
                .contains("VISIBLE_MEMORY")
        );

        let linked_ancestor = journal("solstone-week-linked-ancestor-");
        whole(linked_ancestor.path());
        symlink(
            outside.path().join("reflections"),
            linked_ancestor.path().join("reflections"),
        )
        .unwrap();
        assert_eq!(
            probe(linked_ancestor.path(), json!({"week": "20260315"})),
            Err(McpProbeError::Unavailable)
        );
        assert_eq!(
            probe(linked_ancestor.path(), json!({})),
            Err(McpProbeError::Unavailable)
        );
        assert!(page(linked_ancestor.path()).entries.iter().all(|entry| {
            entry.outcome == RecordedOutcome::Error
                && !entry.reason.as_deref().unwrap_or("").contains(sentinel)
        }));
    }
}
