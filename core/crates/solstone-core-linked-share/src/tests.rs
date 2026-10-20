// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Matching, scoping and attribution over small synthetic activities.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::path::Path;

use serde_json::{Value, json};

use crate::lines::message_lines;
use crate::matching::{ActivityMatch, Basis, Shape, match_sources, with_neighbours};
use crate::messages::conversation_threads;
use crate::names::PersonRef;
use crate::people::Owner;
use crate::source::{FrameMessage, MessagingFrame, SegmentSource, Utterance, Voice};
use crate::summary::summarize_segments;
use crate::talents::{ACTIVITY_TALENT, PRESENCE_TALENT, SEGMENT_TALENT, TalentCaller};

const SEGMENTS: [&str; 3] = ["150000_300", "150500_300", "151000_300"];

fn person() -> PersonRef {
    PersonRef::new("jane_doe", "Jane Doe", &[])
}

fn owner() -> Owner {
    Owner::new(Some("sam_rivera".to_owned()), "Sam Rivera", &[])
}

fn no_model() -> impl Fn(&str, &str) -> Result<Value, String> {
    |talent: &str, _: &str| panic!("unexpected talent call: {talent}")
}

fn message(sender: &str, from_me: Option<bool>, stamp: &str, text: &str) -> FrameMessage {
    FrameMessage {
        sender: sender.to_owned(),
        from_me,
        timestamp: (!stamp.is_empty()).then(|| stamp.to_owned()),
        text: text.to_owned(),
    }
}

fn frame(thread: &str, view: &str, messages: Vec<FrameMessage>) -> MessagingFrame {
    MessagingFrame {
        captured_at: None,
        app: "Signal".to_owned(),
        thread: thread.to_owned(),
        view: view.to_owned(),
        messages,
    }
}

fn segment(name: &str, frames: Vec<MessagingFrame>) -> SegmentSource {
    SegmentSource {
        segment: name.to_owned(),
        frames,
        ..SegmentSource::default()
    }
}

fn spoken(voice: Voice, speaker: i64, text: &str) -> Utterance {
    Utterance {
        speaker: Some(speaker),
        voice,
        text: text.to_owned(),
    }
}

fn jane_voice() -> Voice {
    Voice::Entity {
        id: "jane_doe".to_owned(),
        name: "Jane Doe".to_owned(),
    }
}

fn matched(sources: &[SegmentSource]) -> Option<ActivityMatch> {
    match_sources(sources, &person(), &owner(), &no_model())
}

// ---- message_thread ----

#[test]
fn a_live_exchange_in_the_persons_thread_qualifies() {
    let sources = vec![
        segment(SEGMENTS[0], vec![frame("Calendar", "unknown", vec![])]),
        segment(
            SEGMENTS[1],
            vec![frame(
                "Jane Doe",
                "conversation",
                vec![
                    message("Jane Doe", Some(false), "Tuesday", "Old news"),
                    message(
                        "Jane Doe",
                        Some(false),
                        "3:06 PM",
                        "Can we move the launch?",
                    ),
                    message("Me", Some(true), "Now", "Yes, to May"),
                ],
            )],
        ),
        segment(SEGMENTS[2], vec![]),
    ];
    let found = matched(&sources).expect("match");
    assert_eq!(found.basis, Basis::MessageThread);
    assert_eq!(found.shape, Shape::Messaging1to1);
    assert_eq!(found.segments, vec![SEGMENTS[1]]);
    assert_eq!(
        found.threads,
        vec![("Signal".to_owned(), "Jane Doe".to_owned())]
    );
}

#[test]
fn history_alone_does_not_qualify() {
    let history = vec![
        message("Jane Doe", Some(false), "Yesterday", "See you Friday"),
        message("Me", Some(true), "Mon", "Great"),
    ];
    let mut frames = vec![frame("Jane Doe", "conversation", history)];
    // Mostly other threads, so this is not a focused read either.
    for thread in ["Lee Chen", "Ops", "Family"] {
        frames.push(frame(
            thread,
            "conversation",
            vec![message("Lee Chen", None, "3:05 PM", "ping")],
        ));
    }
    assert_eq!(matched(&[segment(SEGMENTS[0], frames)]), None);
}

#[test]
fn triage_across_many_threads_does_not_qualify() {
    // The owner opens the person's 1:1 once while working through other threads.
    let mut frames = vec![frame(
        "Jane Doe",
        "conversation",
        vec![message("Jane Doe", None, "Thursday", "Any update?")],
    )];
    frames.push(frame(
        "Jane Doe",
        "conversation",
        vec![message("Jane Doe", None, "Thursday", "Any update?")],
    ));
    for thread in ["Lee Chen", "Ops", "Family", "Book club", "Lee Chen"] {
        frames.push(frame(
            thread,
            "conversation",
            vec![message("Me", Some(true), "Now", "on it")],
        ));
    }
    assert_eq!(matched(&[segment(SEGMENTS[0], frames)]), None);
}

#[test]
fn a_focused_read_of_a_one_to_one_qualifies_unanswered() {
    let read = vec![message("Jane Doe", None, "3m", "Here is the long update")];
    let sources = vec![
        segment(
            SEGMENTS[0],
            vec![frame("Jane Doe", "conversation", read.clone())],
        ),
        segment(
            SEGMENTS[1],
            vec![
                frame("Jane Doe", "conversation", read),
                frame(
                    "Ops",
                    "conversation",
                    vec![message("Lee Chen", None, "Now", "deploy done")],
                ),
            ],
        ),
    ];
    let found = matched(&sources).expect("focused read");
    assert_eq!(found.basis, Basis::MessageThread);
    assert_eq!(found.shape, Shape::Messaging1to1);
    assert_eq!(found.segments, vec![SEGMENTS[0], SEGMENTS[1]]);
}

#[test]
fn a_one_to_one_left_open_with_only_history_is_not_a_focused_read() {
    let old = vec![message(
        "Jane Doe",
        None,
        "Monday",
        "Here is the long update",
    )];
    let sources = vec![
        segment(
            SEGMENTS[0],
            vec![frame("Jane Doe", "conversation", old.clone())],
        ),
        segment(SEGMENTS[1], vec![frame("Jane Doe", "conversation", old)]),
    ];
    assert_eq!(matched(&sources), None);
}

#[test]
fn inbox_and_list_views_never_count() {
    let rows = vec![
        message("Jane Doe", None, "Now", "Can we move the launch?"),
        message("Me", Some(true), "Now", "Yes"),
    ];
    let sources = vec![segment(
        SEGMENTS[0],
        vec![
            frame("Jane Doe", "inbox", rows.clone()),
            frame("Jane Doe", "inbox", rows.clone()),
            frame("Jane Doe", "unknown", rows),
        ],
    )];
    assert_eq!(matched(&sources), None);
}

#[test]
fn absent_from_me_falls_back_to_sender_names() {
    // Captures carry no from_me; a group thread belongs to the person
    // through three of her messages.
    let sources = vec![segment(
        SEGMENTS[0],
        vec![frame(
            "Launch crew",
            "conversation",
            vec![
                message("Jane Doe", None, "3:00 PM", "Morning all"),
                message("Jane Doe", None, "3:01 PM", "Draft is up"),
                message("Jane Doe", None, "3:02 PM", "Comments welcome"),
                message("Sam Rivera", None, "3:04 PM", "Reading now"),
                message("Lee Chen", None, "3:05 PM", "Same"),
            ],
        )],
    )];
    let found = matched(&sources).expect("group exchange");
    assert_eq!(found.shape, Shape::MessagingGroup);
    // Without any owner-side message there is no exchange.
    let quiet = vec![segment(
        SEGMENTS[0],
        vec![frame(
            "Launch crew",
            "conversation",
            vec![
                message("Jane Doe", None, "3:00 PM", "Morning all"),
                message("Jane Doe", None, "3:01 PM", "Draft is up"),
                message("Jane Doe", None, "3:02 PM", "Comments welcome"),
                message("Lee Chen", None, "3:05 PM", "Same"),
            ],
        )],
    )];
    assert_eq!(matched(&quiet), None);
}

#[test]
fn an_ocr_misread_thread_title_still_belongs_to_the_person() {
    let sources = vec![segment(
        SEGMENTS[0],
        vec![frame(
            "Jnae Doe",
            "conversation",
            vec![
                message("Jnae Doe", Some(false), "Now", "Call?"),
                message("Me", Some(true), "Now", "Sure"),
            ],
        )],
    )];
    assert_eq!(
        matched(&sources).expect("match").shape,
        Shape::Messaging1to1
    );
}

// ---- voice ----

fn voiced(person_lines: usize, other_lines: usize) -> Vec<SegmentSource> {
    let mut sources = SEGMENTS
        .iter()
        .map(|name| segment(name, vec![]))
        .collect::<Vec<_>>();
    for index in 0..other_lines {
        sources[index % 3]
            .utterances
            .push(spoken(Voice::Unidentified, 1, "other talk"));
    }
    for _ in 0..person_lines {
        sources[1]
            .utterances
            .push(spoken(jane_voice(), 2, "a line"));
    }
    sources
}

#[test]
fn sustained_voice_qualifies_with_neighbouring_segments() {
    let found = matched(&voiced(12, 60)).expect("voice");
    assert_eq!(found.basis, Basis::Voice);
    assert_eq!(found.shape, Shape::Conversation);
    assert_eq!(found.segments, SEGMENTS.to_vec());
}

#[test]
fn sporadic_voice_labels_do_not_qualify() {
    assert_eq!(matched(&voiced(2, 38)), None);
    // Ten lines that are under a tenth of the activity are still sporadic.
    assert_eq!(matched(&voiced(10, 120)), None);
}

#[test]
fn neighbours_stay_inside_the_activity() {
    let sources = (0..6)
        .map(|index| segment(&format!("15{index}000_300"), vec![]))
        .collect::<Vec<_>>();
    assert_eq!(
        with_neighbours(&sources, &BTreeSet::from([0])),
        vec!["150000_300", "151000_300"]
    );
    assert_eq!(
        with_neighbours(&sources, &BTreeSet::from([3, 5])),
        vec!["152000_300", "153000_300", "154000_300", "155000_300"]
    );
}

// ---- spoken_with ----

#[test]
fn a_spoken_name_qualifies_only_when_the_model_says_with() {
    let mut sources = SEGMENTS
        .iter()
        .map(|name| segment(name, vec![]))
        .collect::<Vec<_>>();
    sources[0].utterances = vec![
        spoken(Voice::Owner, 1, "Jane said she would call later"),
        spoken(Voice::Unidentified, 2, "okay"),
    ];
    sources[2].utterances = vec![
        spoken(Voice::Owner, 1, "Jane, what do you think?"),
        spoken(Voice::Unidentified, 2, "I like it"),
    ];
    let asked = RefCell::new(Vec::new());
    let presence = |talent: &str, text: &str| {
        assert_eq!(talent, PRESENCE_TALENT);
        asked.borrow_mut().push(text.to_owned());
        let marked = text
            .lines()
            .find(|line| line.starts_with(">> "))
            .expect("marked line")
            .to_owned();
        Ok(json!({"role": if marked.contains("what do you think") { "with" } else { "about" }}))
    };
    let found = match_sources(&sources, &person(), &owner(), &presence).expect("spoken with");
    assert_eq!(found.basis, Basis::SpokenWith);
    assert_eq!(found.segments, vec![SEGMENTS[1], SEGMENTS[2]]);
    let asked = asked.into_inner();
    assert_eq!(asked.len(), 2);
    assert!(
        asked[1].contains("Name in question: Jane Doe"),
        "{}",
        asked[1]
    );
    assert!(
        asked[1].contains(">> Sam Rivera: Jane, what do you think?"),
        "{}",
        asked[1]
    );
    assert!(asked[1].contains("Speaker 2: I like it"), "{}", asked[1]);

    let about = |_: &str, _: &str| Ok(json!({"role": "about"}));
    assert_eq!(match_sources(&sources, &person(), &owner(), &about), None);
    let failing = |_: &str, _: &str| Err("model unavailable".to_owned());
    assert_eq!(match_sources(&sources, &person(), &owner(), &failing), None);
}

#[test]
fn at_most_eight_presence_questions_per_activity() {
    let mut source = segment(SEGMENTS[0], vec![]);
    source.utterances = (0..20)
        .map(|_| spoken(Voice::Owner, 1, "Jane mentioned it"))
        .collect();
    let calls = RefCell::new(0);
    let about = |_: &str, _: &str| {
        *calls.borrow_mut() += 1;
        Ok(json!({"role": "unclear"}))
    };
    assert_eq!(match_sources(&[source], &person(), &owner(), &about), None);
    assert_eq!(calls.into_inner(), 8);
}

// ---- scoping and attribution ----

fn group_activity() -> Vec<SegmentSource> {
    vec![
        segment(
            SEGMENTS[0],
            vec![
                frame(
                    "Jane Doe",
                    "conversation",
                    vec![
                        // The capture puts the person's name on the owner's bubble once.
                        message(
                            "Jane Doe",
                            Some(true),
                            "3:01 PM",
                            "Shall we move the launch?",
                        ),
                        message("Jane Doe", Some(false), "3:02 PM", "Yes, to May 4"),
                    ],
                ),
                frame(
                    "Lee Chen",
                    "conversation",
                    vec![message(
                        "Lee Chen",
                        Some(false),
                        "Now",
                        "Private: my visa fell through",
                    )],
                ),
            ],
        ),
        segment(
            SEGMENTS[1],
            vec![frame(
                "Jane Doe",
                "conversation",
                vec![
                    message("Me", Some(true), "3:01 PM", "shall we move the LAUNCH?"),
                    message("Me", Some(true), "3:01 PM", "Shall we move the launch?"),
                    message("Jane Doe", Some(false), "3:02 PM", "Yes, to May 4"),
                    message("Jane Doe", Some(false), "3:07 PM", "If legal agrees"),
                ],
            )],
        ),
    ]
}

#[test]
fn message_scope_keeps_only_the_matched_thread_deduplicated() {
    let sources = group_activity();
    let found = matched(&sources).expect("match");
    let (threads, _) = conversation_threads(&sources);
    let first = message_lines(&threads, &found.threads, SEGMENTS[0], &person(), &owner())
        .iter()
        .map(|line| line.render())
        .collect::<Vec<_>>();
    assert_eq!(
        first,
        vec![
            "[m1] Sam Rivera: Shall we move the launch? (3:01 PM)",
            "[m2] Jane Doe: Yes, to May 4 (3:02 PM)",
        ]
    );
    let second = message_lines(&threads, &found.threads, SEGMENTS[1], &person(), &owner())
        .iter()
        .map(|line| line.render())
        .collect::<Vec<_>>();
    assert_eq!(second, vec!["[m1] Jane Doe: If legal agrees (3:07 PM)"]);
}

#[test]
fn points_are_attributed_by_code_and_invalid_ids_dropped() {
    let mut sources = vec![segment(SEGMENTS[0], vec![])];
    sources[0].utterances = vec![
        spoken(Voice::Owner, 1, "Let's move the launch to May."),
        spoken(jane_voice(), 2, "May 4 works if legal agrees."),
        spoken(Voice::Unidentified, 3, "I can draft the note."),
    ];
    let found = ActivityMatch {
        partial_input: None,
        basis: Basis::Voice,
        shape: Shape::Conversation,
        segments: vec![SEGMENTS[0].to_owned()],
        threads: Vec::new(),
    };
    let input = RefCell::new(String::new());
    let talents = |talent: &str, text: &str| {
        assert_eq!(talent, SEGMENT_TALENT);
        *input.borrow_mut() = text.to_owned();
        Ok(json!({"points": [
            {"ids": ["u1"], "point": "Proposed moving the launch to May"},
            {"ids": ["u2", "u9"], "point": "accepted May 4 if legal agrees."},
            {"ids": ["u3"], "point": "offered to draft the note"},
            {"ids": ["u2", "u1"], "point": "agreed on May"},
            {"ids": ["u7"], "point": "invented something"},
        ]}))
    };
    let summaries = summarize_segments(&sources, &found, &person(), &owner(), &talents);
    let input = input.into_inner();
    assert!(input.contains("Shared with: Jane Doe"), "{input}");
    assert!(
        input.contains("[u2] Jane Doe: May 4 works if legal agrees."),
        "{input}"
    );
    assert!(
        input.contains("[u3] Speaker 3: I can draft the note."),
        "{input}"
    );
    let points = &summaries[0].points;
    let rendered = points.iter().map(|p| p.text.as_str()).collect::<Vec<_>>();
    assert_eq!(
        rendered,
        vec![
            "Sam Rivera proposed moving the launch to May.",
            "Jane Doe accepted May 4 if legal agrees.",
            "Another participant offered to draft the note.",
            "Sam Rivera and Jane Doe agreed on May.",
        ]
    );
    assert_eq!(points[1].ids, vec!["u2"]);
    assert_eq!(points[3].who, "Sam Rivera and Jane Doe");
    assert_eq!(
        summaries[0].summary.as_deref(),
        Some(rendered.join(" ").as_str())
    );
}

#[test]
fn a_failed_segment_call_makes_that_summary_unavailable() {
    let mut sources = vec![segment(SEGMENTS[0], vec![]), segment(SEGMENTS[1], vec![])];
    sources[0].utterances = vec![spoken(Voice::Owner, 1, "hello")];
    let found = ActivityMatch {
        partial_input: None,
        basis: Basis::Voice,
        shape: Shape::Conversation,
        segments: vec![SEGMENTS[0].to_owned(), SEGMENTS[1].to_owned()],
        threads: Vec::new(),
    };
    let invalid = |_: &str, _: &str| Ok(json!({"points": "not a list"}));
    let summaries = summarize_segments(&sources, &found, &person(), &owner(), &invalid);
    assert_eq!(summaries[0].summary, None);
    assert!(summaries[0].points.is_empty());
    // A segment with nothing in scope needs no call and is simply empty.
    assert_eq!(summaries[1].summary.as_deref(), Some(""));
}

// ---- journal fixtures through the verb ----

const DAY: &str = "20261013";

fn write(root: &Path, relative: &str, contents: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
    fs::write(path, contents).expect("write");
}

fn jsonl(rows: &[Value]) -> String {
    rows.iter().map(|row| format!("{row}\n")).collect()
}

fn analyzed() -> Value {
    json!({"_solstone_processing": {"state": "analyzed"}})
}

fn screen_row(thread: &str, messages: Value) -> Value {
    json!({"frame_id": 1, "timestamp": 1.0, "analysis": {"primary": "messaging"},
        "content": {"messaging": {"app": "Signal", "thread": thread, "view": "conversation", "messages": messages}}})
}

fn fixture_journal() -> tempfile::TempDir {
    let root = tempfile::tempdir().expect("journal");
    let path = root.path();
    write(
        path,
        "config/journal.json",
        &json!({"identity": {"name": "Sam Rivera", "preferred": "Sam"}}).to_string(),
    );
    write(
        path,
        "entities/sam_rivera/entity.json",
        &json!({"id":"sam_rivera","name":"Sam Rivera","type":"Person","is_principal":true})
            .to_string(),
    );
    write(
        path,
        "entities/jane_doe/entity.json",
        &json!({"id":"jane_doe","name":"Jane Doe","aka":["JD"],"type":"Person"}).to_string(),
    );
    write(path, "facets/work/facet.json", r#"{"title":"Work"}"#);
    write(
        path,
        &format!("facets/work/activities/{DAY}.jsonl"),
        &jsonl(&[
            json!({"id": "messaging_150000_300", "activity": "messaging", "title": "Secret launch title",
                "description": "Private description", "segments": [SEGMENTS[0]], "stream": "default"}),
            json!({"id": "meeting_151000_300", "activity": "meeting", "title": "Standup",
                "segments": [SEGMENTS[2]], "stream": "default"}),
            json!({"id": "coding_150500_300", "activity": "coding", "segments": [SEGMENTS[1]],
                "stream": "default"}),
        ]),
    );
    let seg = |name: &str| format!("chronicle/{DAY}/default/{name}");
    // A live exchange in the person's thread, with an unanalyzed and a raw
    // capture that must be ignored.
    write(
        path,
        &format!("{}/monitor_1_screen.jsonl", seg(SEGMENTS[0])),
        &jsonl(&[
            analyzed(),
            screen_row(
                "Jane Doe",
                json!([
                    {"sender": "Jane Doe", "from_me": false, "timestamp": "3:02 PM", "subject": null, "text": "Can we move the launch?"},
                    {"sender": "Me", "timestamp": "Now", "subject": null, "text": "Yes, May 4"}
                ]),
            ),
        ]),
    );
    write(
        path,
        &format!("{}/tmux_1_screen.jsonl", seg(SEGMENTS[0])),
        &jsonl(&[analyzed(), screen_row("Ops", json!([]))]),
    );
    // The coding activity only shows the thread in a capture not yet analyzed.
    write(
        path,
        &format!("{}/monitor_1_screen.jsonl", seg(SEGMENTS[1])),
        &jsonl(&[
            json!({"_solstone_processing": {"state": "failed"}}),
            screen_row(
                "Jane Doe",
                json!([
                    {"sender": "Jane Doe", "from_me": false, "timestamp": "Now", "subject": null, "text": "x"},
                    {"sender": "Me", "from_me": true, "timestamp": "Now", "subject": null, "text": "y"}
                ]),
            ),
        ]),
    );
    // A meeting with sustained, acoustically labelled voice.
    let mut rows = vec![analyzed()];
    let mut labels = Vec::new();
    for id in 0..30 {
        let (speaker, label) = if id % 2 == 0 {
            (
                1,
                json!({"sentence_id": id, "speaker": "sam_rivera", "method": "user_assigned"}),
            )
        } else {
            (
                2,
                json!({"sentence_id": id, "speaker": "jane_doe", "method": "acoustic"}),
            )
        };
        rows.push(json!({"start": "00:00:01", "speaker": speaker, "sentence_id": id, "text": format!("line {id}")}));
        labels.push(label);
    }
    write(
        path,
        &format!("{}/audio.jsonl", seg(SEGMENTS[2])),
        &jsonl(&rows),
    );
    write(
        path,
        &format!("{}/talents/speaker_labels.json", seg(SEGMENTS[2])),
        &json!({"labels": labels}).to_string(),
    );
    root
}

fn fake_talents() -> impl Fn(&str, &str) -> Result<Value, String> {
    |talent: &str, text: &str| match talent {
        SEGMENT_TALENT => {
            let first = text
                .lines()
                .find(|line| line.starts_with('['))
                .and_then(|line| line.get(1..3))
                .unwrap_or("u1")
                .to_owned();
            Ok(json!({"points": [{"ids": [first], "point": "raised the launch date"}]}))
        }
        ACTIVITY_TALENT => {
            assert!(!text.contains("Secret launch title"), "{text}");
            Ok(json!({"summary": "Sam raised the launch date."}))
        }
        other => panic!("unexpected talent {other}"),
    }
}

fn run(root: &Path, values: &[&str], talents: &dyn TalentCaller) -> crate::CliRun {
    let args = values.iter().map(OsString::from).collect::<Vec<_>>();
    crate::run_cli(&args, root, talents)
}

#[test]
fn the_verb_lists_matching_activities_from_journal_files() {
    let root = fixture_journal();
    let before = fs::read_dir(root.path()).expect("dir").count();
    let listed = run(
        root.path(),
        &["match", "--person", "Jane Doe", "--day", DAY],
        &fake_talents(),
    );
    assert_eq!(listed.exit_code, 0, "{}", listed.stderr);
    let lines = listed
        .stdout
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("json line"))
        .collect::<Vec<_>>();
    assert_eq!(lines.len(), 2, "{}", listed.stdout);
    assert_eq!(lines[0]["activity_id"], "messaging_150000_300");
    assert_eq!(lines[0]["basis"], "message_thread");
    assert_eq!(lines[0]["shape"], "messaging_1to1");
    assert_eq!(lines[0]["threads"], json!([["Signal", "Jane Doe"]]));
    assert_eq!(lines[1]["activity_id"], "meeting_151000_300");
    assert_eq!(lines[1]["basis"], "voice");
    assert_eq!(lines[1]["segments"], json!([SEGMENTS[2]]));
    assert_eq!(fs::read_dir(root.path()).expect("dir").count(), before);
}

#[test]
fn clipped_shipped_summary_talents_keep_the_package_available_and_read_only() {
    use crate::talents::RuntimeTalents;
    use solstone_core_generate::{GenerateRequest, GenerateResponse, GeneratedResponse};
    use solstone_core_talent_runtime::prepare::RuntimePaths;
    let root = fixture_journal();
    let config_path = root.path().join("config/journal.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
    config["providers"] = json!({"active":{"provider":"google","model":"test-model"}});
    fs::write(config_path, config.to_string()).unwrap();
    let config_before = fs::read(root.path().join("config/journal.json")).unwrap();
    for clipped_talent in [SEGMENT_TALENT, ACTIVITY_TALENT] {
        let generate = |request: &GenerateRequest| {
            let reply = match request.context.as_str() {
                SEGMENT_TALENT => {
                    json!({"points":[{"ids":["m1"],"point":"raised the launch date"}]})
                }
                ACTIVITY_TALENT => json!({"summary":"Jane raised the launch date."}),
                other => panic!("unexpected {other}"),
            };
            Ok(GenerateResponse::Generated(Box::new(GeneratedResponse {
                id: None,
                text: reply.to_string(),
                model: "test-model".into(),
                finish_reason: "stop".into(),
                usage: json!({}),
                thinking: None,
                schema_validation: Some(json!({"valid":true})),
                input_budget: Some(
                    json!({"clipped":request.context==clipped_talent,"dropped_entries":2,"dropped_chars":50}),
                ),
                request_budget: None,
                inference: None,
                hints_applied: Vec::new(),
            })))
        };
        let paths = RuntimePaths {
            talent_root: Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../payload/solstone/talent"),
            apps_root: root.path().join("absent-apps"),
            templates_dir: root.path().join("absent-templates"),
        };
        let talents = RuntimeTalents::new(root.path(), paths, &generate);
        let result = run(
            root.path(),
            &[
                "package",
                "--person",
                "jane_doe",
                "--day",
                DAY,
                "--facet",
                "work",
                "--activity",
                "messaging_150000_300",
            ],
            &talents,
        );
        assert_eq!(result.exit_code, 0, "{}", result.stderr);
        assert!(result.stderr.contains("Left out:"));
        let package: Value = serde_json::from_str(&result.stdout).unwrap();
        assert_eq!(package["available"], true, "{package}");
        assert_eq!(package["activity_summary"], "Jane raised the launch date.");
        assert_eq!(package["partial_input"]["dropped_entries"], 2);
        assert_eq!(
            package["segments"][0].get("partial_input").is_some(),
            clipped_talent == SEGMENT_TALENT
        );
    }
    assert_eq!(
        fs::read(root.path().join("config/journal.json")).unwrap(),
        config_before
    );
    assert!(!root.path().join("talents").exists());
    assert!(
        !root
            .path()
            .join(format!("chronicle/{DAY}/talents"))
            .exists()
    );
}

#[test]
fn clipped_presence_marks_matches_and_warns_even_when_nothing_matches() {
    let root = fixture_journal();
    let audio = format!("chronicle/{DAY}/default/{}/audio.jsonl", SEGMENTS[1]);
    write(
        root.path(),
        &audio,
        &jsonl(&[
            analyzed(),
            json!({"start":"00:00:01","text":"Jane, can you check?","sentence_id":0}),
        ]),
    );
    for role in ["with", "about"] {
        let talents = |talent: &str, _: &str| {
            assert_eq!(talent, PRESENCE_TALENT);
            Ok(json!({"role":role,"partial_input":{"dropped_entries":1}}))
        };
        let result = run(
            root.path(),
            &["match", "--person", "jane_doe", "--day", DAY],
            &talents,
        );
        assert_eq!(result.exit_code, 0);
        assert!(result.stderr.contains("Left out:"));
        let rows: Vec<Value> = result
            .stdout
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let found = rows
            .iter()
            .find(|row| row["activity_id"] == "coding_150500_300");
        if role == "with" {
            assert_eq!(found.unwrap()["partial_input"]["dropped_entries"], 1);
        } else {
            assert!(found.is_none());
        }
    }
}

#[test]
fn the_verb_packages_a_match_without_the_journals_own_words() {
    let root = fixture_journal();
    let packaged = run(
        root.path(),
        &[
            "package",
            "--person",
            "jane_doe",
            "--day",
            DAY,
            "--facet",
            "work",
            "--activity",
            "messaging_150000_300",
        ],
        &fake_talents(),
    );
    assert_eq!(packaged.exit_code, 0, "{}", packaged.stderr);
    assert!(!packaged.stdout.contains("Secret launch title"));
    assert!(!packaged.stdout.contains("Private description"));
    let package = serde_json::from_str::<Value>(&packaged.stdout).expect("package");
    assert_eq!(package["sender"], "Sam");
    assert_eq!(package["shared_with"], "Jane Doe");
    assert_eq!(package["activity_type"], "messaging");
    assert_eq!(package["available"], true);
    assert_eq!(package["activity_summary"], "Sam raised the launch date.");
    assert_eq!(
        package["segments"][0]["points"][0],
        json!({"who": "Jane Doe", "text": "Jane Doe raised the launch date.", "ids": ["m1"]})
    );

    let unmatched = run(
        root.path(),
        &[
            "package",
            "--person",
            "jane_doe",
            "--day",
            DAY,
            "--facet",
            "work",
            "--activity",
            "coding_150500_300",
        ],
        &fake_talents(),
    );
    assert_eq!(unmatched.exit_code, 1);
    assert!(
        unmatched.stderr.contains("does not match Jane Doe"),
        "{}",
        unmatched.stderr
    );

    let failing = |_: &str, _: &str| Err::<Value, String>("model unavailable".to_owned());
    let unavailable = run(
        root.path(),
        &[
            "package",
            "--person",
            "jane_doe",
            "--day",
            DAY,
            "--facet",
            "work",
            "--activity",
            "meeting_151000_300",
        ],
        &failing,
    );
    assert_eq!(unavailable.exit_code, 0, "{}", unavailable.stderr);
    let package = serde_json::from_str::<Value>(&unavailable.stdout).expect("package");
    assert_eq!(package["available"], false);
    assert_eq!(package["activity_summary"], Value::Null);
    assert_eq!(package["segments"][0]["summary"], Value::Null);
}

#[test]
fn an_unknown_person_or_activity_is_a_clear_error() {
    let root = fixture_journal();
    let unknown = run(
        root.path(),
        &["match", "--person", "Nobody", "--day", DAY],
        &no_model(),
    );
    assert_eq!(unknown.exit_code, 1);
    assert!(unknown.stderr.contains("Nobody"), "{}", unknown.stderr);
    let missing = run(
        root.path(),
        &[
            "package",
            "--person",
            "jane_doe",
            "--day",
            DAY,
            "--facet",
            "work",
            "--activity",
            "nope",
        ],
        &no_model(),
    );
    assert_eq!(missing.exit_code, 1);
    assert!(
        missing.stderr.contains("no activity 'nope'"),
        "{}",
        missing.stderr
    );
}

/// A contact titled by first name and last initial, with minute-relative
/// stamps and many aliases on both sides, as a real Signal capture shows it.
#[test]
fn initialed_thread_with_relative_stamps_is_a_live_exchange() {
    let dana = PersonRef::new(
        "dana_van_holt",
        "Dana Van Holt",
        &["dvh".to_owned(), "dvh van holt".to_owned()],
    );
    let sam = Owner::new(
        Some("sam_rivera".to_owned()),
        "Sam Rivera",
        &[
            "Sam".to_owned(),
            "Sam (S)".to_owned(),
            "Samuel Rivera".to_owned(),
        ],
    );
    let sources = vec![
        segment(
            "154649_215",
            vec![frame(
                "dana v",
                "conversation",
                vec![
                    message("dana v", None, "24m", "are you around"),
                    message("me", None, "22m", "yes, what's up"),
                    message("dana v", None, "17m", "can we talk about hours"),
                ],
            )],
        ),
        segment(
            "155644_300",
            vec![frame(
                "dana v",
                "conversation",
                vec![
                    message("me", None, "6m", "sure, after lunch"),
                    message("dana v", None, "3m", "great"),
                ],
            )],
        ),
    ];
    let matched = match_sources(&sources, &dana, &sam, &no_model()).expect("live exchange");
    assert_eq!(matched.basis, Basis::MessageThread);
}
