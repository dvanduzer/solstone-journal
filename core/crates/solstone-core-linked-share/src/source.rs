// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Read-only views of an activity's analyzed audio and messaging frames.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde_json::{Map, Value};
use solstone_core_processing_record::vocab::STATE_ANALYZED;
use solstone_core_transcripts::VoiceNames;

/// Who a transcript line's voice label names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Voice {
    Owner,
    Entity { id: String, name: String },
    Unidentified,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Utterance {
    pub(crate) speaker: Option<i64>,
    pub(crate) voice: Voice,
    pub(crate) text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FrameMessage {
    pub(crate) sender: String,
    /// Absent in captures made before the field existed: unknown, not false.
    pub(crate) from_me: Option<bool>,
    pub(crate) timestamp: Option<String>,
    pub(crate) text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MessagingFrame {
    pub(crate) app: String,
    pub(crate) thread: String,
    pub(crate) view: String,
    pub(crate) messages: Vec<FrameMessage>,
    /// When the frame was captured, in seconds of the day, if known. A
    /// relative stamp ("24m") is anchored to it.
    pub(crate) captured_at: Option<u32>,
}

impl MessagingFrame {
    pub(crate) fn is_conversation(&self) -> bool {
        self.view == "conversation"
    }

    pub(crate) fn key(&self) -> (String, String) {
        (self.app.clone(), self.thread.clone())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SegmentSource {
    pub(crate) segment: String,
    pub(crate) utterances: Vec<Utterance>,
    pub(crate) frames: Vec<MessagingFrame>,
}

/// Every segment of one activity, in the record's order. A segment with no
/// directory or no analyzed sources contributes an empty entry.
pub(crate) fn load_activity_sources(
    journal: &Path,
    day: &str,
    segments: &[String],
    stream: Option<&str>,
    voices: Option<&VoiceNames>,
) -> Vec<SegmentSource> {
    segments
        .iter()
        .map(|segment| {
            match solstone_core_system_health::find_segment_dir(journal, day, segment, stream) {
                Some(dir) => load_segment(&dir, segment, voices),
                None => SegmentSource {
                    segment: segment.clone(),
                    ..SegmentSource::default()
                },
            }
        })
        .collect()
}

pub(crate) fn load_segment(
    dir: &Path,
    segment: &str,
    voices: Option<&VoiceNames>,
) -> SegmentSource {
    let mut files = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect::<Vec<_>>();
    files.sort();
    let labels = voices.and_then(|voices| segment_labels(dir, voices));
    let mut source = SegmentSource {
        segment: segment.to_owned(),
        ..SegmentSource::default()
    };
    for file in &files {
        let Some(stem) = file.strip_suffix(".jsonl") else {
            continue;
        };
        if stem.ends_with("audio") {
            let Some(rows) = analyzed_rows(&dir.join(file)) else {
                continue;
            };
            let names = labels
                .as_ref()
                .filter(|(source, _)| source == stem)
                .map(|(_, names)| names);
            source
                .utterances
                .extend(rows.iter().filter_map(|row| utterance(row, names)));
        } else if (stem == "screen" || stem.ends_with("_screen")) && !stem.starts_with("tmux_") {
            let Some(rows) = analyzed_rows(&dir.join(file)) else {
                continue;
            };
            let start = segment_start_seconds(segment);
            source
                .frames
                .extend(rows.iter().filter_map(|row| messaging_frame(row, start)));
        }
    }
    source
}

/// The rows after an analyzed processing header; `None` for any other file.
fn analyzed_rows(path: &Path) -> Option<Vec<Map<String, Value>>> {
    let text = fs::read_to_string(path).ok()?;
    let mut lines = text.lines().filter(|line| !line.trim().is_empty());
    let header = serde_json::from_str::<Value>(lines.next()?).ok()?;
    if header
        .pointer("/_solstone_processing/state")
        .and_then(Value::as_str)
        != Some(STATE_ANALYZED)
    {
        return None;
    }
    Some(
        lines
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter_map(|value| match value {
                Value::Object(row) => Some(row),
                _ => None,
            })
            .collect(),
    )
}

/// Sentence id to voice for the segment's labelled audio source.
fn segment_labels(dir: &Path, voices: &VoiceNames) -> Option<(String, BTreeMap<i64, Voice>)> {
    let text = fs::read_to_string(dir.join("talents").join("speaker_labels.json")).ok()?;
    let payload = serde_json::from_str::<Value>(&text).ok()?;
    let labels = payload
        .get("labels")?
        .as_array()?
        .iter()
        .filter_map(Value::as_object)
        .filter_map(|row| {
            let sentence = row.get("sentence_id")?.as_i64()?;
            let name = voices.name_for(row)?;
            let speaker = row.get("speaker")?.as_str()?;
            let voice = if speaker == voices.principal {
                Voice::Owner
            } else {
                Voice::Entity {
                    id: speaker.to_owned(),
                    name,
                }
            };
            Some((sentence, voice))
        })
        .collect();
    Some((
        solstone_core_transcripts::labelled_audio_source(dir)?,
        labels,
    ))
}

fn utterance(row: &Map<String, Value>, labels: Option<&BTreeMap<i64, Voice>>) -> Option<Utterance> {
    let text = ["corrected", "text"]
        .iter()
        .filter_map(|key| row.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .find(|text| !text.is_empty())?
        .to_owned();
    let voice = row
        .get("sentence_id")
        .and_then(Value::as_i64)
        .and_then(|id| labels?.get(&id).cloned())
        .unwrap_or(Voice::Unidentified);
    Some(Utterance {
        speaker: row.get("speaker").and_then(Value::as_i64),
        voice,
        text,
    })
}

fn segment_start_seconds(segment: &str) -> Option<u32> {
    let (times, _) = solstone_core_format::segment::segment_start_and_end_seconds(segment)?;
    Some(u32::from(times.hour) * 3_600 + u32::from(times.minute) * 60 + u32::from(times.second))
}

fn messaging_frame(row: &Map<String, Value>, segment_start: Option<u32>) -> Option<MessagingFrame> {
    let messaging = row.get("content")?.get("messaging")?.as_object()?;
    let text_of = |value: Option<&Value>| {
        value
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default()
            .to_owned()
    };
    let messages = messaging
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_object)
        .map(|message| FrameMessage {
            sender: text_of(message.get("sender")),
            from_me: message.get("from_me").and_then(Value::as_bool),
            timestamp: message
                .get("timestamp")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|stamp| !stamp.is_empty())
                .map(str::to_owned),
            text: text_of(message.get("text")),
        })
        .filter(|message| !message.text.is_empty())
        .collect();
    let offset = row
        .get("timestamp")
        .and_then(Value::as_f64)
        .filter(|offset| offset.is_finite() && *offset >= 0.0);
    Some(MessagingFrame {
        app: text_of(messaging.get("app")),
        thread: text_of(messaging.get("thread")),
        view: text_of(messaging.get("view")),
        messages,
        captured_at: segment_start
            .zip(offset)
            .map(|(start, offset)| start.saturating_add(offset as u32)),
    })
}
