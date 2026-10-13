// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Frozen, typed input to supported daily hook stages. Workers never rebuild it.

use crate::{
    ExecutionContext, PreparedTalent, RuntimeOutcome, StageError,
    contract::{self, GateDecision, PrePostState, StageSpec},
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

#[derive(Serialize, Deserialize)]
enum FrozenState {
    None,
    DailySchedule(crate::daily_schedule::DailySchedulePreState),
    FacetNewsletter(crate::facet_newsletter::FacetNewsletterState),
    MorningBriefing(crate::morning_briefing::MorningBriefingPreState),
    EntitiesReview(crate::entities::review::ReviewState),
    EntitySuggest(crate::entities::suggest::SuggestState),
    EntityObserver(crate::entities::observer::ObserverState),
}

fn no_output_reason(reason: &str) -> bool {
    matches!(
        reason,
        "disabled" | "no_input" | "no_candidates" | "no substantive facet/day sources"
    ) || reason.starts_with("missing_required_")
}

pub fn freeze(
    mut prepared: PreparedTalent,
    context: &ExecutionContext,
) -> Result<Value, RuntimeOutcome> {
    let hook = solstone_core_indexer::daily_evidence::daily_hook(&prepared.name, &prepared.config)
        .map_err(|error| failure(&prepared, error))?;
    let stage = contract::resolve_hook(&hook)
        .ok_or_else(|| failure(&prepared, format!("unsupported daily hook {hook}")))?;
    capture_owner_expectations(&mut prepared, context, &hook)?;
    let mut state = PrePostState::None;
    if !prepared.config.contains_key("skip_reason") {
        if let Some(gate) = stage.gate {
            match gate(&prepared, context).map_err(RuntimeOutcome::StageFailed)? {
                GateDecision::Proceed => {}
                GateDecision::Skip(reason) => {
                    if hook == "facet_newsletter" && reason == "no substantive facet/day sources" {
                        // A no-output decision still owes the frozen coverage
                        // diagnostics, including sources wholly clipped by the
                        // canonical formatter. The admission E check brackets
                        // this read just as it brackets the normal build.
                        state = crate::facet_newsletter::build(&mut prepared, context)?;
                    }
                    prepared
                        .config
                        .insert("skip_reason".to_owned(), json!(reason));
                }
            }
        }
        if !prepared.config.contains_key("skip_reason") {
            if let Some(build) = stage.build {
                match build(&mut prepared, context) {
                    Ok(value) => state = value,
                    Err(RuntimeOutcome::Skipped { reason, .. }) if no_output_reason(&reason) => {
                        prepared
                            .config
                            .insert("skip_reason".to_owned(), json!(reason));
                    }
                    Err(error) => return Err(error),
                }
            }
            if let Some(apply) = stage.prompt_override {
                apply(&mut prepared, &state).map_err(RuntimeOutcome::StageFailed)?;
            }
        }
    }
    if let Some(reason) = prepared.config.get("skip_reason").and_then(Value::as_str)
        && !no_output_reason(reason)
    {
        return Err(failure(
            &prepared,
            format!("daily preparation refused: {reason}"),
        ));
    }
    let state = match state {
        PrePostState::None => FrozenState::None,
        PrePostState::DailySchedule(v) => FrozenState::DailySchedule(v),
        PrePostState::FacetNewsletter(v) => FrozenState::FacetNewsletter(v),
        PrePostState::MorningBriefing(v) => FrozenState::MorningBriefing(v),
        PrePostState::EntitiesReview(v) => FrozenState::EntitiesReview(v),
        PrePostState::EntitySuggest(v) => FrozenState::EntitySuggest(v),
        PrePostState::EntityObserver(v) => {
            prepared.config.insert(
                "_daily_observation_before".to_owned(),
                Value::Object(v.observation_before.clone()),
            );
            FrozenState::EntityObserver(v)
        }
        _ => {
            return Err(failure(
                &prepared,
                "unsupported frozen daily stage state".to_owned(),
            ));
        }
    };
    attach_daily_location(&mut prepared, context, &hook)?;
    Ok(
        json!({"version":1,"prepared":{"name":prepared.name,"config":prepared.config},"hook":hook,"state":state}),
    )
}

#[cfg(test)]
thread_local! {
    static CAPTURE_BARRIER: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}

const LOCATION_PART_BYTE_CAP: usize = 16384;
const LOCATION_CAVEAT: &str = "These words do not establish someone else's presence and do not establish a future commitment.";

fn attach_daily_location(
    prepared: &mut PreparedTalent,
    context: &ExecutionContext,
    hook: &str,
) -> Result<(), RuntimeOutcome> {
    use solstone_core_location::daily_location_fingerprint;
    let Some(day) = prepared
        .config
        .get("day")
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        return Ok(());
    };
    if !crate::daily_location::location_applies(&context.journal, hook, &day)
        .map_err(|error| failure(prepared, error))?
    {
        return Ok(());
    }
    // Source mutation cannot pass through capture, including an ABA returning
    // to the same bytes after rendering transient words.
    let _admission = solstone_core_journal_io::hold_daily_location_admission(&context.journal)
        .map_err(|error| failure(prepared, error.to_string()))?;
    let first = daily_location_fingerprint(&context.journal, &day)
        .map_err(|error| failure(prepared, error.to_string()))?;
    #[cfg(test)]
    CAPTURE_BARRIER.with(|slot| {
        if let Some(barrier) = slot.borrow_mut().take() {
            barrier();
        }
    });
    let rendered =
        render_location_part(&context.journal, &day).map_err(|error| failure(prepared, error))?;
    let ordinary = crate::ordinary_text_parts(&prepared.config);
    let (part, omitted) = fit_location_part(hook, &day, &rendered, &ordinary, prepared, context);
    let second = daily_location_fingerprint(&context.journal, &day)
        .map_err(|error| failure(prepared, error.to_string()))?;
    if first != second {
        return Err(failure(
            prepared,
            "daily location changed while it was read".to_owned(),
        ));
    }
    prepared.config.insert(
        "_daily_location_fingerprint".to_owned(),
        Value::String(second),
    );
    if !ordinary.is_empty()
        && let Some(text) = part
    {
        prepared
            .config
            .insert("_daily_location_text".to_owned(), Value::String(text));
    }
    if omitted > 0 {
        prepared.config.insert(
            "_daily_location_omitted".to_owned(),
            Value::Number(omitted.into()),
        );
    }
    Ok(())
}

fn render_location_part(journal: &std::path::Path, day: &str) -> Result<String, String> {
    let view = solstone_core_location::read_journal_day(journal, day)
        .map_err(|error| error.to_string())?;
    let store = solstone_core_location::places::PlaceStore::read_strict(journal)
        .map_err(|error| error.to_string())?;
    Ok(solstone_core_location::projection::render_day_document(
        &view, &store,
    ))
}

fn location_label(hook: &str, day: &str) -> String {
    if hook == "morning_briefing" {
        format!("Location words for {day}, the analysis day.")
    } else {
        format!("Location words for {day}.")
    }
}

fn fit_location_part(
    hook: &str,
    day: &str,
    rendered: &str,
    ordinary: &[String],
    prepared: &PreparedTalent,
    context: &ExecutionContext,
) -> (Option<String>, u32) {
    use solstone_core_local::generate::{
        compute_input_budget, estimate_tokens, launched_context_window,
    };
    let max_output_tokens = prepared
        .config
        .get("max_output_tokens")
        .and_then(Value::as_u64)
        .and_then(|tokens| u32::try_from(tokens).ok())
        .unwrap_or(0);
    let budget = compute_input_budget(max_output_tokens, launched_context_window(&context.journal));
    let ordinary_tokens = ordinary.iter().fold(0u32, |total, text| {
        total.saturating_add(estimate_tokens(text))
    });
    let room = budget.saturating_sub(ordinary_tokens);
    let byte_cap = usize::try_from(room)
        .unwrap_or(usize::MAX)
        .saturating_mul(3)
        .min(LOCATION_PART_BYTE_CAP);
    let label = location_label(hook, day);
    if room == 0 || byte_cap == 0 {
        return (None, clip_location_part(&label, rendered, 0).1);
    }
    // estimate_tokens is ceil(character_count / 3). The byte bound above is
    // conservative for UTF-8, so whole observations fitting it also fit room.
    // Searching byte caps would be invalid: below the first whole observation
    // clip returns None, even though a larger cap can fit it.
    let (part, omitted) = clip_location_part(&label, rendered, byte_cap);
    debug_assert!(
        part.as_ref()
            .is_none_or(|text| estimate_tokens(text) <= room)
    );
    (part, omitted)
}

/// Keep a prefix of whole observation lines that fits in `max_bytes`.
pub(crate) fn clip_location_part(
    label: &str,
    rendered: &str,
    max_bytes: usize,
) -> (Option<String>, u32) {
    let mut heading = String::new();
    let mut observations = Vec::new();
    for line in rendered.split_inclusive('\n') {
        if line.starts_with("- ") {
            observations.push(line);
        } else if observations.is_empty() {
            heading.push_str(line);
        }
    }
    let total = u32::try_from(observations.len()).unwrap_or(u32::MAX);
    let prefix = format!("{label}\n{LOCATION_CAVEAT}\n{heading}");
    if max_bytes == 0 || prefix.len() > max_bytes || observations.is_empty() {
        return (None, total);
    }
    let mut kept = prefix;
    let mut kept_count = 0u32;
    for line in observations {
        if kept.len() + line.len() > max_bytes {
            break;
        }
        kept.push_str(line);
        kept_count += 1;
    }
    if kept_count == 0 {
        return (None, total);
    }
    (Some(kept), total - kept_count)
}

fn failure(prepared: &PreparedTalent, detail: String) -> RuntimeOutcome {
    RuntimeOutcome::StageFailed(StageError::new(
        "prepare",
        "daily",
        prepared.name.clone(),
        detail,
    ))
}

pub type ThawedStage = (PreparedTalent, Option<(&'static StageSpec, PrePostState)>);

pub fn thaw(packet: &Value) -> Result<ThawedStage, String> {
    if packet.get("version").and_then(Value::as_u64) != Some(1) {
        return Err("unsupported frozen daily packet version".to_owned());
    }
    let value = packet
        .get("prepared")
        .ok_or("frozen daily packet has no prepared input")?;
    let name = value
        .get("name")
        .and_then(Value::as_str)
        .ok_or("frozen daily packet has no talent")?
        .to_owned();
    let config: Map<String, Value> = value
        .get("config")
        .and_then(Value::as_object)
        .cloned()
        .ok_or("frozen daily packet has no config")?;
    let hook = solstone_core_indexer::daily_evidence::daily_hook(&name, &config)?;
    if packet.get("hook").and_then(Value::as_str) != Some(&hook) {
        return Err("frozen daily packet hook mismatch".to_owned());
    }
    let stage = contract::resolve_hook(&hook).ok_or("unsupported frozen daily hook")?;
    let frozen: FrozenState = serde_json::from_value(
        packet
            .get("state")
            .cloned()
            .ok_or("frozen daily packet has no state")?,
    )
    .map_err(|e| e.to_string())?;
    let (state, expected) = match frozen {
        FrozenState::None => (PrePostState::None, "schedule"),
        FrozenState::DailySchedule(v) => (PrePostState::DailySchedule(v), "daily_schedule"),
        FrozenState::FacetNewsletter(v) => (PrePostState::FacetNewsletter(v), "facet_newsletter"),
        FrozenState::MorningBriefing(v) => (PrePostState::MorningBriefing(v), "morning_briefing"),
        FrozenState::EntitiesReview(v) => {
            (PrePostState::EntitiesReview(v), "entities:entities_review")
        }
        FrozenState::EntitySuggest(v) => {
            (PrePostState::EntitySuggest(v), "entities:entity_suggest")
        }
        FrozenState::EntityObserver(v) => {
            (PrePostState::EntityObserver(v), "entities:entity_observer")
        }
    };
    if let Some(reason) = config.get("skip_reason").and_then(Value::as_str) {
        if !no_output_reason(reason) {
            return Err("invalid frozen daily skip reason".to_owned());
        }
    } else if hook != expected {
        return Err("frozen daily packet state mismatch".to_owned());
    }
    Ok((PreparedTalent { name, config }, Some((stage, state))))
}

pub fn packet_digest(packet: &Value) -> String {
    solstone_core_indexer::daily_evidence::digest(packet)
}

fn review_identity_stage_failed(
    prepared: &PreparedTalent,
    error: solstone_core_facets::FacetIdentityError,
) -> RuntimeOutcome {
    review_owner_stage_failed(prepared, error.into())
}

fn review_owner_stage_failed(
    prepared: &PreparedTalent,
    error: solstone_core_entity::ReviewOwnerError,
) -> RuntimeOutcome {
    let identity = solstone_core_journal_io::DailyUnitIdentity::new(
        prepared
            .config
            .get("day")
            .and_then(Value::as_str)
            .unwrap_or_default(),
        prepared.name.clone(),
        prepared
            .config
            .get("facet")
            .and_then(Value::as_str)
            .map(str::to_owned),
    );
    RuntimeOutcome::StageFailed(match error {
        solstone_core_entity::ReviewOwnerError::Conflict { kind, detail } => {
            StageError::owner_conflict(&identity, "daily_publication", kind, detail)
        }
        solstone_core_entity::ReviewOwnerError::Failed { detail } => StageError::new(
            "publication",
            "daily_publication",
            identity.name.clone(),
            detail,
        )
        .with_identity(&identity),
    })
}

fn capture_owner_expectations(
    prepared: &mut PreparedTalent,
    context: &ExecutionContext,
    hook: &str,
) -> Result<(), RuntimeOutcome> {
    if prepared.config.contains_key("skip_reason") {
        return Ok(());
    }
    let observe_review = hook == "entities:entities_review";
    let facet_ids = {
        let _guard = solstone_core_facets::hold_facet_trust_lock(&context.journal)
            .map_err(|e| failure(prepared, e.to_string()))?;
        let names = if hook == "schedule" {
            solstone_core_facets::list_declared_facet_names(&context.journal)
                .map_err(|e| failure(prepared, e.to_string()))?
        } else if matches!(
            hook,
            "facet_newsletter"
                | "entities:entities_review"
                | "entities:entity_suggest"
                | "entities:entity_observer"
        ) {
            prepared
                .config
                .get("facet")
                .and_then(Value::as_str)
                .map(|facet| vec![facet.to_owned()])
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let mut identities = Map::new();
        for facet in names {
            let id = if observe_review {
                solstone_core_facets::observe_facet_write_identity(&context.journal, &facet)
                    .map_err(|error| review_identity_stage_failed(prepared, error))?
            } else {
                solstone_core_facets::facet_write_identity(&context.journal, &facet)
                    .map_err(|error| failure(prepared, error.to_string()))?
            };
            identities.insert(facet, json!(id));
        }
        identities
    };
    prepared
        .config
        .insert("_daily_facet_ids".to_owned(), Value::Object(facet_ids));
    let mut paths = std::collections::BTreeSet::new();
    if let Some(path) = prepared.config.get("output_path").and_then(Value::as_str) {
        paths.insert(std::path::PathBuf::from(path));
    }
    if let (Some(facet), Some(day)) = (
        prepared.config.get("facet").and_then(Value::as_str),
        prepared.config.get("day").and_then(Value::as_str),
    ) {
        let relative = match hook {
            "facet_newsletter" => Some(format!("facets/{facet}/news/{day}.md")),
            "entities:entity_suggest" => Some(format!(
                "facets/{facet}/entities/{day}_observer_suggestions.json"
            )),
            "entities:entity_observer" => Some(format!(
                "facets/{facet}/entities/{day}_observer_outcome.json"
            )),
            "entities:entities_review" => {
                Some(format!("facets/{facet}/entities/{day}_review_outcome.json"))
            }
            _ => None,
        };
        if let Some(relative) = relative {
            paths.insert(context.journal.join(relative));
        }
    }
    let mut artifacts = Map::new();
    for path in paths {
        let action =
            crate::writers::bind_output_action(&context.journal, &path, Vec::new(), observe_review)
                .map_err(|error| {
                    if observe_review {
                        review_owner_stage_failed(prepared, error)
                    } else {
                        failure(prepared, error.to_string())
                    }
                })?;
        let crate::writers::PreparedDailyAction::Output { path, before, .. } = action else {
            return Err(failure(prepared, "invalid artifact preparation".to_owned()));
        };
        artifacts.insert(path, json!(before));
    }
    prepared.config.insert(
        "_daily_artifact_before".to_owned(),
        Value::Object(artifacts),
    );
    if hook == "daily_schedule" {
        let before = solstone_core_system::schedule::prepare_daily_time(
            &context.journal.join("config/schedules.json"),
            "00:00",
        )
        .map_err(|e| failure(prepared, e))?
        .before;
        prepared.config.insert(
            "_daily_time_before".to_owned(),
            before.unwrap_or(Value::Null),
        );
    }
    if hook == "schedule" {
        let day = prepared
            .config
            .get("day")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                failure(
                    prepared,
                    "daily calendar preparation requires day".to_owned(),
                )
            })?
            .to_owned();
        let _lock = solstone_core_facets::hold_facet_trust_lock(&context.journal)
            .map_err(|e| failure(prepared, e.to_string()))?;
        let mut before = Map::new();
        for facet in
            strict_entries(&context.journal.join("facets")).map_err(|e| failure(prepared, e))?
        {
            if !facet
                .file_type()
                .map_err(|e| failure(prepared, e.to_string()))?
                .is_dir()
            {
                continue;
            }
            let facet_name = facet
                .file_name()
                .to_str()
                .ok_or_else(|| failure(prepared, "invalid facet filename".to_owned()))?
                .to_owned();
            for entry in strict_entries(&facet.path().join("activities"))
                .map_err(|e| failure(prepared, e))?
            {
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                    continue;
                }
                let target = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .ok_or_else(|| failure(prepared, "invalid activity filename".to_owned()))?;
                if chrono::NaiveDate::parse_from_str(target, "%Y%m%d").is_err()
                    || target <= day.as_str()
                {
                    continue;
                }
                let raw = solstone_core_facets::read_activity_file(
                    &context.journal,
                    &facet_name,
                    &format!("{target}.jsonl"),
                )
                .map_err(|e| failure(prepared, e.to_string()))?;
                if let Some(raw) = &raw {
                    for line in raw.lines().filter(|s| !s.trim().is_empty()) {
                        let row: Value = serde_json::from_str(line).map_err(|e| {
                            failure(prepared, format!("calendar {facet_name}/{target}: {e}"))
                        })?;
                        if !row.is_object() {
                            return Err(failure(
                                prepared,
                                format!("calendar {facet_name}/{target} row is not an object"),
                            ));
                        }
                    }
                }
                before.insert(
                    format!("{facet_name}/{target}"),
                    raw.map(Value::String).unwrap_or(Value::Null),
                );
            }
        }
        prepared
            .config
            .insert("_daily_calendar_before".to_owned(), Value::Object(before));
    }
    Ok(())
}

fn strict_entries(path: &std::path::Path) -> Result<Vec<std::fs::DirEntry>, String> {
    match std::fs::read_dir(path) {
        Ok(entries) => entries
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn write(root: &std::path::Path, rel: &str, text: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    #[test]
    fn review_admission_snapshot_observes_malformed_facet_without_repair() {
        let root = tempfile::tempdir().unwrap();
        solstone_core_facets::create_facet(root.path(), "work", "Work", "", "", "", None).unwrap();
        let path = root.path().join("facets/work/facet.json");
        std::fs::write(&path, b"{").unwrap();
        let prepared = PreparedTalent {
            name: "entities:entities_review".to_owned(),
            config: json!({
                "type":"generate","max_output_tokens":1024,
                "day":"20260910",
                "facet":"work",
                "prompt":"review",
                "hook":{"pre":"entities:entities_review","post":"entities:entities_review"}
            })
            .as_object()
            .unwrap()
            .clone(),
        };
        let outcome = freeze(
            prepared,
            &ExecutionContext {
                journal: root.path().to_owned(),
            },
        )
        .unwrap_err();
        let RuntimeOutcome::StageFailed(error) = outcome else {
            panic!("{outcome:?}");
        };
        assert_eq!(error.phase, "publication");
        assert_eq!(error.reason_code(), "talent_stage_failed");
        assert_eq!(error.owner_conflict_kind(), None);
        assert_eq!(std::fs::read(&path).unwrap(), b"{");
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap())
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry.file_name().to_string_lossy().contains("wedged"))
                .count(),
            0
        );
    }

    #[test]
    fn review_admission_snapshot_types_missing_facet_as_owning_facet_changed() {
        let root = tempfile::tempdir().unwrap();
        let prepared = PreparedTalent {
            name: "entities:entities_review".to_owned(),
            config: json!({
                "type":"generate","max_output_tokens":1024,
                "day":"20260910",
                "facet":"missing",
                "prompt":"review",
                "hook":{"pre":"entities:entities_review","post":"entities:entities_review"}
            })
            .as_object()
            .unwrap()
            .clone(),
        };
        let outcome = freeze(
            prepared,
            &ExecutionContext {
                journal: root.path().to_owned(),
            },
        )
        .unwrap_err();
        let RuntimeOutcome::StageFailed(error) = outcome else {
            panic!("{outcome:?}");
        };
        assert_eq!(error.phase, "conflict");
        assert_eq!(error.reason_code(), "daily_owner_conflict");
        assert_eq!(error.owner_conflict_kind(), Some("owning_facet_changed"));
        assert_eq!(error.day(), "20260910");
        assert_eq!(error.facet(), Some("missing"));
    }

    #[test]
    fn newsletter_no_output_retains_all_clipped_source_diagnostics() {
        let root = tempfile::tempdir().unwrap();
        solstone_core_facets::create_facet(root.path(), "work", "Work", "", "", "", None).unwrap();
        // Continuity alone is not a substantive source for the day, but its
        // clipping is still coverage the no-output decision owes.
        write(
            root.path(),
            "facets/work/news/20260909.md",
            &"long prior newsletter line ".repeat(400),
        );
        let prepared = PreparedTalent {
            name: "facet_newsletter".to_owned(),
            config: json!({
                "type":"generate","max_output_tokens":1024, "day":"20260910", "facet":"work", "prompt":"$source_packet",
                "hook":{"pre":"facet_newsletter","post":"facet_newsletter"}
            })
            .as_object()
            .unwrap()
            .clone(),
        };
        let packet = freeze(
            prepared,
            &ExecutionContext {
                journal: root.path().to_owned(),
            },
        )
        .unwrap();
        assert_eq!(
            packet
                .pointer("/prepared/config/skip_reason")
                .and_then(Value::as_str),
            Some("no substantive facet/day sources")
        );
        let gaps = packet
            .pointer("/state/FacetNewsletter/packet/gaps")
            .and_then(Value::as_array)
            .expect("no-output coverage packet retained");
        assert!(
            gaps.iter()
                .filter_map(Value::as_str)
                .any(|gap| gap.contains("clipped:") && gap.contains("prior_newsletter")),
            "{gaps:?}"
        );
        assert!(thaw(&packet).is_ok());
    }
    #[test]
    fn frozen_newsletter_carries_its_sources_and_thaws_without_reads() {
        let root = tempfile::tempdir().unwrap();
        write(
            root.path(),
            "facets/work/facet.json",
            r#"{"id":"12345678-1234-4234-8234-123456789abc","title":"Work","description":"test scope"}"#,
        );
        let rel = "facets/work/activities/20260910.jsonl";
        write(
            root.path(),
            rel,
            &json!({"id":"coding_090000_60","activity":"coding","title":"Release work","story":{"body":"A freshly arrived source fact."}}).to_string(),
        );
        let context = ExecutionContext {
            journal: root.path().to_owned(),
        };
        let prepared = PreparedTalent {
            name: "facet_newsletter".to_owned(),
            config: Map::from_iter([
                ("max_output_tokens".to_owned(), json!(1024)),
                ("day".to_owned(), json!("20260910")),
                ("facet".to_owned(), json!("work")),
                (
                    "hook".to_owned(),
                    json!({"pre":"facet_newsletter","post":"facet_newsletter"}),
                ),
                ("user_instruction".to_owned(), json!("${source_packet}")),
            ]),
        };
        let packet = freeze(prepared, &context).unwrap();
        assert!(
            !packet["prepared"]["config"]
                .as_object()
                .unwrap()
                .contains_key("skip_reason")
        );
        assert!(packet.to_string().contains("freshly arrived source fact"));
        assert!(packet.to_string().contains("coding_090000_60"));
        let original_digest = packet_digest(&packet);
        std::fs::remove_file(root.path().join(rel)).unwrap();
        let (restored, state) = thaw(&packet).unwrap();
        assert!(
            restored.config["user_instruction"]
                .as_str()
                .unwrap()
                .contains("freshly arrived source fact")
        );
        assert!(matches!(state.unwrap().1, PrePostState::FacetNewsletter(_)));
        assert_eq!(packet_digest(&packet), original_digest);
        assert!(!root.path().join("indexer").exists());
    }
    #[test]
    fn frozen_packet_rejects_hook_state_mismatch_and_retains_legitimate_no_output() {
        let root = tempfile::tempdir().unwrap();
        let context = ExecutionContext {
            journal: root.path().to_owned(),
        };
        let config = Map::from_iter([
            ("hook".to_owned(), json!({"post":"schedule"})),
            ("skip_reason".to_owned(), json!("no_input")),
        ]);
        let packet = freeze(
            PreparedTalent {
                name: "schedule".to_owned(),
                config,
            },
            &context,
        )
        .unwrap();
        assert_eq!(thaw(&packet).unwrap().0.config["skip_reason"], "no_input");
        let mut corrupt = packet;
        corrupt["prepared"]["config"]
            .as_object_mut()
            .unwrap()
            .remove("skip_reason");
        corrupt["prepared"]["config"]["hook"] = json!({"pre":"morning_briefing"});
        corrupt["hook"] = json!("morning_briefing");
        corrupt["prepared"]["name"] = json!("morning_briefing");
        assert!(thaw(&corrupt).err().unwrap().contains("state mismatch"));
    }

    const LOCATION_DAY: &str = "20261001";

    fn write_bytes(root: &std::path::Path, rel: &str, bytes: &[u8]) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    fn location_journal(root: &std::path::Path, cutoff: &str) {
        write(
            root,
            "config/journal.json",
            &format!(
                r#"{{"identity":{{"timezone":"America/Denver"}},"daily_inputs":{{"location":{{"location-words-1":"{cutoff}"}}}}}}"#
            ),
        );
        let header = r#"{"accuracy":"full","fix_count":0,"gap":false,"kind":"location","platform":"ios","schema":"solstone.location.segment/1","source":"location","tier":"full"}"#;
        let body = format!(
            "{header}\n{}\n{}\n{}\n",
            r#"{"alt":null,"course":null,"h_acc":10.0,"lat":39.7555,"lon":-105.2211,"schema":"solstone.location.fix/1","speed":null,"stationary":false,"t":"2026-10-01T20:00:10Z","v_acc":null}"#,
            r#"{"alt":null,"course":null,"h_acc":10.0,"lat":39.7556,"lon":-105.2212,"schema":"solstone.location.fix/1","speed":null,"stationary":false,"t":"2026-10-01T20:04:10Z","v_acc":null}"#,
            r#"{"arrival":"2026-10-01T21:02:00Z","departure":"2026-10-01T22:40:00Z","h_acc":40,"lat":39.7527,"lon":-105.0001,"schema":"solstone.location.visit/1"}"#
        );
        write_bytes(
            root,
            &format!("chronicle/{LOCATION_DAY}/phone/140000_300/location.jsonl"),
            body.as_bytes(),
        );
        let store = solstone_core_location::places::PlaceStore {
            version: 1,
            places: vec![solstone_core_location::places::Place {
                id: "named".to_owned(),
                latitude: 39.7527,
                longitude: -105.0001,
                name: Some("$source_packet".to_owned()),
                alias: None,
                hidden: false,
            }],
        };
        store.write(root).unwrap();
    }

    fn prepared(name: &str, day: &str, extra: Value) -> PreparedTalent {
        let mut config = Map::from_iter([
            ("type".to_owned(), json!("generate")),
            ("max_output_tokens".to_owned(), json!(1024)),
            ("day".to_owned(), json!(day)),
            ("prompt".to_owned(), json!(format!("{name} ordinary"))),
        ]);
        if let Some(object) = extra.as_object() {
            for (key, value) in object {
                config.insert(key.clone(), value.clone());
            }
        }
        PreparedTalent {
            name: name.to_owned(),
            config,
        }
    }

    fn text_parts(prepared: &PreparedTalent) -> Vec<String> {
        crate::generate_contents(prepared)
            .into_iter()
            .map(|part| match part {
                solstone_core_generate::ContentPart::Text { text } => text,
                solstone_core_generate::ContentPart::Image { .. } => {
                    panic!("location context is text")
                }
            })
            .collect()
    }

    fn rendered_observations(root: &std::path::Path) -> Vec<String> {
        let view = solstone_core_location::read_journal_day(root, LOCATION_DAY).unwrap();
        let store = solstone_core_location::places::PlaceStore::read_strict(root).unwrap();
        solstone_core_location::projection::render_day_document(&view, &store)
            .lines()
            .filter(|line| line.starts_with("- "))
            .map(|line| format!("{line}\n"))
            .collect()
    }

    #[test]
    fn freeze_appends_location_for_the_five_recipients_only() {
        assert!(crate::contract::SCHEDULE.gate.is_none());
        assert!(crate::contract::SCHEDULE.build.is_none());
        let root = tempfile::tempdir().unwrap();
        location_journal(root.path(), LOCATION_DAY);
        solstone_core_facets::create_facet(root.path(), "work", "Work", "", "", "", None).unwrap();
        let context = ExecutionContext {
            journal: root.path().to_owned(),
        };
        let observations = rendered_observations(root.path());
        assert!(observations.len() >= 2, "{observations:?}");
        assert!(
            observations
                .iter()
                .any(|line| line.contains("$source_packet")),
            "{observations:?}"
        );

        let recipients = [
            (
                "schedule",
                prepared(
                    "schedule",
                    LOCATION_DAY,
                    json!({"hook":{"post":"schedule"}}),
                ),
            ),
            (
                "morning_briefing",
                prepared(
                    "morning_briefing",
                    LOCATION_DAY,
                    json!({
                        "hook":{"pre":"morning_briefing"},
                        "skip_reason":"no_input",
                        "messages":[{"role":"user","content":"briefing ordinary"}]
                    }),
                ),
            ),
            (
                "facet_newsletter",
                prepared(
                    "facet_newsletter",
                    LOCATION_DAY,
                    json!({
                        "facet":"work",
                        "hook":{"pre":"facet_newsletter","post":"facet_newsletter"},
                        "prompt":"$source_packet"
                    }),
                ),
            ),
            (
                "entities:entity_suggest",
                prepared(
                    "entities:entity_suggest",
                    LOCATION_DAY,
                    json!({
                        "facet":"work",
                        "hook":{"pre":"entities:entity_suggest","post":"entities:entity_suggest"}
                    }),
                ),
            ),
            (
                "entities:entity_observer",
                prepared(
                    "entities:entity_observer",
                    LOCATION_DAY,
                    json!({
                        "facet":"work",
                        "hook":{"pre":"entities:entity_observer","post":"entities:entity_observer"},
                        "skip_reason":"no_input"
                    }),
                ),
            ),
        ];
        for (hook, talent) in recipients {
            let ordinary = crate::ordinary_text_parts(&talent.config);
            assert!(!ordinary.is_empty(), "{hook}");
            let packet = freeze(talent, &context).unwrap();
            let config = packet["prepared"]["config"].as_object().unwrap();
            if hook == "facet_newsletter" {
                assert_eq!(
                    config.get("skip_reason").and_then(Value::as_str),
                    Some("no substantive facet/day sources")
                );
                assert_eq!(
                    config.get("prompt").and_then(Value::as_str),
                    Some("$source_packet")
                );
            }
            let fingerprint = config
                .get("_daily_location_fingerprint")
                .and_then(Value::as_str)
                .unwrap_or_else(|| panic!("{hook} missing fingerprint"));
            assert_eq!(fingerprint.len(), 64);
            assert!(config.get("_daily_location_omitted").is_none(), "{hook}");
            let text = config
                .get("_daily_location_text")
                .and_then(Value::as_str)
                .unwrap_or_else(|| panic!("{hook} missing location text"));
            assert!(text.contains(LOCATION_DAY), "{hook}");
            for line in &observations {
                assert!(text.contains(line.trim_end()), "{hook} lost {line}");
            }
            for coordinate in ["39.75", "105.2", "105.0"] {
                assert!(!text.contains(coordinate), "{hook} {coordinate}");
            }
            let thawed = thaw(&packet).unwrap().0;
            let parts = text_parts(&thawed);
            assert!(parts.len() > ordinary.len(), "{hook}");
            assert_eq!(&parts[..ordinary.len()], ordinary.as_slice(), "{hook}");
            assert_eq!(parts.last().map(String::as_str), Some(text), "{hook}");
            for part in &parts {
                for coordinate in ["39.75", "105.2", "105.0"] {
                    assert!(!part.contains(coordinate), "{hook}");
                }
            }
        }

        let excluded = [
            prepared(
                "daily_schedule",
                LOCATION_DAY,
                json!({"hook":{"pre":"daily_schedule","post":"daily_schedule"},"skip_reason":"no_input"}),
            ),
            prepared(
                "entities:entities_review",
                LOCATION_DAY,
                json!({
                    "facet":"work",
                    "hook":{"pre":"entities:entities_review","post":"entities:entities_review"},
                    "skip_reason":"no_input"
                }),
            ),
            prepared("schedule", "20260901", json!({"hook":{"post":"schedule"}})),
        ];
        for talent in excluded {
            let name = talent.name.clone();
            let packet = freeze(talent, &context).unwrap();
            let config = packet["prepared"]["config"].as_object().unwrap();
            assert!(
                config.get("_daily_location_fingerprint").is_none(),
                "{name}"
            );
            assert!(config.get("_daily_location_text").is_none(), "{name}");
            assert!(config.get("_daily_location_omitted").is_none(), "{name}");
        }

        let empty = prepared(
            "schedule",
            LOCATION_DAY,
            json!({"hook":{"post":"schedule"},"prompt":""}),
        );
        let packet = freeze(empty, &context).unwrap();
        let config = packet["prepared"]["config"].as_object().unwrap();
        assert_eq!(
            config
                .get("_daily_location_fingerprint")
                .and_then(Value::as_str)
                .map(str::len),
            Some(64)
        );
        assert!(config.get("_daily_location_text").is_none());
        let parts = text_parts(&thaw(&packet).unwrap().0);
        assert_eq!(parts, vec!["No input provided.".to_owned()]);

        write(
            root.path(),
            "config/journal.json",
            r#"{"identity":{"timezone":"America/Denver"},"daily_inputs":{"location":{"location-words-1":"not-a-day"}}}"#,
        );
        let malformed = freeze(
            prepared(
                "schedule",
                LOCATION_DAY,
                json!({"hook":{"post":"schedule"}}),
            ),
            &context,
        );
        assert!(malformed.is_err());
        let excluded_malformed = freeze(
            prepared(
                "daily_schedule",
                LOCATION_DAY,
                json!({"hook":{"pre":"daily_schedule","post":"daily_schedule"},"skip_reason":"no_input"}),
            ),
            &context,
        )
        .unwrap();
        assert!(
            excluded_malformed["prepared"]["config"]
                .get("_daily_location_fingerprint")
                .is_none()
        );
    }

    #[test]
    fn clip_location_part_drops_whole_observations_under_the_byte_cap() {
        let rendered = "# location\n\n- one\n- two\n- three\n";
        let (full, omitted) = clip_location_part("Label.", rendered, 10_000);
        assert_eq!(omitted, 0);
        let full = full.expect("all three lines fit");
        assert!(full.contains("- one\n") && full.contains("- three\n"));
        let (shrunk, omitted) = clip_location_part("Label.", rendered, full.len() - 1);
        assert_eq!(omitted, 1);
        let shrunk = shrunk.expect("two lines fit");
        assert!(shrunk.contains("- two\n"));
        assert!(!shrunk.contains("- three\n"));
        let (none, count) = clip_location_part("Label.", rendered, 1);
        assert!(none.is_none());
        assert_eq!(count, 3);
        let (none, count) = clip_location_part("Label.", "# heading only\n", 10_000);
        assert!(none.is_none());
        assert_eq!(count, 0);

        let root = tempfile::tempdir().unwrap();
        location_journal(root.path(), LOCATION_DAY);
        write_bytes(root.path(), "health/local.ctx", b"300");
        let observations = rendered_observations(root.path());
        assert!(!observations.is_empty());
        let context = ExecutionContext {
            journal: root.path().to_owned(),
        };
        let talent = prepared(
            "schedule",
            LOCATION_DAY,
            json!({"hook":{"post":"schedule"},"prompt":"keep me"}),
        );
        let packet = freeze(talent, &context).unwrap();
        let config = packet["prepared"]["config"].as_object().unwrap();
        assert_eq!(
            config.get("prompt").and_then(Value::as_str),
            Some("keep me")
        );
        assert!(config.get("_daily_location_fingerprint").is_some());
        assert!(config.get("_daily_location_text").is_none());
        assert_eq!(
            config
                .get("_daily_location_omitted")
                .and_then(Value::as_u64),
            Some(u64::try_from(observations.len()).unwrap())
        );
        let parts = text_parts(&thaw(&packet).unwrap().0);
        assert_eq!(parts, vec!["keep me".to_owned()]);
        for coordinate in ["39.75", "105.2", "105.0"] {
            assert!(!parts[0].contains(coordinate));
        }
    }
    #[cfg(feature = "full-tests")]
    #[test]
    fn capture_excludes_a_supported_place_name_aba() {
        let root = tempfile::tempdir().unwrap();
        location_journal(root.path(), LOCATION_DAY);
        let context = ExecutionContext {
            journal: root.path().to_owned(),
        };
        let before =
            solstone_core_location::daily_location_fingerprint(root.path(), LOCATION_DAY).unwrap();
        let journal = root.path().to_path_buf();
        let worker = std::rc::Rc::new(std::cell::RefCell::new(None));
        let saved_worker = worker.clone();
        CAPTURE_BARRIER.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move || {
                let (sent, received) = std::sync::mpsc::channel();
                let handle = std::thread::spawn(move || {
                    let original =
                        solstone_core_location::places::PlaceStore::update(&journal, |store| {
                            store.places[0].name.replace("Transient name".into())
                        })
                        .unwrap();
                    solstone_core_location::places::PlaceStore::update(&journal, |store| {
                        store.places[0].name = original;
                    })
                    .unwrap();
                    // The probe receiver ends before capture releases admission.
                    let _ = sent.send(());
                });
                *saved_worker.borrow_mut() = Some(handle);
                assert!(
                    matches!(
                        received.recv_timeout(std::time::Duration::from_millis(100)),
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                    ),
                    "supported ABA passed through capture"
                );
            }))
        });
        let mut talent = prepared("schedule", LOCATION_DAY, json!({}));
        attach_daily_location(&mut talent, &context, "schedule").unwrap();
        worker.borrow_mut().take().unwrap().join().unwrap();
        assert_eq!(
            solstone_core_location::daily_location_fingerprint(root.path(), LOCATION_DAY).unwrap(),
            before
        );
        assert_eq!(talent.config["_daily_location_fingerprint"], before);
        assert!(
            !talent.config["_daily_location_text"]
                .as_str()
                .unwrap()
                .contains("Transient name")
        );
    }
    #[test]
    fn a_large_whole_first_observation_that_fits_is_not_discarded() {
        let root = tempfile::tempdir().unwrap();
        let context = ExecutionContext {
            journal: root.path().to_owned(),
        };
        let talent = prepared("schedule", LOCATION_DAY, json!({}));
        let rendered = format!("# location\n\n- {}\n", "長".repeat(3500));
        let (part, omitted) = fit_location_part(
            "schedule",
            LOCATION_DAY,
            &rendered,
            &["ordinary".into()],
            &talent,
            &context,
        );
        let part = part.expect("a complete observation fits the 16 KiB and request budgets");
        assert!(part.len() <= LOCATION_PART_BYTE_CAP);
        assert!(part.ends_with(&format!("- {}\n", "長".repeat(3500))));
        assert_eq!(omitted, 0);
    }
}
