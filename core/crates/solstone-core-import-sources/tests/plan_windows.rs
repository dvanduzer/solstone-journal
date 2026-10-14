// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

mod support;

use chrono::{FixedOffset, Utc};
use serde_json::{Value, json};
use solstone_core_format::content::{RawPerceptFamily, produce_raw_percept_chunks};
use solstone_core_import::RegistrySource;
use solstone_core_import_sources::{chatgpt, claude, conversations, gemini};
use support::{TempTree, write_zip};

#[test]
fn conversation_entries_keep_roles_timestamps_and_five_minute_windows() {
    let tree = TempTree::new();
    let claude_plan = claude::plan(&support::claude_archive(&tree), &Utc).unwrap();
    let chatgpt_plan = chatgpt::plan(&support::chatgpt_archive(&tree), &Utc).unwrap();
    for plan in [&claude_plan, &chatgpt_plan] {
        assert_eq!(
            plan.date_range,
            ("20260311".to_owned(), "20260311".to_owned())
        );
        assert_eq!(plan.segments[0].day, "20260311");
        assert_eq!(plan.segments[0].segment_key, "120000_300");
        assert_eq!(plan.segments[0].entries[0].speaker, "Human");
        assert_eq!(plan.segments[0].entries[1].speaker, "Assistant");
        assert_eq!(plan.segments[0].entries[0].start, "00:00:00");
        assert_eq!(plan.segments[0].entries[1].start, "00:01:00");
    }
}

#[test]
fn imported_user_messages_reach_talent_sources_as_the_owner() {
    let tree = TempTree::new();
    let cases = [
        (
            RegistrySource::Chatgpt,
            chatgpt::plan(&support::chatgpt_archive(&tree), &Utc).unwrap(),
            vec![("You", "Hello"), ("Assistant", "Hi")],
        ),
        (
            RegistrySource::Claude,
            claude::plan(&support::claude_archive(&tree), &Utc).unwrap(),
            vec![("You", "Hello"), ("Assistant", "Hi")],
        ),
        (
            RegistrySource::Gemini,
            gemini::plan(&support::gemini_archive(&tree), &Utc).unwrap(),
            vec![
                ("You", "prompt only"),
                ("Assistant", "response only &\u{a0}!?"),
                ("You", "name fallback"),
                ("Assistant", "both"),
            ],
        ),
    ];

    for (source, plan, expected) in cases {
        let rendered = conversations::render(source, plan, "owner-test");
        assert_eq!(rendered.files.len(), 1, "{}", source.name());
        let file = &rendered.files[0];
        let rows = file
            .contents
            .lines()
            .skip(1)
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(rows.len(), expected.len(), "{}", source.name());
        let rel = format!(
            "{}/import.{}/{}/{}",
            file.day,
            source.name(),
            file.segment,
            file.name
        );
        // The transcript source loader uses the raw audio renderer for these rows.
        let presented = produce_raw_percept_chunks(RawPerceptFamily::Audio, &rel, &file.contents);
        assert_eq!(presented.error, None);
        assert_eq!(presented.chunks.len(), expected.len());
        for ((row, chunk), (speaker, text)) in rows.iter().zip(&presented.chunks).zip(&expected) {
            assert_eq!(row["speaker"], *speaker, "{}", source.name());
            assert_eq!(row["text"], *text);
            assert_eq!(row["source"], "import");
            assert!(chunk.content.contains(&format!("{speaker}: {text}")));
        }
        assert_eq!(rendered.items[0]["preview"], expected[0].1);
    }
}

#[test]
fn claude_unfamiliar_or_missing_senders_are_not_assigned_to_either_participant() {
    let tree = TempTree::new();
    let path = tree.path().join("unknown-senders.zip");
    let messages = [
        json!({"sender": "human", "text": "owner message"}),
        json!({"sender": "assistant", "text": "assistant message"}),
        json!({"sender": "tool", "text": "tool message"}),
        json!({"sender": "user", "text": "unrecognized native sender"}),
        json!({"text": "missing sender"}),
        json!({"sender": null, "text": "null sender"}),
        json!({"sender": 7, "text": "non-string sender"}),
    ];
    write_zip(
        &path,
        &[(
            "conversations.json".to_owned(),
            json!([{
                "created_at": "2026-03-11T12:00:00Z",
                "chat_messages": messages
            }])
            .to_string()
            .into_bytes(),
        )],
    );
    let plan = claude::plan(&path, &Utc).unwrap();
    let rendered = conversations::render(RegistrySource::Claude, plan, "unknown-test");
    let rows = rendered.files[0]
        .contents
        .lines()
        .skip(1)
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(rows.len(), messages.len());
    let expected = [
        "You",
        "Assistant",
        "Unknown",
        "Unknown",
        "Unknown",
        "Unknown",
        "Unknown",
    ];
    for ((row, message), speaker) in rows.iter().zip(&messages).zip(expected) {
        assert_eq!(row["speaker"], speaker);
        assert_eq!(row["text"], message["text"]);
    }
    assert_eq!(rendered.items[0]["preview"], "owner message");
}

#[test]
fn a_message_lands_on_the_day_and_minute_of_the_zone_it_is_planned_in() {
    let tree = TempTree::new();
    let path = tree.path().join("offset.zip");
    write_zip(
        &path,
        &[(
            "conversations.json".to_owned(),
            json!([{
                "chat_messages": [{
                    "sender": "human",
                    "text": "offset message",
                    "created_at": "2026-03-11T23:30:00-08:00"
                }]
            }])
            .to_string()
            .into_bytes(),
        )],
    );

    // Late evening in the owner's zone is the next morning in UTC. The journal is kept in
    // the owner's local time, so the owner's zone decides the day.
    let pacific = FixedOffset::west_opt(8 * 3600).unwrap();
    let local = claude::plan(&path, &pacific).unwrap();
    assert_eq!(
        local.date_range,
        ("20260311".to_owned(), "20260311".to_owned())
    );
    assert_eq!(local.segments[0].day, "20260311");
    assert_eq!(local.segments[0].segment_key, "233000_300");

    let utc = claude::plan(&path, &Utc).unwrap();
    assert_eq!(utc.segments[0].day, "20260312");
    assert_eq!(utc.segments[0].segment_key, "073000_300");
}
