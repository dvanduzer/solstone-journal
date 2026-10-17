// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Issuer for transcript segment links attached to search and fetch responses.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use serde_json::{Value, json};
use solstone_core_format::content::{ContentResolution, Family, classify, produce_chunks};
use solstone_core_indexer_query::AdmittedCategory;
use solstone_core_indexer_store::classification::{FacetDeclarationSet, classify_source};
use solstone_core_journal_io::is_day_key;
use solstone_core_journal_io::paths::{
    PathOrDay, Segment, iter_segments_skipping_unreadable_streams,
};
use solstone_core_transcripts::segment_clock;

use crate::dispatch::{
    DispatchPrincipal, boundary, codec, differing_capture_zone, segment_assignment_probe_path,
};
use crate::permissions::ConnectionReadSnapshot;
use crate::references::{ReferenceTarget, SegmentReference};

pub(crate) trait TranscriptPageCheck {
    fn check_first_page(
        &mut self,
        journal_root: &Path,
        segment: &Segment,
    ) -> Result<Option<u64>, ()>;
}

pub(crate) struct LiveTranscriptPages;

impl TranscriptPageCheck for LiveTranscriptPages {
    fn check_first_page(
        &mut self,
        journal_root: &Path,
        segment: &Segment,
    ) -> Result<Option<u64>, ()> {
        match solstone_core_transcripts::read_segment_transcript_page(journal_root, segment, None) {
            Ok(page) if !page.entries.is_empty() => Ok(Some(page.version.fingerprint())),
            Ok(_) => Ok(None),
            Err(_) => Err(()),
        }
    }
}

pub(crate) struct LinkSubject<'a> {
    pub path: &'a str,
    pub idx: i64,
    pub text: &'a str,
    pub segments: Vec<Value>,
    pub more_segments: bool,
}

pub(crate) fn issue_segment_links(
    journal_root: &Path,
    principal: DispatchPrincipal<'_>,
    snapshot: &ConnectionReadSnapshot,
    subjects: &mut [LinkSubject<'_>],
    per_result: usize,
    response_budget: usize,
    checker: &mut dyn TranscriptPageCheck,
) {
    if !snapshot.categories.contains(&AdmittedCategory::Transcripts) {
        return;
    }
    let Ok(declarations) = FacetDeclarationSet::from_journal(journal_root) else {
        return;
    };
    let transcript_boundary = boundary(snapshot, Some(AdmittedCategory::Transcripts), None);
    let owner_zone = solstone_core_journal_config::owner_zone(journal_root);

    let mut day_cache: BTreeMap<String, Vec<Segment>> = BTreeMap::new();
    let mut response_checks = 0;

    for subject in subjects.iter_mut() {
        let candidates = resolve_candidates(journal_root, subject, &mut day_cache);
        let mut subject_checks = 0;
        for (day, candidate) in candidates {
            let Ok(identity) = candidate.record_identity() else {
                continue;
            };
            let Some(clock) = segment_clock(identity.name) else {
                continue;
            };
            let probe_path = segment_assignment_probe_path(&day, identity.stream, identity.name);
            let classification = classify_source(
                journal_root,
                &probe_path,
                Some(identity.stream),
                &declarations,
            );
            // Scope comes first: a segment this connection may not read leaves
            // no trace, not even in `more_segments`.
            if !transcript_boundary.allows_classification(&classification) {
                continue;
            }
            if subject_checks == per_result || response_checks == response_budget {
                subject.more_segments = true;
                break;
            }
            subject_checks += 1;
            response_checks += 1;
            if let Ok(Some(version)) = checker.check_first_page(journal_root, &candidate) {
                let target = ReferenceTarget::Segment(SegmentReference {
                    day: day.clone(),
                    stream: identity.stream.to_string(),
                    segment: identity.name.to_string(),
                    version,
                    source_index: None,
                    byte_offset: None,
                    statement_ordinal: None,
                });
                if let Ok(token) = codec().mint(principal.connection, snapshot.generation, target) {
                    let mut link_val = json!({
                        "time": clock.label,
                        "reference": token,
                    });
                    if let Some(zone) = differing_capture_zone(
                        journal_root,
                        &owner_zone,
                        &day,
                        identity.name,
                        &candidate,
                    ) {
                        link_val["capture_zone"] = zone;
                    }
                    subject.segments.push(link_val);
                }
            }
        }
    }
}

fn resolve_candidates(
    journal_root: &Path,
    subject: &LinkSubject<'_>,
    day_cache: &mut BTreeMap<String, Vec<Segment>>,
) -> Vec<(String, Segment)> {
    let rel_path = subject
        .path
        .strip_prefix("chronicle/")
        .unwrap_or(subject.path);
    let parts: Vec<&str> = rel_path.split('/').collect();

    // 1. Segment file: day-rooted path with at least 3 components whose third contains segment_key
    if parts.len() >= 3 && is_day_key(parts[0]) {
        if let Some(filename) = parts.last() {
            if *filename == "imported.md"
                || filename.ends_with("_transcript.md")
                || *filename == "workout.json"
            {
                return Vec::new();
            }
            if classify(subject.path) == ContentResolution::Indexed(Family::AgentMemory) {
                return Vec::new();
            }
            if subject.path.contains("agent-memory-") && *filename == "note.txt" {
                return Vec::new();
            }
        }
        if solstone_core_format::segment::segment_key(parts[2]).is_some() {
            let day = parts[0];
            let stream = parts[1];
            let seg_name = parts[2];
            let day_segments = get_day_segments(journal_root, day, day_cache);
            let found = day_segments.iter().find(|s| {
                s.record_identity()
                    .map(|id| id.stream == stream && id.name == seg_name)
                    .unwrap_or(false)
            });
            if let Some(segment) = found {
                return vec![(day.to_string(), segment.clone())];
            }
            return Vec::new();
        }
    }

    // 2. Activity file: exactly facets/<facet>/activities/<YYYYMMDD>.jsonl (4 components)
    if parts.len() == 4 && parts[0] == "facets" && parts[2] == "activities" {
        let Some(day) = parts[3].strip_suffix(".jsonl") else {
            return Vec::new();
        };
        if !is_day_key(day) {
            return Vec::new();
        }
        let Ok(file_content) = fs::read_to_string(journal_root.join(subject.path)) else {
            return Vec::new();
        };
        let mut kept = Vec::new();
        for line in file_content.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(trimmed) {
                kept.push((trimmed, map));
            }
        }
        if subject.idx < 0 || subject.idx as usize >= kept.len() {
            return Vec::new();
        }
        let (trimmed_line, record) = &kept[subject.idx as usize];
        let produced = produce_chunks(Family::Activity, subject.path, trimmed_line);
        let Some(first_chunk) = produced.chunks.first() else {
            return Vec::new();
        };
        let (Some(rendered_time), Some(indexed_time)) = (
            extract_time_line(&first_chunk.content),
            extract_time_line(subject.text),
        ) else {
            return Vec::new();
        };
        if rendered_time != indexed_time {
            return Vec::new();
        }
        let (Some(rendered_title), Some(indexed_title)) = (
            extract_title(&first_chunk.content),
            extract_title(subject.text),
        ) else {
            return Vec::new();
        };
        if normalize_title(rendered_title) != normalize_title(indexed_title) {
            return Vec::new();
        }

        let Some(Value::Array(seg_values)) = record.get("segments") else {
            return Vec::new();
        };
        let keys: Vec<&str> = seg_values.iter().filter_map(Value::as_str).collect();
        if keys.is_empty() {
            return Vec::new();
        }

        let day_segments = get_day_segments(journal_root, day, day_cache);
        let stream_opt = record
            .get("stream")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());

        let mut candidates: Vec<(String, Segment)> = Vec::new();
        for key in keys {
            if let Some(stream_name) = stream_opt {
                for seg in day_segments {
                    if seg.stream().matches(stream_name) && seg.key() == key {
                        candidates.push((day.to_string(), seg.clone()));
                    }
                }
            } else {
                let named_matches: Vec<&Segment> = day_segments
                    .iter()
                    .filter(|s| s.stream().directory().is_some() && s.name().to_str() == Some(key))
                    .collect();
                if !named_matches.is_empty() {
                    for seg in named_matches {
                        candidates.push((day.to_string(), seg.clone()));
                    }
                } else if let Some(flat_seg) = day_segments
                    .iter()
                    .find(|s| s.stream().is_direct() && s.name().to_str() == Some(key))
                {
                    candidates.push((day.to_string(), flat_seg.clone()));
                }
            }
        }

        // Sort candidates by start seconds, record_identity stream (_default for flat), then directory name
        candidates.sort_by(|(_, a), (_, b)| {
            let clock_a = segment_clock(a.name().to_str().unwrap_or(""));
            let clock_b = segment_clock(b.name().to_str().unwrap_or(""));
            let start_a = clock_a.as_ref().map(|c| c.start_seconds).unwrap_or(0);
            let start_b = clock_b.as_ref().map(|c| c.start_seconds).unwrap_or(0);

            let id_a = a.record_identity().ok();
            let id_b = b.record_identity().ok();
            let stream_a = id_a.as_ref().map(|id| id.stream).unwrap_or("");
            let stream_b = id_b.as_ref().map(|id| id.stream).unwrap_or("");
            let name_a = a.name().to_str().unwrap_or("");
            let name_b = b.name().to_str().unwrap_or("");

            start_a
                .cmp(&start_b)
                .then_with(|| stream_a.cmp(stream_b))
                .then_with(|| name_a.cmp(name_b))
        });

        // Drop a later duplicate of the same stream and directory name
        let mut seen = BTreeSet::new();
        candidates.retain(|(_, seg)| {
            if let Ok(id) = seg.record_identity() {
                seen.insert((id.stream.to_string(), id.name.to_string()))
            } else {
                false
            }
        });

        return candidates;
    }

    Vec::new()
}

fn get_day_segments<'a>(
    journal_root: &Path,
    day: &str,
    cache: &'a mut BTreeMap<String, Vec<Segment>>,
) -> &'a [Segment] {
    cache
        .entry(day.to_string())
        .or_insert_with(|| {
            iter_segments_skipping_unreadable_streams(journal_root, PathOrDay::Day(day))
                .unwrap_or_default()
        })
        .as_slice()
}

fn extract_time_line(text: &str) -> Option<&str> {
    text.lines().find(|l| l.starts_with("- Time: "))
}

fn extract_title(text: &str) -> Option<&str> {
    text.lines().find(|l| l.starts_with("### "))
}

fn normalize_title(title_line: &str) -> String {
    let title = title_line.strip_prefix("### ").unwrap_or(title_line);
    title
        .split_ascii_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}
