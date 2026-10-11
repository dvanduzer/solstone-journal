// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Bounded transcript-only reads for one chronicle segment.

//! This module intentionally does not share the broad day-clustering surface:
//! it has no percept, browser, or talent selection flags.

use std::collections::hash_map::DefaultHasher;
use std::fs::{self, File};
use std::hash::{Hash, Hasher};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde_json::Value;
use solstone_core_format::content::{
    ContentResolution, Family, RawPerceptFamily, classify, produce_chunks,
    produce_raw_percept_chunks,
};
use solstone_core_journal_io::paths::{Segment, segment_origin};

/// Largest source-byte window parsed for one transcript page.
pub const MAX_TRANSCRIPT_PAGE_BYTES: usize = 64 * 1024;
/// Largest number of transcript entries returned for one transcript page.
pub const MAX_TRANSCRIPT_PAGE_ITEMS: usize = 100;

/// Parsed wall-clock times and formatted display range for a chronicle segment.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SegmentClock {
    pub label: String,
    pub start_seconds: u32,
    pub exclusive_end_seconds: u32,
}

pub fn segment_clock(directory_name: &str) -> Option<SegmentClock> {
    let key = solstone_core_format::segment::segment_key(directory_name)?;
    let (time_part, length_part) = key.split_once('_')?;
    if time_part.len() != 6 || !time_part.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let hour = time_part[0..2].parse::<u32>().ok()?;
    let minute = time_part[2..4].parse::<u32>().ok()?;
    let second = time_part[4..6].parse::<u32>().ok()?;
    if hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let duration = length_part.parse::<u32>().ok()?;
    let start_seconds = hour * 3600 + minute * 60 + second;
    let exclusive_end_seconds = start_seconds.saturating_add(duration).min(86400);
    let display_end = start_seconds.saturating_add(duration).min(86399);
    let end_h = display_end / 3600;
    let end_m = (display_end % 3600) / 60;
    let label = format!("{hour:02}:{minute:02}-{end_h:02}:{end_m:02}");
    Some(SegmentClock {
        label,
        start_seconds,
        exclusive_end_seconds,
    })
}

/// Opaque-to-callers source version for one segment's approved transcript files.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SegmentTranscriptVersion(u64);

impl SegmentTranscriptVersion {
    /// Stable source-version value for binding an opaque continuation.
    #[must_use]
    pub fn fingerprint(&self) -> u64 {
        self.0
    }

    /// Reconstruct a cursor version previously obtained from this API.
    #[must_use]
    pub fn from_fingerprint(value: u64) -> Self {
        Self(value)
    }
}

/// Private continuation point within the approved transcript file list.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SegmentTranscriptCursor {
    pub source_index: usize,
    pub byte_offset: u64,
    pub version: SegmentTranscriptVersion,
}

/// One projected transcript entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SegmentTranscriptEntry {
    pub text: String,
}

/// One bounded page from a single segment.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SegmentTranscriptPage {
    pub entries: Vec<SegmentTranscriptEntry>,
    pub next: Option<SegmentTranscriptCursor>,
    pub version: SegmentTranscriptVersion,
    pub untimed: bool,
}

/// Failure while reading a bounded transcript page.
#[derive(Debug)]
pub enum SegmentTranscriptReadError {
    Changed,
    InvalidCursor,
    Io,
    RecordTooLarge,
}

fn peek_untimed(path: &Path) -> bool {
    let Ok(file) = File::open(path) else {
        return false;
    };
    use std::io::BufRead;
    let mut reader = std::io::BufReader::new(file.take(MAX_TRANSCRIPT_PAGE_BYTES as u64));
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return false;
    }
    if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(line.trim()) {
        map.get("untimed") == Some(&Value::Bool(true))
    } else {
        false
    }
}

/// Read one bounded page of approved transcript text from exactly one segment.
///
/// The caller owns authorization. This function only projects transcript input;
/// it never walks a day, reads percept/browser/talent sources, or applies scope.
pub fn read_segment_transcript_page(
    segment: &Segment,
    cursor: Option<&SegmentTranscriptCursor>,
) -> Result<SegmentTranscriptPage, SegmentTranscriptReadError> {
    let files = transcript_files(segment.path())?;
    let version = source_version(&files)?;
    let untimed = files
        .iter()
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
        .any(|path| peek_untimed(path));
    let (mut source_index, mut byte_offset) = match cursor {
        Some(cursor) if cursor.version != version => {
            return Err(SegmentTranscriptReadError::Changed);
        }
        Some(cursor) if cursor.source_index > files.len() => {
            return Err(SegmentTranscriptReadError::InvalidCursor);
        }
        Some(cursor) => (cursor.source_index, cursor.byte_offset),
        None => (0, 0),
    };

    // Read the sidecar through the same bound. Labels are deliberately local to
    // this projection; no speaker-label storage API is widened for connection reads.
    let _labels = bounded_labels(segment.path());

    let segment_origin = segment_origin(segment.path());
    let mut entries = Vec::new();
    while source_index < files.len() && entries.len() < MAX_TRANSCRIPT_PAGE_ITEMS {
        let path = &files[source_index];
        let (window, advanced, exhausted) = read_window(path, byte_offset)?;
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        let mut consumed = 0;
        let mut stopped_in_window = false;
        for (line, line_end) in complete_lines(&window)? {
            let text = project_line(&segment_origin, name, line);
            if !text.trim().is_empty() {
                entries.push(SegmentTranscriptEntry { text });
            }
            consumed = line_end;
            if entries.len() == MAX_TRANSCRIPT_PAGE_ITEMS {
                stopped_in_window = consumed < advanced;
                break;
            }
        }
        byte_offset = byte_offset.saturating_add(
            (if stopped_in_window {
                consumed
            } else {
                advanced
            }) as u64,
        );
        if exhausted && !stopped_in_window {
            source_index += 1;
            byte_offset = 0;
        }
        if advanced == 0 && !exhausted {
            return Err(SegmentTranscriptReadError::RecordTooLarge);
        }
    }

    let next = (source_index < files.len()).then(|| SegmentTranscriptCursor {
        source_index,
        byte_offset,
        version: version.clone(),
    });
    Ok(SegmentTranscriptPage {
        entries,
        next,
        version,
        untimed,
    })
}

fn transcript_files(segment: &Path) -> Result<Vec<PathBuf>, SegmentTranscriptReadError> {
    let mut files = fs::read_dir(segment)
        .map_err(|_| SegmentTranscriptReadError::Io)?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            let name = path.file_name()?.to_str()?;
            (path.is_file() && is_transcript_name(name)).then_some(path)
        })
        .collect::<Vec<_>>();
    files.sort();
    Ok(files)
}

fn is_transcript_name(name: &str) -> bool {
    name == "audio.jsonl"
        || name.ends_with("_audio.jsonl")
        || name.ends_with("_transcript.jsonl")
        || name == "imported.md"
        || name.ends_with("_transcript.md")
        || name == "workout.json"
}

fn source_version(
    files: &[PathBuf],
) -> Result<SegmentTranscriptVersion, SegmentTranscriptReadError> {
    let mut hasher = DefaultHasher::new();
    for path in files {
        let metadata = fs::metadata(path).map_err(|_| SegmentTranscriptReadError::Io)?;
        path.file_name().hash(&mut hasher);
        metadata.len().hash(&mut hasher);
        let modified = metadata
            .modified()
            .map_err(|_| SegmentTranscriptReadError::Io)?
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| SegmentTranscriptReadError::Io)?;
        modified.as_secs().hash(&mut hasher);
        modified.subsec_nanos().hash(&mut hasher);
    }
    Ok(SegmentTranscriptVersion(hasher.finish()))
}

fn bounded_labels(segment: &Path) -> Option<Vec<u8>> {
    let path = segment.join("talents/speaker_labels.json");
    let metadata = fs::metadata(&path).ok()?;
    if metadata.len() > MAX_TRANSCRIPT_PAGE_BYTES as u64 {
        return None;
    }
    fs::read(path).ok()
}

fn read_window(
    path: &Path,
    offset: u64,
) -> Result<(Vec<u8>, usize, bool), SegmentTranscriptReadError> {
    let mut file = File::open(path).map_err(|_| SegmentTranscriptReadError::Io)?;
    let length = file
        .metadata()
        .map_err(|_| SegmentTranscriptReadError::Io)?
        .len();
    if offset > length {
        return Err(SegmentTranscriptReadError::Changed);
    }
    file.seek(SeekFrom::Start(offset))
        .map_err(|_| SegmentTranscriptReadError::Io)?;
    let mut bytes = Vec::with_capacity(MAX_TRANSCRIPT_PAGE_BYTES);
    file.take(MAX_TRANSCRIPT_PAGE_BYTES as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| SegmentTranscriptReadError::Io)?;
    let exhausted = offset + bytes.len() as u64 == length;
    let advanced = if exhausted {
        bytes.len()
    } else {
        bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map(|index| index + 1)
            .unwrap_or(0)
    };
    bytes.truncate(advanced);
    Ok((bytes, advanced, exhausted))
}

fn complete_lines(bytes: &[u8]) -> Result<Vec<(&str, usize)>, SegmentTranscriptReadError> {
    let text = std::str::from_utf8(bytes).map_err(|_| SegmentTranscriptReadError::Io)?;
    let mut end = 0;
    let lines = text
        .split_inclusive('\n')
        .map(|raw| {
            end += raw.len();
            let line = raw.strip_suffix('\n').unwrap_or(raw);
            let line = line.strip_suffix('\r').unwrap_or(line);
            (line, end)
        })
        .collect::<Vec<_>>();
    Ok(lines)
}

fn project_line(segment_origin: &str, name: &str, line: &str) -> String {
    let rel = format!("{segment_origin}/{name}");
    match classify(&rel) {
        ContentResolution::Unindexed(RawPerceptFamily::Audio) => {
            produce_raw_percept_chunks(RawPerceptFamily::Audio, &rel, line)
                .chunks
                .into_iter()
                .map(|chunk| chunk.content)
                .collect::<Vec<_>>()
                .join("\n")
        }
        ContentResolution::Indexed(Family::AiChat) => {
            let produced = produce_chunks(Family::AiChat, &rel, line);
            let start = serde_json::from_str::<Value>(line)
                .ok()
                .and_then(|v| v.get("start").and_then(Value::as_str).map(str::to_owned));
            produced
                .chunks
                .into_iter()
                .map(|chunk| {
                    if let Some(ref start) = start {
                        format!("[{start}] {}", chunk.content)
                    } else {
                        chunk.content
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
        _ => {
            if name == "workout.json" {
                produce_chunks(Family::Workout, name, line)
                    .chunks
                    .into_iter()
                    .map(|chunk| chunk.content)
                    .collect::<Vec<_>>()
                    .join("\n")
            } else if name == "audio.jsonl"
                || name.ends_with("_audio.jsonl")
                || name.ends_with("_transcript.jsonl")
            {
                produce_raw_percept_chunks(RawPerceptFamily::Audio, &rel, line)
                    .chunks
                    .into_iter()
                    .map(|chunk| chunk.content)
                    .collect::<Vec<_>>()
                    .join("\n")
            } else {
                line.to_owned()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{
        MAX_TRANSCRIPT_PAGE_BYTES, MAX_TRANSCRIPT_PAGE_ITEMS, read_segment_transcript_page,
        segment_clock,
    };
    use solstone_core_format::content::{
        Family, RawPerceptFamily, produce_chunks, produce_raw_percept_chunks,
    };
    use solstone_core_journal_io::paths::{PathOrDay, iter_segments};

    #[test]
    fn routine_tests_for_segment_clock() {
        let clock = segment_clock("000000_86400").unwrap();
        assert_eq!(clock.label, "00:00-23:59");
        assert_eq!(clock.start_seconds, 0);
        assert_eq!(clock.exclusive_end_seconds, 86400);

        let clock = segment_clock("110000_60").unwrap();
        assert_eq!(clock.label, "11:00-11:01");
        assert_eq!(clock.start_seconds, 39600);
        assert_eq!(clock.exclusive_end_seconds, 39660);

        let clock = segment_clock("110000_30").unwrap();
        assert_eq!(clock.label, "11:00-11:00");
        assert_eq!(clock.start_seconds, 39600);
        assert_eq!(clock.exclusive_end_seconds, 39630);

        let clock = segment_clock("233000_3600").unwrap();
        assert_eq!(clock.label, "23:30-23:59");
        assert_eq!(clock.start_seconds, 84600);
        assert_eq!(clock.exclusive_end_seconds, 86400);

        let clock = segment_clock("x-120000_300").unwrap();
        assert_eq!(clock.label, "12:00-12:05");
        assert_eq!(clock.start_seconds, 43200);
        assert_eq!(clock.exclusive_end_seconds, 43500);
    }

    #[test]
    fn pages_one_large_transcript_without_reading_talent_or_percept_files() {
        let root = tempfile::tempdir().unwrap();
        let segment = root.path().join("chronicle/20260914/default/090000_300");
        fs::create_dir_all(segment.join("talents")).unwrap();
        fs::write(
            segment.join("meeting_transcript.md"),
            "approved transcript line\n".repeat(MAX_TRANSCRIPT_PAGE_BYTES / 8),
        )
        .unwrap();
        fs::write(segment.join("screen.jsonl"), "secret percept").unwrap();
        fs::write(segment.join("talents/brief.md"), "secret talent").unwrap();
        let segment = iter_segments(root.path(), PathOrDay::Day("20260914"))
            .unwrap()
            .pop()
            .unwrap();

        let first = read_segment_transcript_page(&segment, None).unwrap();
        assert!(!first.entries.is_empty());
        assert!(
            first
                .entries
                .iter()
                .all(|entry| entry.text == "approved transcript line")
        );
        assert!(first.next.is_some());
        assert!(!first.untimed);
    }

    #[test]
    fn transcript_page_projects_recorded_speech_without_other_sources() {
        let root = tempfile::tempdir().unwrap();
        let segment = root.path().join("chronicle/20260914/default/090000_300");
        fs::create_dir_all(segment.join("talents")).unwrap();
        fs::write(
            segment.join("meeting_transcript.md"),
            "approved transcript line\n",
        )
        .unwrap();
        fs::write(
            segment.join("audio.jsonl"),
            "{\"recording\":\"metadata only\"}\n{\"start\":\"00:00:01\",\"text\":\"what was said\",\"raw\":\"secret audio metadata\"}\n",
        )
        .unwrap();
        fs::write(segment.join("screen.jsonl"), "screen percept\n").unwrap();
        fs::write(segment.join("browser_history.jsonl"), "browser percept\n").unwrap();
        fs::write(segment.join("talents/brief.md"), "secret talent\n").unwrap();
        fs::write(segment.join("talents/sense.json"), "secret sense\n").unwrap();
        let segment = iter_segments(root.path(), PathOrDay::Day("20260914"))
            .unwrap()
            .pop()
            .unwrap();

        let page = read_segment_transcript_page(&segment, None).unwrap();
        assert_eq!(
            page.entries,
            vec![
                super::SegmentTranscriptEntry {
                    text: "[00:00:01] what was said".to_owned(),
                },
                super::SegmentTranscriptEntry {
                    text: "approved transcript line".to_owned(),
                },
            ]
        );
        assert!(!page.untimed);
    }

    #[test]
    fn audio_and_aichat_projections_and_renderer_guard() {
        let root = tempfile::tempdir().unwrap();
        // 1. import.audio
        let seg_audio = root
            .path()
            .join("chronicle/20260914/import.audio/090000_300");
        fs::create_dir_all(&seg_audio).unwrap();
        let audio_bytes =
            "{\"recording\":\"metadata\"}\n{\"start\":\"00:00:02\",\"text\":\"spoken words\"}\n";
        fs::write(seg_audio.join("imported_audio.jsonl"), audio_bytes).unwrap();

        // 2. legacy import.chatgpt
        let seg_chatgpt = root
            .path()
            .join("chronicle/20260914/import.chatgpt/090000_300");
        fs::create_dir_all(&seg_chatgpt).unwrap();
        let chat_bytes = "{\"imported\":{}}\n{\"start\":\"00:00:00\",\"speaker\":\"Ana Lima\",\"text\":\"Hello all.\"}\n";
        fs::write(seg_chatgpt.join("imported_audio.jsonl"), chat_bytes).unwrap();

        // 3. source_audio.jsonl in default
        let seg_custom = root.path().join("chronicle/20260914/default/090500_300");
        fs::create_dir_all(&seg_custom).unwrap();
        fs::write(seg_custom.join("mic_audio.jsonl"), audio_bytes).unwrap();

        // 4. timed import.text
        let seg_text = root
            .path()
            .join("chronicle/20260914/import.text/091000_300");
        fs::create_dir_all(&seg_text).unwrap();
        fs::write(seg_text.join("conversation_transcript.jsonl"), chat_bytes).unwrap();

        let segments = iter_segments(root.path(), PathOrDay::Day("20260914")).unwrap();

        // Verify import.audio page
        let seg = segments.iter().find(|s| s.path() == seg_audio).unwrap();
        let page = read_segment_transcript_page(seg, None).unwrap();
        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.entries[0].text, "[00:00:02] spoken words");

        // Verify custom _audio.jsonl page
        let seg = segments.iter().find(|s| s.path() == seg_custom).unwrap();
        let page = read_segment_transcript_page(seg, None).unwrap();
        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.entries[0].text, "[00:00:02] spoken words");

        // Verify import.chatgpt page
        let seg = segments.iter().find(|s| s.path() == seg_chatgpt).unwrap();
        let page = read_segment_transcript_page(seg, None).unwrap();
        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.entries[0].text, "[00:00:00] **Ana Lima:** Hello all.");

        // Verify import.text page
        let seg = segments.iter().find(|s| s.path() == seg_text).unwrap();
        let page = read_segment_transcript_page(seg, None).unwrap();
        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.entries[0].text, "[00:00:00] **Ana Lima:** Hello all.");

        // Renderer guard: call produce_raw_percept_chunks and produce_chunks directly
        let raw = produce_raw_percept_chunks(
            RawPerceptFamily::Audio,
            "20260914/import.audio/090000_300/imported_audio.jsonl",
            audio_bytes,
        );
        assert_eq!(raw.chunks.len(), 1);
        assert_eq!(raw.chunks[0].content, "[00:00:02] spoken words");

        let chat = produce_chunks(
            Family::AiChat,
            "20260914/import.chatgpt/090000_300/imported_audio.jsonl",
            chat_bytes,
        );
        assert_eq!(chat.chunks.len(), 1);
        assert_eq!(chat.chunks[0].content, "**Ana Lima:** Hello all.");
        assert_eq!(chat.header.as_deref(), Some("# ChatGPT conversation"));
    }

    #[test]
    fn a_flat_legacy_source_transcript_reads_projected_not_raw() {
        let root = tempfile::TempDir::new_in("/var/tmp").unwrap();
        let segment = root.path().join("chronicle/20250301/093000_300");
        fs::create_dir_all(&segment).unwrap();
        fs::write(
            segment.join("session_transcript.jsonl"),
            "{\"raw\": \"session.flac\"}\n{\"start\": \"00:00:04\", \"speaker\": 2, \"text\": \"send the deck on friday\"}\n",
        )
        .unwrap();
        let segment = iter_segments(root.path(), PathOrDay::Day("20250301"))
            .unwrap()
            .pop()
            .unwrap();
        let page = read_segment_transcript_page(&segment, None).unwrap();
        assert_eq!(page.entries.len(), 1, "{:?}", page.entries);
        assert!(page.entries[0].text.contains("send the deck on friday"));
        assert!(
            !page.entries[0].text.contains('{'),
            "{}",
            page.entries[0].text
        );
    }

    #[test]
    fn untimed_flag_persists_across_continuation_pages() {
        let root = tempfile::TempDir::new_in("/var/tmp").unwrap();
        let segment = root
            .path()
            .join("chronicle/20260914/import.text/090000_300");
        fs::create_dir_all(&segment).unwrap();

        let mut lines = String::from("{\"untimed\": true, \"imported\": {}}\n");
        for i in 1..=MAX_TRANSCRIPT_PAGE_ITEMS + 10 {
            lines.push_str(&format!(
                "{{\"start\": \"00:00:00\", \"speaker\": \"User\", \"text\": \"turn {i}\"}}\n"
            ));
        }
        fs::write(segment.join("conversation_transcript.jsonl"), lines).unwrap();

        let segment = iter_segments(root.path(), PathOrDay::Day("20260914"))
            .unwrap()
            .pop()
            .unwrap();

        let first = read_segment_transcript_page(&segment, None).unwrap();
        assert!(first.untimed);
        assert_eq!(first.entries.len(), MAX_TRANSCRIPT_PAGE_ITEMS);
        assert_eq!(first.entries[0].text, "[00:00:00] **User:** turn 1");
        assert!(first.next.is_some());

        let second = read_segment_transcript_page(&segment, first.next.as_ref()).unwrap();
        assert!(second.untimed);
        assert_eq!(second.entries.len(), 10);
        assert_eq!(second.entries[0].text, "[00:00:00] **User:** turn 101");
        assert!(second.next.is_none());
    }

    #[test]
    fn a_strava_workout_reads_as_what_strava_recorded_on_its_first_piece_only() {
        let root = tempfile::tempdir().unwrap();
        let workout = serde_json::json!({
            "schema": "solstone.import.strava.tile.v1",
            "activity_id": 9,
            "tile": {"index": 0, "count": 2},
            "workout": {"name": "Morning Run", "type": "Run", "distance_m": 10200.0}
        });
        for (key, index) in [("070200_300", 0), ("070700_300", 1)] {
            let segment = root
                .path()
                .join("chronicle/20261004/import.strava")
                .join(key);
            fs::create_dir_all(&segment).unwrap();
            let mut tile = workout.clone();
            tile["tile"]["index"] = serde_json::json!(index);
            fs::write(segment.join("workout.json"), format!("{tile}\n")).unwrap();
        }
        let segments = iter_segments(root.path(), PathOrDay::Day("20261004")).unwrap();
        let pages = segments
            .iter()
            .map(|segment| read_segment_transcript_page(segment, None).unwrap().entries)
            .collect::<Vec<_>>();
        assert_eq!(
            pages[0],
            vec![super::SegmentTranscriptEntry {
                text: "## Strava workout: Morning Run\nRun\n10.2 km".to_owned(),
            }]
        );
        assert!(pages[1].is_empty());
    }

    #[test]
    fn item_limited_page_resumes_at_the_first_unreturned_line() {
        let root = tempfile::TempDir::new_in("/var/tmp").unwrap();
        let segment = root.path().join("chronicle/20260914/default/090000_300");
        fs::create_dir_all(&segment).unwrap();
        let lines = (1..=MAX_TRANSCRIPT_PAGE_ITEMS + 5)
            .map(|line| format!("line {line}\n"))
            .collect::<String>();
        fs::write(segment.join("meeting_transcript.md"), lines).unwrap();
        let segment = iter_segments(root.path(), PathOrDay::Day("20260914"))
            .unwrap()
            .pop()
            .unwrap();

        let first = read_segment_transcript_page(&segment, None).unwrap();
        assert_eq!(first.entries.len(), MAX_TRANSCRIPT_PAGE_ITEMS);
        assert_eq!(first.entries[0].text, "line 1");
        assert_eq!(
            first.entries[MAX_TRANSCRIPT_PAGE_ITEMS - 1].text,
            "line 100"
        );

        let second = read_segment_transcript_page(&segment, first.next.as_ref()).unwrap();
        assert_eq!(
            second
                .entries
                .iter()
                .map(|entry| entry.text.as_str())
                .collect::<Vec<_>>(),
            ["line 101", "line 102", "line 103", "line 104", "line 105"]
        );
        assert!(second.next.is_none());
    }
}
