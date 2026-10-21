// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Schedule post-hook.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{Datelike, NaiveDate, SecondsFormat, Utc};
use serde_json::{Map, Value, json};
use solstone_core_system::activity_state::normalize_activity_description;

use crate::contract::{CommitPlan, ParsedOutput, PrePostState};
use crate::writers::WriteIntent;
use crate::{PreparedTalent, StageError, detected_resolution_entities, stage_error};

const ANTICIPATION_FUZZY_THRESHOLD: f64 = 0.85;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SkipReason {
    MissingField,
    BadTargetDate,
    UnknownFacet,
    BadParticipation,
    BadTime,
    NonObject,
    ResolvedChoiceEntityAbsent,
    ResolvedChoiceEntityBlocked,
}

impl SkipReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::MissingField => "missing field",
            Self::BadTargetDate => "bad target date",
            Self::UnknownFacet => "unknown facet",
            Self::BadParticipation => "bad participation",
            Self::BadTime => "bad time",
            Self::NonObject => "non-object event",
            Self::ResolvedChoiceEntityAbsent => "resolved-choice entity absent",
            Self::ResolvedChoiceEntityBlocked => "resolved-choice entity blocked",
        }
    }
}

#[derive(Debug)]
enum PrepareEventError {
    Skip { reason: SkipReason, detail: String },
    Abort(String),
}

#[cfg(test)]
thread_local! {
    static TEST_SCHEDULE_SKIP_WARNINGS: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn warn_skipped_event(day: &str, reason: SkipReason) {
    let message = format!("schedule event skipped for day {day}: {}", reason.as_str());
    log::warn!("{message}");
    #[cfg(test)]
    TEST_SCHEDULE_SKIP_WARNINGS.with(|warnings| warnings.borrow_mut().push(message));
}

pub fn parse(
    output: &str,
    _prepared: &PreparedTalent,
    _state: &PrePostState,
) -> Result<ParsedOutput, StageError> {
    Ok(ParsedOutput::Text(output.to_owned()))
}

pub fn commit(
    parsed: ParsedOutput,
    prepared: &PreparedTalent,
    _state: &PrePostState,
) -> Result<CommitPlan, StageError> {
    let ParsedOutput::Text(output) = parsed else {
        return Err(stage_error(
            "commit",
            "schedule",
            prepared,
            "expected text output",
        ));
    };
    let Some(day) = prepared.config.get("day").and_then(Value::as_str) else {
        return Ok(CommitPlan::NoOutput);
    };
    Ok(CommitPlan::Write(WriteIntent::Schedule {
        output,
        day: day.to_owned(),
    }))
}

pub fn apply_result(journal: &std::path::Path, output: &str, day: &str) -> Result<(), String> {
    apply_result_cancellable(
        journal,
        output,
        day,
        &solstone_core_generate::Cancellation::never(),
    )
}

pub(crate) fn apply_result_cancellable(
    journal: &std::path::Path,
    output: &str,
    day: &str,
    cancellation: &solstone_core_generate::Cancellation,
) -> Result<(), String> {
    let _guard =
        solstone_core_facets::hold_facet_trust_lock(journal).map_err(|error| error.to_string())?;
    crate::writers::check_cancellation(cancellation)?;
    // Preserve solstone/talent/schedule.py:45-71: malformed output is ignored.
    let Ok(mut events) = serde_json::from_str::<Value>(output.trim()) else {
        return Ok(());
    };
    let partial = events
        .get("partial_input")
        .filter(|v| v.is_object())
        .cloned();
    if events.is_object() {
        events = events
            .as_object_mut()
            .and_then(|object| object.remove("events"))
            .unwrap_or_else(|| Value::Array(Vec::new()));
    }
    let Some(events) = events.as_array() else {
        return Ok(());
    };
    let Ok(current_day) = NaiveDate::parse_from_str(day, "%Y%m%d") else {
        return Ok(());
    };
    let known_facets = solstone_core_facets::list_declared_facet_names(journal)
        .map_err(|error| error.to_string())?
        .into_iter()
        .collect::<BTreeSet<_>>();
    let mut entity_cache = BTreeMap::new();
    for raw in events {
        crate::writers::check_cancellation(cancellation)?;
        let Some(raw) = raw.as_object() else { continue };
        let mut raw = raw.clone();
        crate::partial_input::stamp(&mut raw, partial.as_ref());
        let _ = apply_event(
            journal,
            &raw,
            day,
            current_day,
            &known_facets,
            &mut entity_cache,
            cancellation,
        );
        // Python skips one invalid event and continues with the following event.
    }
    Ok(())
}

fn apply_event(
    journal: &std::path::Path,
    raw: &Map<String, Value>,
    day: &str,
    current_day: NaiveDate,
    known_facets: &BTreeSet<String>,
    entity_cache: &mut BTreeMap<
        (String, String),
        Vec<solstone_core_entity::EntityResolutionEntity>,
    >,
    cancellation: &solstone_core_generate::Cancellation,
) -> Result<(), String> {
    let (facet, target_day_key, mut record) = prepare_event(
        journal,
        raw,
        day,
        current_day,
        known_facets,
        entity_cache,
        false,
    )
    .map_err(|error| match error {
        PrepareEventError::Skip { detail, .. } => detail,
        PrepareEventError::Abort(detail) => detail,
    })?;
    crate::partial_input::stamp(&mut record, raw.get("partial_input"));
    let new_id = require_text(&record, "id")?;
    let cancelled = record
        .get("cancelled")
        .is_some_and(solstone_core_facets::activity_value_truthy);
    let timestamp = Utc::now().to_rfc3339_opts(SecondsFormat::Micros, true);
    let (action, superseded) = dedup_anticipation(journal, &facet, &target_day_key, &record)?;
    match action {
        AnticipationAction::Skip => return Ok(()),
        AnticipationAction::Create => {
            if !matches!(
                solstone_core_facets::append_activity_record_checked(
                    journal,
                    &facet,
                    &target_day_key,
                    record,
                    || crate::writers::check_cancellation(cancellation),
                )
                .map_err(|error| error.to_string())?,
                solstone_core_facets::AppendOutcome::Written(_)
            ) {
                return Ok(());
            }
        }
        AnticipationAction::Update(patch) => {
            solstone_core_facets::update_activity_record_checked(
                journal,
                &facet,
                &target_day_key,
                &new_id,
                &patch,
                "schedule",
                if cancelled {
                    "updated by schedule (cancelled on calendar)"
                } else {
                    "updated by schedule"
                },
                &timestamp,
                || crate::writers::check_cancellation(cancellation),
            )
            .map_err(|error| error.to_string())?;
        }
    }
    for superseded_id in superseded {
        solstone_core_facets::set_activity_hidden_checked(
            journal,
            &facet,
            &target_day_key,
            &superseded_id,
            true,
            "schedule",
            Some(&format!("superseded by {new_id}")),
            &timestamp,
            || crate::writers::check_cancellation(cancellation),
        )
        .map_err(|error| error.to_string())?;
    }
    Ok(())
}

pub fn prepare_publication(
    journal: &std::path::Path,
    output: &str,
    day: &str,
) -> Result<Vec<solstone_core_facets::PreparedAnticipationBatch>, String> {
    let value: Value =
        serde_json::from_str(output).map_err(|e| format!("invalid schedule output: {e}"))?;
    let events = if let Some(events) = value.as_array() {
        events
    } else {
        value
            .get("events")
            .and_then(Value::as_array)
            .ok_or("schedule output must contain events")?
    };
    let current_day = NaiveDate::parse_from_str(day, "%Y%m%d").map_err(|e| e.to_string())?;
    let known = solstone_core_facets::list_declared_facet_names(journal)
        .map_err(|e| e.to_string())?
        .into_iter()
        .collect();
    let mut cache = BTreeMap::new();
    type DayAnticipations = Vec<(Map<String, Value>, Vec<String>)>;
    let mut groups: BTreeMap<(String, String), DayAnticipations> = BTreeMap::new();
    for raw in events {
        let Some(raw) = raw.as_object() else {
            warn_skipped_event(day, SkipReason::NonObject);
            continue;
        };
        let (facet, target, mut record) =
            match prepare_event(journal, raw, day, current_day, &known, &mut cache, true) {
                Ok(prepared) => prepared,
                Err(PrepareEventError::Skip { reason, .. }) => {
                    warn_skipped_event(day, reason);
                    continue;
                }
                Err(PrepareEventError::Abort(detail)) => return Err(detail),
            };
        let existing = solstone_core_facets::load_activity_records(journal, &facet, &target, true)
            .map_err(|e| e.to_string())?;
        crate::partial_input::stamp(
            &mut record,
            value.get("partial_input").filter(|v| v.is_object()),
        );
        if !record.contains_key("partial_input")
            && existing.iter().any(|old| {
                old.get("id") == record.get("id")
                    && old.get("partial_input").is_some_and(Value::is_object)
            })
        {
            record.insert("partial_input".into(), Value::Null);
        }
        let exact_match = existing.iter().any(|old| old.get("id") == record.get("id"));
        let superseded = existing
            .iter()
            .filter(|_| !exact_match)
            .filter(|old| {
                old.get("source").and_then(Value::as_str) == Some("anticipated")
                    && old.get("id") != record.get("id")
                    && sequence_ratio(
                        &normalized_title(old.get("title")),
                        &normalized_title(record.get("title")),
                    ) >= ANTICIPATION_FUZZY_THRESHOLD
            })
            .filter_map(|old| old.get("id").and_then(Value::as_str).map(str::to_owned))
            .collect();
        groups
            .entry((facet, target))
            .or_default()
            .push((record, superseded));
    }
    let timestamp = Utc::now().to_rfc3339_opts(SecondsFormat::Micros, true);
    groups
        .into_iter()
        .map(|((facet, day), mut records)| {
            let incoming_ids: BTreeSet<String> = records
                .iter()
                .filter_map(|(row, _)| row.get("id").and_then(Value::as_str).map(str::to_owned))
                .collect();
            for (_, superseded) in &mut records {
                superseded.retain(|id| !incoming_ids.contains(id));
            }
            solstone_core_facets::prepare_anticipation_batch(
                journal, &facet, &day, &records, &timestamp,
            )
        })
        .collect()
}

fn prepare_event(
    journal: &std::path::Path,
    raw: &Map<String, Value>,
    day: &str,
    current_day: NaiveDate,
    known_facets: &BTreeSet<String>,
    entity_cache: &mut BTreeMap<
        (String, String),
        Vec<solstone_core_entity::EntityResolutionEntity>,
    >,
    read_only: bool,
) -> Result<(String, String, Map<String, Value>), PrepareEventError> {
    let activity = require_text(raw, "activity").map_err(|detail| PrepareEventError::Skip {
        reason: SkipReason::MissingField,
        detail,
    })?;
    let target_date =
        require_text(raw, "target_date").map_err(|detail| PrepareEventError::Skip {
            reason: SkipReason::MissingField,
            detail,
        })?;
    let title = require_text(raw, "title").map_err(|detail| PrepareEventError::Skip {
        reason: SkipReason::MissingField,
        detail,
    })?;
    let description =
        require_text(raw, "description").map_err(|detail| PrepareEventError::Skip {
            reason: SkipReason::MissingField,
            detail,
        })?;
    let facet = require_text(raw, "facet").map_err(|detail| PrepareEventError::Skip {
        reason: SkipReason::MissingField,
        detail,
    })?;
    if !known_facets.contains(&facet) {
        return Err(PrepareEventError::Skip {
            reason: SkipReason::UnknownFacet,
            detail: format!("validation: unknown facet {facet:?}"),
        });
    }
    let target_day = NaiveDate::parse_from_str(&target_date, "%Y-%m-%d").map_err(|error| {
        PrepareEventError::Skip {
            reason: SkipReason::BadTargetDate,
            detail: error.to_string(),
        }
    })?;
    if target_day <= current_day {
        return Err(PrepareEventError::Skip {
            reason: SkipReason::BadTargetDate,
            detail: format!("target_date must be after context day ({target_date} <= {day})"),
        });
    }
    let start = optional_time(raw, "start").map_err(|detail| PrepareEventError::Skip {
        reason: SkipReason::BadTime,
        detail,
    })?;
    let end = optional_time(raw, "end").map_err(|detail| PrepareEventError::Skip {
        reason: SkipReason::BadTime,
        detail,
    })?;
    let cancelled = raw
        .get("cancelled")
        .is_some_and(solstone_core_facets::activity_value_truthy);
    let details = python_string(raw.get("details").unwrap_or(&Value::Null));
    let participation_confidence = raw
        .get("participation_confidence")
        .cloned()
        .unwrap_or(Value::Null);
    let Some(participation) = raw.get("participation").and_then(Value::as_array) else {
        return Err(PrepareEventError::Skip {
            reason: SkipReason::BadParticipation,
            detail: "participation must be a list".to_owned(),
        });
    };
    let new_id =
        make_anticipation_id(&activity, start.as_deref(), &target_date).map_err(|detail| {
            PrepareEventError::Skip {
                reason: SkipReason::BadTargetDate,
                detail,
            }
        })?;
    let target_day_key = target_day.format("%Y%m%d").to_string();
    let entities = match entity_cache.entry((facet.clone(), target_day_key.clone())) {
        std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
        std::collections::btree_map::Entry::Vacant(entry) => {
            let loaded = detected_resolution_entities(journal, &facet, &target_day_key)
                .map_err(PrepareEventError::Abort)?;
            entry.insert(loaded)
        }
    };
    let mut resolved = Vec::new();
    let mut active = Vec::new();
    let mut seen_active = BTreeSet::new();
    for entry in participation.iter().filter_map(Value::as_object) {
        let mut entry = entry.clone();
        let resolution = match solstone_core_entity::record_entity_resolution(
            journal,
            entry
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default(),
            entities,
            json!({"kind":"facet","facet":facet}),
            json!({"lane":"talent.schedule","facet":facet,"day":target_day_key,"record_id":new_id,"field":"participation.name"}),
            90.0,
            read_only,
        ) {
            Ok(resolution) => resolution,
            Err(
                error @ solstone_core_entity::EntityResolutionError::ResolvedChoiceEntityAbsent {
                    ..
                },
            ) => {
                return Err(PrepareEventError::Skip {
                    reason: SkipReason::ResolvedChoiceEntityAbsent,
                    detail: error.to_string(),
                });
            }
            Err(
                error @ solstone_core_entity::EntityResolutionError::ResolvedChoiceEntityBlocked {
                    ..
                },
            ) => {
                return Err(PrepareEventError::Skip {
                    reason: SkipReason::ResolvedChoiceEntityBlocked,
                    detail: error.to_string(),
                });
            }
            Err(solstone_core_entity::EntityResolutionError::Read(error)) => {
                return Err(PrepareEventError::Abort(error.to_string()));
            }
            Err(solstone_core_entity::EntityResolutionError::TrustLock(error)) => {
                // Unreachable on prepare_publication because read_only: true; reachable on apply_event (read_only: false)
                return Err(PrepareEventError::Abort(error.to_string()));
            }
            Err(solstone_core_entity::EntityResolutionError::Write(error)) => {
                // Unreachable on prepare_publication because read_only: true; reachable on apply_event (read_only: false)
                return Err(PrepareEventError::Abort(error.to_string()));
            }
        };
        let entity_id = resolved_id(&resolution, entities);
        entry.insert("entity_id".to_owned(), entity_id.clone());
        if entry.get("role").and_then(Value::as_str) == Some("attendee")
            && let Some(id) = entity_id.as_str()
            && seen_active.insert(id.to_owned())
        {
            active.push(Value::String(id.to_owned()));
        }
        resolved.push(Value::Object(entry));
    }
    let timestamp = Utc::now().to_rfc3339_opts(SecondsFormat::Micros, true);
    let fields = [
        "activity",
        "target_date",
        "start",
        "end",
        "title",
        "description",
        "details",
        "source",
        "active_entities",
        "participation",
        "participation_confidence",
        "cancelled",
        "hidden",
    ];
    let record = solstone_core_facets::append_edit(
        Map::from_iter([
            ("id".to_owned(), Value::String(new_id.clone())),
            ("activity".to_owned(), Value::String(activity)),
            ("target_date".to_owned(), Value::String(target_date)),
            ("start".to_owned(), start.map_or(Value::Null, Value::String)),
            ("end".to_owned(), end.map_or(Value::Null, Value::String)),
            (
                "title".to_owned(),
                Value::String(normalize_activity_description(&title)),
            ),
            (
                "description".to_owned(),
                Value::String(normalize_activity_description(&description)),
            ),
            ("details".to_owned(), Value::String(details)),
            ("facet".to_owned(), Value::String(facet.clone())),
            ("source".to_owned(), Value::String("anticipated".to_owned())),
            ("active_entities".to_owned(), Value::Array(active)),
            ("participation".to_owned(), Value::Array(resolved)),
            (
                "participation_confidence".to_owned(),
                participation_confidence,
            ),
            ("cancelled".to_owned(), Value::Bool(cancelled)),
            ("hidden".to_owned(), Value::Bool(cancelled)),
        ]),
        "schedule",
        fields.into_iter().map(str::to_owned).collect(),
        if cancelled {
            "created by schedule (cancelled on calendar)"
        } else {
            "created by schedule"
        },
        &timestamp,
    );
    Ok((facet, target_day_key, record))
}

fn require_text(item: &Map<String, Value>, key: &str) -> Result<String, String> {
    item.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| format!("missing required field '{key}'"))
}

fn optional_time(item: &Map<String, Value>, key: &str) -> Result<Option<String>, String> {
    let Some(value) = item.get(key) else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let Some(value) = value.as_str() else {
        return Err(format!("invalid {key:?}: expected HH:MM:SS or null"));
    };
    Ok(Some(
        (value.len() == 8
            && value.as_bytes()[2] == b':'
            && value.as_bytes()[5] == b':'
            && value
                .bytes()
                .enumerate()
                .all(|(index, byte)| matches!(index, 2 | 5) || byte.is_ascii_digit()))
        .then_some(value.to_owned())
        .ok_or_else(|| format!("invalid {key:?}: expected HH:MM:SS or null"))?,
    ))
}

// This is a writer's key derived from name-and-position. Preserve the inherited
// reference behavior from solstone/think/activities.py:1350; do not treat it as a stable identity rule.
fn make_anticipation_id(
    activity_type: &str,
    start: Option<&str>,
    target_date: &str,
) -> Result<String, String> {
    let activity = activity_type.trim();
    if activity.is_empty() {
        return Err("activity_type must be non-empty".to_owned());
    }
    let target = NaiveDate::parse_from_str(target_date, "%Y-%m-%d")
        .map_err(|_| "target_date must match YYYY-MM-DD".to_owned())?;
    let start = match start {
        Some(start) => optional_time(
            &Map::from_iter([("start".to_owned(), Value::String(start.to_owned()))]),
            "start",
        )?
        .unwrap()
        .replace(':', ""),
        None => "000000".to_owned(),
    };
    Ok(format!(
        "anticipated_{activity}_{start}_{:02}{:02}",
        target.month(),
        target.day()
    ))
}

#[derive(Debug, PartialEq)]
enum AnticipationAction {
    Skip,
    Create,
    Update(Map<String, Value>),
}

fn dedup_anticipation(
    journal: &std::path::Path,
    facet: &str,
    target_day: &str,
    record: &Map<String, Value>,
) -> Result<(AnticipationAction, Vec<String>), String> {
    let new_id = require_text(record, "id")?;
    let new_title = normalized_title(record.get("title"));
    let mut superseded = Vec::new();
    let existing_records =
        solstone_core_facets::load_activity_records(journal, facet, target_day, true)
            .map_err(|error| error.to_string())?;
    for existing in existing_records {
        if existing.get("source").and_then(Value::as_str) != Some("anticipated") {
            continue;
        }
        let existing_id = python_string(existing.get("id").unwrap_or(&Value::Null))
            .trim()
            .to_owned();
        if existing_id == new_id {
            let stored_cancelled = existing
                .get("cancelled")
                .is_some_and(solstone_core_facets::activity_value_truthy);
            let incoming_cancelled = record
                .get("cancelled")
                .is_some_and(solstone_core_facets::activity_value_truthy);
            let cancel_flip = !stored_cancelled && incoming_cancelled;

            let mut patch = Map::new();
            for key in [
                "partial_input",
                "activity",
                "target_date",
                "start",
                "end",
                "title",
                "description",
                "details",
                "active_entities",
                "participation",
                "participation_confidence",
                "cancelled",
                "hidden",
            ] {
                let existing_val = existing.get(key).unwrap_or(&Value::Null);
                let new_val = record.get(key).unwrap_or(&Value::Null);
                if existing_val != new_val {
                    if solstone_core_facets::activity_field_held(&existing, key) {
                        if key == "hidden" && cancel_flip {
                            // Cancel exception: hidden is unheld on cancel flip
                        } else {
                            continue;
                        }
                    }
                    patch.insert(key.to_owned(), new_val.clone());
                }
            }
            if patch.is_empty() {
                return Ok((AnticipationAction::Skip, Vec::new()));
            } else {
                return Ok((AnticipationAction::Update(patch), Vec::new()));
            }
        }
        if sequence_ratio(&new_title, &normalized_title(existing.get("title")))
            >= ANTICIPATION_FUZZY_THRESHOLD
        {
            superseded.push(existing_id);
        }
    }
    Ok((AnticipationAction::Create, superseded))
}

fn resolved_id(
    result: &solstone_core_entity::EntityResolution,
    entities: &[solstone_core_entity::EntityResolutionEntity],
) -> Value {
    if result.outcome == solstone_core_entity::EntityResolutionOutcome::Resolved {
        result
            .entity_index
            .and_then(|index| entities[index].id.clone())
            .map_or(Value::Null, Value::String)
    } else {
        Value::Null
    }
}
fn normalized_title(value: Option<&Value>) -> String {
    python_string(value.unwrap_or(&Value::Null))
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}
fn python_string(value: &Value) -> String {
    match value {
        Value::String(value) => value.clone(),
        Value::Bool(value) => {
            if *value {
                "True".into()
            } else {
                "False".into()
            }
        }
        Value::Null => "None".into(),
        value => value.to_string(),
    }
}

/// Python `difflib.SequenceMatcher(None, a, b).ratio()` over Unicode code points.
fn sequence_ratio(left: &str, right: &str) -> f64 {
    let left = left.chars().collect::<Vec<_>>();
    let right = right.chars().collect::<Vec<_>>();
    let total = left.len() + right.len();
    if total == 0 {
        return 1.0;
    }
    let mut queue = vec![(0, left.len(), 0, right.len())];
    let mut blocks = Vec::new();
    while let Some((alo, ahi, blo, bhi)) = queue.pop() {
        let (i, j, size) = longest_match(&left, &right, alo, ahi, blo, bhi);
        if size == 0 {
            continue;
        }
        blocks.push((i, j, size));
        if alo < i && blo < j {
            queue.push((alo, i, blo, j));
        }
        if i + size < ahi && j + size < bhi {
            queue.push((i + size, ahi, j + size, bhi));
        }
    }
    blocks.sort_unstable();
    let mut matched = 0;
    let mut prior = (0, 0, 0);
    for block in blocks {
        if prior.0 + prior.2 == block.0 && prior.1 + prior.2 == block.1 {
            prior.2 += block.2;
        } else {
            matched += prior.2;
            prior = block;
        }
    }
    matched += prior.2;
    2.0 * matched as f64 / total as f64
}

fn longest_match(
    a: &[char],
    b: &[char],
    alo: usize,
    ahi: usize,
    blo: usize,
    bhi: usize,
) -> (usize, usize, usize) {
    let mut best = (alo, blo, 0);
    let mut previous = BTreeMap::new();
    for (i, left) in a.iter().enumerate().take(ahi).skip(alo) {
        let mut current = BTreeMap::new();
        for (j, right) in b.iter().enumerate().take(bhi).skip(blo) {
            if left != right {
                continue;
            }
            let size = previous.get(&j.saturating_sub(1)).copied().unwrap_or(0) + 1;
            current.insert(j, size);
            if size > best.2 {
                best = (i + 1 - size, j + 1 - size, size);
            }
        }
        previous = current;
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn partial_schedule_survives_creation_and_clears_on_both_update_paths() {
        for frozen in [false, true] {
            let root = tempfile::tempdir().unwrap();
            solstone_core_facets::create_facet(root.path(), "work", "Work", "", "", "", None)
                .unwrap();
            for clipped in [true, false, true] {
                let mut output = serde_json::json!({"events":[{"activity":"meeting","target_date":"2026-03-14","start":"09:30:00","title":"Project sync","description":"Discuss roadmap","facet":"work","participation":[]}]});
                if clipped {
                    output["partial_input"] = serde_json::json!({"dropped_entries":2});
                }
                if frozen {
                    for batch in
                        prepare_publication(root.path(), &output.to_string(), "20260310").unwrap()
                    {
                        solstone_core_facets::publish_anticipation_batch(
                            root.path(),
                            &batch,
                            true,
                            || Ok(()),
                        )
                        .unwrap();
                    }
                } else {
                    apply_result(root.path(), &output.to_string(), "20260310").unwrap();
                }
                let rows = solstone_core_facets::load_activity_records(
                    root.path(),
                    "work",
                    "20260314",
                    true,
                )
                .unwrap();
                assert_eq!(rows.len(), 1);
                assert_eq!(
                    rows[0].get("partial_input").is_some_and(Value::is_object),
                    clipped,
                    "frozen={frozen}"
                );
            }
        }
    }

    #[test]
    fn anticipated_titles_and_descriptions_drop_a_leading_subject() {
        // the schedule talent's output reaches Home's today card verbatim, so it takes the
        // same register normaliser as every other activity record
        assert_eq!(
            normalize_activity_description("The user meets Sam at two"),
            "Meets Sam at two"
        );
        assert_eq!(
            normalize_activity_description("Dentist at nine"),
            "Dentist at nine"
        );
    }

    #[test]
    fn anticipation_id_and_fuzzy_supersede_match_reference_shape() {
        // Derived from solstone/think/activities.py:1350-1401.
        assert_eq!(
            make_anticipation_id("meeting", Some("09:30:00"), "2026-03-14").unwrap(),
            "anticipated_meeting_093000_0314"
        );
        assert!(sequence_ratio("project sync", "project synch") >= ANTICIPATION_FUZZY_THRESHOLD);
    }

    #[test]
    fn apply_result_updates_same_id_when_cancelled_or_modified() {
        let root = tempfile::tempdir().unwrap();
        solstone_core_facets::create_facet(root.path(), "work", "Work", "", "", "", None).unwrap();

        // Initial creation
        let output1 = r#"{"events":[{"activity":"meeting","target_date":"2026-03-14","start":"09:30:00","title":"Project sync","description":"Discuss roadmap","facet":"work","participation":[]}]}"#;
        apply_result(root.path(), output1, "20260310").unwrap();

        let records =
            solstone_core_facets::load_activity_records(root.path(), "work", "20260314", true)
                .unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["cancelled"], false);

        // Same ID update with cancelled = true
        let output2 = r#"{"events":[{"activity":"meeting","target_date":"2026-03-14","start":"09:30:00","title":"Project sync","description":"Discuss roadmap","facet":"work","cancelled":true,"participation":[]}]}"#;
        apply_result(root.path(), output2, "20260310").unwrap();

        let records2 =
            solstone_core_facets::load_activity_records(root.path(), "work", "20260314", true)
                .unwrap();
        assert_eq!(records2.len(), 1);
        assert_eq!(records2[0]["cancelled"], true);
    }

    fn reset_schedule_skip_warnings() {
        TEST_SCHEDULE_SKIP_WARNINGS.with(|warnings| warnings.borrow_mut().clear());
    }

    fn schedule_skip_warnings() -> Vec<String> {
        TEST_SCHEDULE_SKIP_WARNINGS.with(|warnings| warnings.borrow().clone())
    }

    #[test]
    fn prepare_publication_drops_non_future_event_and_keeps_valid_sibling() {
        let root = tempfile::tempdir().unwrap();
        solstone_core_facets::create_facet(root.path(), "work", "Work", "", "", "", None).unwrap();
        reset_schedule_skip_warnings();

        let output = json!({
            "events": [
                {
                    "activity": "meeting",
                    "target_date": "2026-09-10",
                    "start": "09:00:00",
                    "title": "Past sync",
                    "description": "Discuss roadmap",
                    "facet": "work",
                    "participation": []
                },
                {
                    "activity": "meeting",
                    "target_date": "2026-09-20",
                    "start": "09:00:00",
                    "title": "Future sync",
                    "description": "Discuss roadmap",
                    "facet": "work",
                    "participation": []
                }
            ]
        })
        .to_string();

        let batches = prepare_publication(root.path(), &output, "20260910").unwrap();
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].facet, "work");
        assert_eq!(batches[0].day, "20260920");
        assert!(batches[0].after.contains("Future sync"));
        assert!(!batches[0].after.contains("Past sync"));

        let warnings = schedule_skip_warnings();
        assert_eq!(warnings.len(), 1);
        assert_eq!(
            warnings[0],
            "schedule event skipped for day 20260910: bad target date"
        );
    }

    #[test]
    fn prepare_publication_aborts_on_structural_json_shape_and_day_errors() {
        let root = tempfile::tempdir().unwrap();
        solstone_core_facets::create_facet(root.path(), "work", "Work", "", "", "", None).unwrap();

        assert!(prepare_publication(root.path(), "not-json", "20260910").is_err());
        assert!(
            prepare_publication(root.path(), "{\"events\": \"not an array\"}", "20260910").is_err()
        );
        assert!(prepare_publication(root.path(), "[]", "bad-day").is_err());
    }

    #[test]
    fn prepare_publication_drops_single_invalid_events_and_warns_with_reason() {
        let root = tempfile::tempdir().unwrap();
        solstone_core_facets::create_facet(root.path(), "work", "Work", "", "", "", None).unwrap();

        let valid_sibling = json!({
            "activity": "meeting",
            "target_date": "2026-09-20",
            "start": "10:00:00",
            "title": "Valid meeting",
            "description": "Description",
            "facet": "work",
            "participation": []
        });

        let test_cases = vec![
            (
                json!({
                    "target_date": "2026-09-20",
                    "title": "Title",
                    "description": "Description",
                    "facet": "work",
                    "participation": []
                }),
                "missing field",
                Some("Title"),
            ),
            (
                json!({
                    "activity": "meeting",
                    "target_date": "2026-99-99",
                    "title": "Title",
                    "description": "Description",
                    "facet": "work",
                    "participation": []
                }),
                "bad target date",
                Some("Title"),
            ),
            (
                json!({
                    "activity": "meeting",
                    "target_date": "2026-09-10",
                    "title": "Title",
                    "description": "Description",
                    "facet": "work",
                    "participation": []
                }),
                "bad target date",
                Some("Title"),
            ),
            (
                json!({
                    "activity": "meeting",
                    "target_date": "2026-09-20",
                    "title": "Title",
                    "description": "Description",
                    "facet": "unknown_facet_name",
                    "participation": []
                }),
                "unknown facet",
                Some("Title"),
            ),
            (
                json!({
                    "activity": "meeting",
                    "target_date": "2026-09-20",
                    "title": "Title",
                    "description": "Description",
                    "facet": "work",
                }),
                "bad participation",
                Some("Title"),
            ),
            (
                json!({
                    "activity": "meeting",
                    "target_date": "2026-09-20",
                    "title": "Title",
                    "description": "Description",
                    "facet": "work",
                    "participation": "not an array"
                }),
                "bad participation",
                Some("Title"),
            ),
            (
                json!({
                    "activity": "meeting",
                    "target_date": "2026-09-20",
                    "start": "9:00",
                    "title": "Title",
                    "description": "Description",
                    "facet": "work",
                    "participation": []
                }),
                "bad time",
                Some("Title"),
            ),
            (json!("not an object"), "non-object event", None),
        ];

        for (bad_event, expected_reason, bad_title) in test_cases {
            reset_schedule_skip_warnings();
            let output = json!([bad_event, valid_sibling.clone()]).to_string();
            let batches = prepare_publication(root.path(), &output, "20260910").unwrap();
            assert_eq!(batches.len(), 1, "Failed for reason: {expected_reason}");
            assert!(
                batches[0].after.contains("Valid meeting"),
                "batches[0].after missing 'Valid meeting' for reason: {expected_reason}"
            );
            if let Some(bad_title) = bad_title {
                assert!(
                    !batches[0].after.contains(bad_title),
                    "batches[0].after contains '{bad_title}' for reason: {expected_reason}"
                );
            }

            let warnings = schedule_skip_warnings();
            assert_eq!(warnings.len(), 1, "Failed for reason: {expected_reason}");
            assert_eq!(
                warnings[0],
                format!("schedule event skipped for day 20260910: {expected_reason}")
            );
        }
    }

    #[test]
    fn prepare_publication_returns_empty_batches_when_all_events_are_skipped() {
        let root = tempfile::tempdir().unwrap();
        solstone_core_facets::create_facet(root.path(), "work", "Work", "", "", "", None).unwrap();
        reset_schedule_skip_warnings();

        let output = json!([
            {
                "activity": "meeting",
                "target_date": "2026-09-10",
                "title": "Past meeting",
                "description": "Description",
                "facet": "work",
                "participation": []
            },
            {
                "activity": "meeting",
                "target_date": "2026-09-20",
                "title": "Unknown facet meeting",
                "description": "Description",
                "facet": "unknown",
                "participation": []
            }
        ])
        .to_string();

        let batches = prepare_publication(root.path(), &output, "20260910").unwrap();
        assert!(batches.is_empty());

        let warnings = schedule_skip_warnings();
        assert_eq!(warnings.len(), 2);
    }

    #[test]
    #[cfg(unix)]
    fn prepare_publication_aborts_on_unreadable_detected_entities_store_error() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        solstone_core_facets::create_facet(root.path(), "work", "Work", "", "", "", None).unwrap();

        let entities_dir = root.path().join("facets/work/entities");
        std::fs::create_dir_all(&entities_dir).unwrap();
        let entity_file = entities_dir.join("20260920.jsonl");
        std::fs::write(&entity_file, "{\"name\":\"test\"}\n").unwrap();

        // Make unreadable
        std::fs::set_permissions(&entity_file, std::fs::Permissions::from_mode(0o000)).unwrap();

        let output = json!([{
            "activity": "meeting",
            "target_date": "2026-09-20",
            "title": "Meeting",
            "description": "Description",
            "facet": "work",
            "participation": []
        }])
        .to_string();

        let result = prepare_publication(root.path(), &output, "20260910");
        // Restore permissions for cleanup
        let _ = std::fs::set_permissions(&entity_file, std::fs::Permissions::from_mode(0o644));

        assert!(result.is_err());
    }

    #[test]
    fn prepare_publication_aborts_on_corrupt_ambiguities_store_read_error() {
        let root = tempfile::tempdir().unwrap();
        solstone_core_facets::create_facet(root.path(), "work", "Work", "", "", "", None).unwrap();

        let ambiguities_path = root.path().join("entities/ambiguities.jsonl");
        std::fs::create_dir_all(ambiguities_path.parent().unwrap()).unwrap();
        std::fs::write(&ambiguities_path, "{not valid json}\n").unwrap();

        let output = json!([{
            "activity": "meeting",
            "target_date": "2026-09-20",
            "title": "Meeting",
            "description": "Description",
            "facet": "work",
            "participation": [
                {
                    "name": "Sarah",
                    "role": "attendee",
                    "source": "screen",
                    "confidence": 0.9,
                    "context": "test"
                }
            ]
        }])
        .to_string();

        let result = prepare_publication(root.path(), &output, "20260910");
        assert!(result.is_err());
    }

    #[test]
    fn prepare_publication_drops_absent_and_blocked_resolved_choice_events() {
        let root = tempfile::tempdir().unwrap();
        solstone_core_facets::create_facet(root.path(), "work", "Work", "", "", "", None).unwrap();

        // Seed choices
        solstone_core_entity::record_ambiguity_observation(
            root.path(),
            &solstone_core_entity::AmbiguityObservation {
                scope: json!({"kind": "facet", "facet": "work"}),
                query: "Sarah".to_owned(),
                normalized_query: "sarah".to_owned(),
                observed_tier: 5,
                ranked_candidates: vec![json!({
                    "id": "sarah_absent",
                    "name": "Sarah",
                    "tier": 5,
                    "score": 90.0,
                })],
                origin: json!({"lane": "test"}),
            },
        )
        .unwrap();

        solstone_core_entity::record_ambiguity_choice(
            root.path(),
            &solstone_core_entity::AmbiguityChoiceRequest {
                scope: json!({"kind": "facet", "facet": "work"}),
                query: "Sarah".to_owned(),
                entity_id: "sarah_absent".to_owned(),
                origin: None,
            },
            &[solstone_core_entity::AmbiguityChoiceEntity {
                id: "sarah_absent".to_owned(),
                blocked: false,
            }],
        )
        .unwrap();

        solstone_core_entity::record_ambiguity_observation(
            root.path(),
            &solstone_core_entity::AmbiguityObservation {
                scope: json!({"kind": "facet", "facet": "work"}),
                query: "Bob".to_owned(),
                normalized_query: "bob".to_owned(),
                observed_tier: 5,
                ranked_candidates: vec![json!({
                    "id": "bob_blocked",
                    "name": "Bob",
                    "tier": 5,
                    "score": 90.0,
                })],
                origin: json!({"lane": "test"}),
            },
        )
        .unwrap();

        solstone_core_entity::record_ambiguity_choice(
            root.path(),
            &solstone_core_entity::AmbiguityChoiceRequest {
                scope: json!({"kind": "facet", "facet": "work"}),
                query: "Bob".to_owned(),
                entity_id: "bob_blocked".to_owned(),
                origin: None,
            },
            &[solstone_core_entity::AmbiguityChoiceEntity {
                id: "bob_blocked".to_owned(),
                blocked: false,
            }],
        )
        .unwrap();

        // Create detected entities in target day containing bob_blocked with blocked=true
        let entities_dir = root.path().join("facets/work/entities");
        std::fs::create_dir_all(&entities_dir).unwrap();
        let entity_file = entities_dir.join("20260920.jsonl");
        std::fs::write(
            &entity_file,
            json!({"id": "bob_blocked", "type": "person", "name": "Bob", "blocked": true})
                .to_string()
                + "\n",
        )
        .unwrap();

        reset_schedule_skip_warnings();

        let output = json!([
            {
                "activity": "meeting",
                "target_date": "2026-09-20",
                "start": "09:00:00",
                "title": "Meeting with Sarah",
                "description": "Description",
                "facet": "work",
                "participation": [{"name": "Sarah", "role": "attendee"}]
            },
            {
                "activity": "meeting",
                "target_date": "2026-09-20",
                "start": "10:00:00",
                "title": "Meeting with Bob",
                "description": "Description",
                "facet": "work",
                "participation": [{"name": "Bob", "role": "attendee"}]
            },
            {
                "activity": "meeting",
                "target_date": "2026-09-20",
                "start": "11:00:00",
                "title": "Solo sync",
                "description": "Description",
                "facet": "work",
                "participation": []
            }
        ])
        .to_string();

        let batches = prepare_publication(root.path(), &output, "20260910").unwrap();
        assert_eq!(batches.len(), 1);
        assert!(batches[0].after.contains("Solo sync"));
        assert!(!batches[0].after.contains("Meeting with Sarah"));
        assert!(!batches[0].after.contains("Meeting with Bob"));

        let warnings = schedule_skip_warnings();
        assert_eq!(warnings.len(), 2);
        assert_eq!(
            warnings[0],
            "schedule event skipped for day 20260910: resolved-choice entity absent"
        );
        assert_eq!(
            warnings[1],
            "schedule event skipped for day 20260910: resolved-choice entity blocked"
        );
    }

    #[test]
    fn prepare_publication_records_distinct_warning_for_each_skipped_event() {
        let root = tempfile::tempdir().unwrap();
        solstone_core_facets::create_facet(root.path(), "work", "Work", "", "", "", None).unwrap();
        reset_schedule_skip_warnings();

        let output = json!([
            {
                "target_date": "2026-09-20",
                "title": "No activity",
                "description": "Description",
                "facet": "work",
                "participation": []
            },
            {
                "activity": "meeting",
                "target_date": "2026-09-01",
                "title": "Past date",
                "description": "Description",
                "facet": "work",
                "participation": []
            },
            "not an object",
            {
                "activity": "meeting",
                "target_date": "2026-09-20",
                "title": "Valid sync",
                "description": "Description",
                "facet": "work",
                "participation": []
            }
        ])
        .to_string();

        let batches = prepare_publication(root.path(), &output, "20260910").unwrap();
        assert_eq!(batches.len(), 1);
        assert!(batches[0].after.contains("Valid sync"));

        let warnings = schedule_skip_warnings();
        assert_eq!(warnings.len(), 3);
        assert_eq!(
            warnings[0],
            "schedule event skipped for day 20260910: missing field"
        );
        assert_eq!(
            warnings[1],
            "schedule event skipped for day 20260910: bad target date"
        );
        assert_eq!(
            warnings[2],
            "schedule event skipped for day 20260910: non-object event"
        );
    }

    #[test]
    fn schedule_anticipation_protection_and_cancellation_lifecycle() {
        let root = tempfile::TempDir::new_in("/var/tmp").unwrap();
        solstone_core_facets::create_facet(root.path(), "work", "Work", "", "", "", None).unwrap();
        let day_file = root.path().join("facets/work/activities/20260314.jsonl");
        std::fs::create_dir_all(day_file.parent().unwrap()).unwrap();

        // 1. An owner-hidden plan (owner:mute is the latest edit listing hidden, value true)
        // stays hidden when the sighting says hidden: false.
        // 2. An owner-retitled plan (owner:update lists title) keeps title while changed details is written.
        let seed_record = json!({
            "id": "anticipated_meeting_093000_0314",
            "activity": "meeting",
            "target_date": "2026-03-14",
            "start": "09:30:00",
            "end": "10:00:00",
            "title": "Owner Title",
            "description": "Discuss roadmap",
            "details": "old details",
            "facet": "work",
            "source": "anticipated",
            "active_entities": [],
            "participation": [],
            "participation_confidence": Value::Null,
            "cancelled": false,
            "hidden": true,
            "edits": [
                {
                    "timestamp": "2026-03-10T09:00:00Z",
                    "actor": "schedule",
                    "fields": ["activity", "target_date", "start", "end", "title", "description", "details", "source", "active_entities", "participation", "participation_confidence", "cancelled", "hidden"],
                    "note": "created by schedule"
                },
                {
                    "timestamp": "2026-03-10T09:30:00Z",
                    "actor": "owner:update",
                    "fields": ["title"],
                    "note": "updated title"
                },
                {
                    "timestamp": "2026-03-10T10:00:00Z",
                    "actor": "owner:mute",
                    "fields": ["hidden"],
                    "note": "muted by owner"
                }
            ]
        });
        std::fs::write(
            &day_file,
            format!("{}\n", serde_json::to_string(&seed_record).unwrap()),
        )
        .unwrap();

        // Single path: sighting says hidden: false, title: "Schedule Title", details: "new details"
        let sighting = json!({
            "events": [{
                "activity": "meeting",
                "target_date": "2026-03-14",
                "start": "09:30:00",
                "end": "10:00:00",
                "title": "Schedule Title",
                "description": "Discuss roadmap",
                "details": "new details",
                "facet": "work",
                "participation": []
            }]
        })
        .to_string();
        apply_result(root.path(), &sighting, "20260310").unwrap();

        let records =
            solstone_core_facets::load_activity_records(root.path(), "work", "20260314", true)
                .unwrap();
        assert_eq!(records.len(), 1);
        // Stays hidden
        assert_eq!(records[0]["hidden"], true);
        // Keeps owner title
        assert_eq!(records[0]["title"], "Owner Title");
        // Details updated
        assert_eq!(records[0]["details"], "new details");

        // 3. A plan whose edits are only schedule updates every changed field.
        let plain_plan = json!({
            "id": "anticipated_call_140000_0314",
            "activity": "call",
            "target_date": "2026-03-14",
            "start": "14:00:00",
            "end": "14:30:00",
            "title": "Old Call",
            "description": "Old desc",
            "details": "old details",
            "facet": "work",
            "source": "anticipated",
            "active_entities": [],
            "participation": [],
            "participation_confidence": 0.5,
            "cancelled": false,
            "hidden": false,
            "edits": [
                {
                    "timestamp": "2026-03-10T09:00:00Z",
                    "actor": "schedule",
                    "fields": ["activity", "target_date", "start", "end", "title", "description", "details", "source", "active_entities", "participation", "participation_confidence", "cancelled", "hidden"],
                    "note": "created by schedule"
                }
            ]
        });
        std::fs::write(
            &day_file,
            format!("{}\n", serde_json::to_string(&plain_plan).unwrap()),
        )
        .unwrap();
        let sighting_call = json!({
            "events": [{
                "activity": "call",
                "target_date": "2026-03-14",
                "start": "14:00:00",
                "end": "15:00:00",
                "title": "New Call",
                "description": "New desc",
                "details": "new details",
                "facet": "work",
                "participation": []
            }]
        })
        .to_string();
        apply_result(root.path(), &sighting_call, "20260310").unwrap();
        let records =
            solstone_core_facets::load_activity_records(root.path(), "work", "20260314", true)
                .unwrap();
        assert_eq!(records[0]["end"], "15:00:00");
        assert_eq!(records[0]["title"], "New Call");
        assert_eq!(records[0]["description"], "New desc");

        // 4. A protected-only difference leaves the day file bytes identical and appends no schedule edit.
        // For the batch, assert after == before on the PreparedAnticipationBatch.
        // For the single path, read the file before and after apply_result.
        // Single path:
        std::fs::write(
            &day_file,
            format!("{}\n", serde_json::to_string(&seed_record).unwrap()),
        )
        .unwrap();
        let before_bytes = std::fs::read(&day_file).unwrap();
        // Sighting only differs in title (held) and hidden (held)
        let protected_only_sighting = json!({
            "events": [{
                "activity": "meeting",
                "target_date": "2026-03-14",
                "start": "09:30:00",
                "end": "10:00:00",
                "title": "Schedule Title",
                "description": "Discuss roadmap",
                "details": "old details",
                "facet": "work",
                "participation": []
            }]
        })
        .to_string();
        apply_result(root.path(), &protected_only_sighting, "20260310").unwrap();
        let after_bytes = std::fs::read(&day_file).unwrap();
        assert_eq!(before_bytes, after_bytes);

        // Batch path:
        let batches =
            prepare_publication(root.path(), &protected_only_sighting, "20260310").unwrap();
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].after, batches[0].before.clone().unwrap());

        // 5. An owner-unhid plan (owner:unmute, hidden false) that arrives cancelled is hidden,
        // the new schedule edit lists hidden, and its note is "updated by schedule (cancelled on calendar)".
        // A following uncancelled sighting makes it visible. On that second sighting the latest edit that listed
        // hidden is the schedule cancel, so hidden may change.
        let unhid_record = json!({
            "id": "anticipated_meeting_093000_0314",
            "activity": "meeting",
            "target_date": "2026-03-14",
            "start": "09:30:00",
            "end": "10:00:00",
            "title": "Project sync",
            "description": "Discuss roadmap",
            "details": "",
            "facet": "work",
            "source": "anticipated",
            "active_entities": [],
            "participation": [],
            "participation_confidence": 0.5,
            "cancelled": false,
            "hidden": false,
            "edits": [
                {
                    "timestamp": "2026-03-10T09:00:00Z",
                    "actor": "schedule",
                    "fields": ["activity", "target_date", "start", "end", "title", "description", "details", "source", "active_entities", "participation", "participation_confidence", "cancelled", "hidden"],
                    "note": "created by schedule"
                },
                {
                    "timestamp": "2026-03-10T10:00:00Z",
                    "actor": "owner:unmute",
                    "fields": ["hidden"],
                    "note": "unmuted by owner"
                }
            ]
        });
        std::fs::write(
            &day_file,
            format!("{}\n", serde_json::to_string(&unhid_record).unwrap()),
        )
        .unwrap();
        let cancelled_sighting = json!({
            "events": [{
                "activity": "meeting",
                "target_date": "2026-03-14",
                "start": "09:30:00",
                "end": "10:00:00",
                "title": "Project sync",
                "description": "Discuss roadmap",
                "facet": "work",
                "cancelled": true,
                "participation": []
            }]
        })
        .to_string();
        apply_result(root.path(), &cancelled_sighting, "20260310").unwrap();
        let records =
            solstone_core_facets::load_activity_records(root.path(), "work", "20260314", true)
                .unwrap();
        assert_eq!(records[0]["hidden"], true);
        let edits = records[0]["edits"].as_array().unwrap();
        let last_edit = edits.last().unwrap();
        assert_eq!(
            last_edit["note"],
            "updated by schedule (cancelled on calendar)"
        );
        let fields = last_edit["fields"].as_array().unwrap();
        assert!(fields.iter().any(|f| f == "hidden"));
        assert!(fields.iter().any(|f| f == "cancelled"));

        // A following uncancelled sighting makes it visible.
        let uncancelled_sighting = json!({
            "events": [{
                "activity": "meeting",
                "target_date": "2026-03-14",
                "start": "09:30:00",
                "end": "10:00:00",
                "title": "Project sync",
                "description": "Discuss roadmap",
                "facet": "work",
                "cancelled": false,
                "participation": []
            }]
        })
        .to_string();
        apply_result(root.path(), &uncancelled_sighting, "20260310").unwrap();
        let records =
            solstone_core_facets::load_activity_records(root.path(), "work", "20260314", true)
                .unwrap();
        assert_eq!(records[0]["hidden"], false);
        assert_eq!(records[0]["cancelled"], false);

        // 6. An owner-hidden plan that arrives cancelled and then uncancelled stays hidden.
        // The cancel edit does not list hidden because the value was already true. hidden's latest listing edit is still owner:mute.
        let owner_hidden_record = json!({
            "id": "anticipated_meeting_093000_0314",
            "activity": "meeting",
            "target_date": "2026-03-14",
            "start": "09:30:00",
            "end": "10:00:00",
            "title": "Project sync",
            "description": "Discuss roadmap",
            "details": "",
            "facet": "work",
            "source": "anticipated",
            "active_entities": [],
            "participation": [],
            "participation_confidence": 0.5,
            "cancelled": false,
            "hidden": true,
            "edits": [
                {
                    "timestamp": "2026-03-10T09:00:00Z",
                    "actor": "schedule",
                    "fields": ["activity", "target_date", "start", "end", "title", "description", "details", "source", "active_entities", "participation", "participation_confidence", "cancelled", "hidden"],
                    "note": "created by schedule"
                },
                {
                    "timestamp": "2026-03-10T10:00:00Z",
                    "actor": "owner:mute",
                    "fields": ["hidden"],
                    "note": "muted by owner"
                }
            ]
        });
        std::fs::write(
            &day_file,
            format!("{}\n", serde_json::to_string(&owner_hidden_record).unwrap()),
        )
        .unwrap();
        apply_result(root.path(), &cancelled_sighting, "20260310").unwrap();
        let records =
            solstone_core_facets::load_activity_records(root.path(), "work", "20260314", true)
                .unwrap();
        let edits = records[0]["edits"].as_array().unwrap();
        let cancel_edit = edits.last().unwrap();
        let fields = cancel_edit["fields"].as_array().unwrap();
        assert!(!fields.iter().any(|f| f == "hidden"));
        assert!(fields.iter().any(|f| f == "cancelled"));

        // Following uncancelled sighting:
        apply_result(root.path(), &uncancelled_sighting, "20260310").unwrap();
        let records =
            solstone_core_facets::load_activity_records(root.path(), "work", "20260314", true)
                .unwrap();
        assert_eq!(records[0]["hidden"], true);

        // 7. Batch creation of a cancelled plan: the one creation edit's note is
        // "created by schedule (cancelled on calendar)" and its fields include cancelled.
        // There is no second creation edit.
        let batch_create_cancelled = json!({
            "events": [{
                "activity": "meeting",
                "target_date": "2026-03-15",
                "start": "09:30:00",
                "end": "10:00:00",
                "title": "Cancelled from start",
                "description": "Discuss roadmap",
                "facet": "work",
                "cancelled": true,
                "participation": []
            }]
        })
        .to_string();
        let batches =
            prepare_publication(root.path(), &batch_create_cancelled, "20260310").unwrap();
        assert_eq!(batches.len(), 1);
        let created_row: Value =
            serde_json::from_str(batches[0].after.lines().next().unwrap()).unwrap();
        let edits = created_row["edits"].as_array().unwrap();
        assert_eq!(edits.len(), 1);
        assert_eq!(
            edits[0]["note"],
            "created by schedule (cancelled on calendar)"
        );
        let fields = edits[0]["fields"].as_array().unwrap();
        assert!(fields.iter().any(|f| f == "cancelled"));

        // 8. A later batch cancel of an uncancelled plan: the creation edit's note does not contain cancelled;
        // the update edit's note is "updated by schedule (cancelled on calendar)" and its fields include cancelled.
        let batch_create_uncancelled = json!({
            "events": [{
                "activity": "meeting",
                "target_date": "2026-03-16",
                "start": "09:30:00",
                "end": "10:00:00",
                "title": "Will be cancelled",
                "description": "Discuss roadmap",
                "facet": "work",
                "cancelled": false,
                "participation": []
            }]
        })
        .to_string();
        let batches =
            prepare_publication(root.path(), &batch_create_uncancelled, "20260310").unwrap();
        assert_eq!(batches.len(), 1);
        let day_file_20260316 = root.path().join("facets/work/activities/20260316.jsonl");
        std::fs::write(&day_file_20260316, &batches[0].after).unwrap();
        let created_row: Value =
            serde_json::from_str(batches[0].after.lines().next().unwrap()).unwrap();
        assert_eq!(created_row["edits"][0]["note"], "created by schedule");

        let batch_cancel = json!({
            "events": [{
                "activity": "meeting",
                "target_date": "2026-03-16",
                "start": "09:30:00",
                "end": "10:00:00",
                "title": "Will be cancelled",
                "description": "Discuss roadmap",
                "facet": "work",
                "cancelled": true,
                "participation": []
            }]
        })
        .to_string();
        let batches = prepare_publication(root.path(), &batch_cancel, "20260310").unwrap();
        let updated_row: Value =
            serde_json::from_str(batches[0].after.lines().next().unwrap()).unwrap();
        let edits = updated_row["edits"].as_array().unwrap();
        assert_eq!(edits.len(), 2);
        assert_eq!(
            edits[1]["note"],
            "updated by schedule (cancelled on calendar)"
        );
        let fields = edits[1]["fields"].as_array().unwrap();
        assert!(fields.iter().any(|f| f == "cancelled"));

        // 9. Single-path resight of an already-cancelled plan with a new details:
        // the appended edit's fields do not include cancelled. Its note is still
        // "updated by schedule (cancelled on calendar)" because apply_event was not changed.
        // Assert that note so a future edit cannot "fix" it by dropping the suffix.
        std::fs::write(&day_file_20260316, &batches[0].after).unwrap();
        let resight_cancelled_new_details = json!({
            "events": [{
                "activity": "meeting",
                "target_date": "2026-03-16",
                "start": "09:30:00",
                "end": "10:00:00",
                "title": "Will be cancelled",
                "description": "Discuss roadmap",
                "details": "brand new details",
                "facet": "work",
                "cancelled": true,
                "participation": []
            }]
        })
        .to_string();
        apply_result(root.path(), &resight_cancelled_new_details, "20260310").unwrap();
        let records =
            solstone_core_facets::load_activity_records(root.path(), "work", "20260316", true)
                .unwrap();
        let edits = records[0]["edits"].as_array().unwrap();
        assert_eq!(edits.len(), 3);
        let third_edit = &edits[2];
        assert_eq!(
            third_edit["note"],
            "updated by schedule (cancelled on calendar)"
        );
        let fields = third_edit["fields"].as_array().unwrap();
        assert!(!fields.iter().any(|f| f == "cancelled"));
        assert!(fields.iter().any(|f| f == "details"));

        // 10. Supersede of a different id still hides it with superseded by {new_id}.
        // One assertion on the existing path is enough; do not rewrite the fuzzy matcher.
        let batch_create_to_supersede = json!({
            "events": [{
                "activity": "meeting",
                "target_date": "2026-03-17",
                "start": "09:30:00",
                "end": "10:00:00",
                "title": "Will be superseded",
                "description": "Discuss roadmap",
                "facet": "work",
                "cancelled": false,
                "participation": []
            }]
        })
        .to_string();
        let batches =
            prepare_publication(root.path(), &batch_create_to_supersede, "20260310").unwrap();
        let day_file_20260317 = root.path().join("facets/work/activities/20260317.jsonl");
        std::fs::write(&day_file_20260317, &batches[0].after).unwrap();

        let batch_supersede = json!({
            "events": [{
                "activity": "meeting",
                "target_date": "2026-03-17",
                "start": "10:30:00",
                "end": "11:00:00",
                "title": "Will be superseded",
                "description": "Discuss roadmap",
                "facet": "work",
                "participation": []
            }]
        })
        .to_string();
        let batches = prepare_publication(root.path(), &batch_supersede, "20260310").unwrap();
        assert!(
            batches[0]
                .after
                .contains("superseded by anticipated_meeting_103000_0317")
        );
    }

    #[test]
    fn batch_publication_owner_hidden_plan_reappearing_unhidden_remains_hidden() {
        let root = tempfile::TempDir::new_in("/var/tmp").unwrap();
        solstone_core_facets::create_facet(root.path(), "work", "Work", "", "", "", None).unwrap();
        let day_file = root.path().join("facets/work/activities/20260314.jsonl");
        std::fs::create_dir_all(day_file.parent().unwrap()).unwrap();

        let seed_record = json!({
            "id": "anticipated_meeting_093000_0314",
            "activity": "meeting",
            "target_date": "2026-03-14",
            "start": "09:30:00",
            "end": "10:00:00",
            "title": "Roadmap sync",
            "description": "Discuss roadmap",
            "details": "old details",
            "facet": "work",
            "source": "anticipated",
            "active_entities": [],
            "participation": [],
            "participation_confidence": Value::Null,
            "cancelled": false,
            "hidden": true,
            "edits": [
                {
                    "timestamp": "2026-03-10T09:00:00Z",
                    "actor": "schedule",
                    "fields": ["activity", "target_date", "start", "end", "title", "description", "details", "source", "active_entities", "participation", "participation_confidence", "cancelled", "hidden"],
                    "note": "created by schedule"
                },
                {
                    "timestamp": "2026-03-10T10:00:00Z",
                    "actor": "owner:mute",
                    "fields": ["hidden"],
                    "note": "muted by owner"
                }
            ]
        });
        std::fs::write(
            &day_file,
            format!("{}\n", serde_json::to_string(&seed_record).unwrap()),
        )
        .unwrap();

        let sighting = json!({
            "events": [{
                "activity": "meeting",
                "target_date": "2026-03-14",
                "start": "09:30:00",
                "end": "10:00:00",
                "title": "Roadmap sync",
                "description": "Discuss roadmap",
                "details": "new details",
                "facet": "work",
                "participation": []
            }]
        })
        .to_string();

        let batches = prepare_publication(root.path(), &sighting, "20260310").unwrap();
        for batch in batches {
            solstone_core_facets::publish_anticipation_batch(root.path(), &batch, true, || Ok(()))
                .unwrap();
        }

        let records =
            solstone_core_facets::load_activity_records(root.path(), "work", "20260314", true)
                .unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["hidden"], true);
        assert_eq!(records[0]["details"], "new details");
    }

    #[test]
    fn batch_publication_owner_retitled_plan_keeps_title_while_end_time_updates() {
        let root = tempfile::TempDir::new_in("/var/tmp").unwrap();
        solstone_core_facets::create_facet(root.path(), "work", "Work", "", "", "", None).unwrap();
        let day_file = root.path().join("facets/work/activities/20260314.jsonl");
        std::fs::create_dir_all(day_file.parent().unwrap()).unwrap();

        let seed_record = json!({
            "id": "anticipated_meeting_093000_0314",
            "activity": "meeting",
            "target_date": "2026-03-14",
            "start": "09:30:00",
            "end": "10:00:00",
            "title": "Owner Title",
            "description": "Discuss roadmap",
            "details": "old details",
            "facet": "work",
            "source": "anticipated",
            "active_entities": [],
            "participation": [],
            "participation_confidence": Value::Null,
            "cancelled": false,
            "hidden": false,
            "edits": [
                {
                    "timestamp": "2026-03-10T09:00:00Z",
                    "actor": "schedule",
                    "fields": ["activity", "target_date", "start", "end", "title", "description", "details", "source", "active_entities", "participation", "participation_confidence", "cancelled", "hidden"],
                    "note": "created by schedule"
                },
                {
                    "timestamp": "2026-03-10T09:30:00Z",
                    "actor": "owner:update",
                    "fields": ["title"],
                    "note": "updated title"
                }
            ]
        });
        std::fs::write(
            &day_file,
            format!("{}\n", serde_json::to_string(&seed_record).unwrap()),
        )
        .unwrap();

        let sighting = json!({
            "events": [{
                "activity": "meeting",
                "target_date": "2026-03-14",
                "start": "09:30:00",
                "end": "10:30:00",
                "title": "Schedule Title",
                "description": "Discuss roadmap",
                "details": "old details",
                "facet": "work",
                "participation": []
            }]
        })
        .to_string();

        let batches = prepare_publication(root.path(), &sighting, "20260310").unwrap();
        for batch in batches {
            solstone_core_facets::publish_anticipation_batch(root.path(), &batch, true, || Ok(()))
                .unwrap();
        }

        let records =
            solstone_core_facets::load_activity_records(root.path(), "work", "20260314", true)
                .unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["title"], "Owner Title");
        assert_eq!(records[0]["end"], "10:30:00");
    }

    #[test]
    fn batch_publication_owner_hidden_plan_seen_cancelled_and_uncancelled_remains_hidden() {
        let root = tempfile::TempDir::new_in("/var/tmp").unwrap();
        solstone_core_facets::create_facet(root.path(), "work", "Work", "", "", "", None).unwrap();
        let day_file = root.path().join("facets/work/activities/20260314.jsonl");
        std::fs::create_dir_all(day_file.parent().unwrap()).unwrap();

        let seed_record = json!({
            "id": "anticipated_meeting_093000_0314",
            "activity": "meeting",
            "target_date": "2026-03-14",
            "start": "09:30:00",
            "end": "10:00:00",
            "title": "Project sync",
            "description": "Discuss roadmap",
            "details": "",
            "facet": "work",
            "source": "anticipated",
            "active_entities": [],
            "participation": [],
            "participation_confidence": 0.5,
            "cancelled": false,
            "hidden": true,
            "edits": [
                {
                    "timestamp": "2026-03-10T09:00:00Z",
                    "actor": "schedule",
                    "fields": ["activity", "target_date", "start", "end", "title", "description", "details", "source", "active_entities", "participation", "participation_confidence", "cancelled", "hidden"],
                    "note": "created by schedule"
                },
                {
                    "timestamp": "2026-03-10T10:00:00Z",
                    "actor": "owner:mute",
                    "fields": ["hidden"],
                    "note": "muted by owner"
                }
            ]
        });
        std::fs::write(
            &day_file,
            format!("{}\n", serde_json::to_string(&seed_record).unwrap()),
        )
        .unwrap();

        let cancelled_sighting = json!({
            "events": [{
                "activity": "meeting",
                "target_date": "2026-03-14",
                "start": "09:30:00",
                "end": "10:00:00",
                "title": "Project sync",
                "description": "Discuss roadmap",
                "facet": "work",
                "cancelled": true,
                "participation": []
            }]
        })
        .to_string();

        let batches = prepare_publication(root.path(), &cancelled_sighting, "20260310").unwrap();
        for batch in batches {
            solstone_core_facets::publish_anticipation_batch(root.path(), &batch, true, || Ok(()))
                .unwrap();
        }

        let records =
            solstone_core_facets::load_activity_records(root.path(), "work", "20260314", true)
                .unwrap();
        assert_eq!(records[0]["hidden"], true);
        assert_eq!(records[0]["cancelled"], true);

        let uncancelled_sighting = json!({
            "events": [{
                "activity": "meeting",
                "target_date": "2026-03-14",
                "start": "09:30:00",
                "end": "10:00:00",
                "title": "Project sync",
                "description": "Discuss roadmap",
                "facet": "work",
                "cancelled": false,
                "participation": []
            }]
        })
        .to_string();

        let batches = prepare_publication(root.path(), &uncancelled_sighting, "20260310").unwrap();
        for batch in batches {
            solstone_core_facets::publish_anticipation_batch(root.path(), &batch, true, || Ok(()))
                .unwrap();
        }

        let records =
            solstone_core_facets::load_activity_records(root.path(), "work", "20260314", true)
                .unwrap();
        assert_eq!(records[0]["hidden"], true);
        assert_eq!(records[0]["cancelled"], false);
    }
}
