// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Home getting-started first-minutes pure projection and reader.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{Offset, TimeDelta, TimeZone};
use chrono_tz::Tz;
use serde_json::Value;

use crate::HomeContext;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoundStream {
    pub name: String,
    pub cid: Option<String>,
    pub platform_allocation: Option<String>,
}

#[derive(Clone, Debug)]
pub struct SegmentCandidate {
    pub day: String,
    pub key: String,
    pub stream: String,
    pub data_state: solstone_core_system_health::DataStateMap,
    pub reported_tz: Option<String>,
    pub reported_offset_seconds: Option<i32>,
}

#[derive(Clone, Debug)]
pub struct ProgressView {
    pub progress: BTreeMap<
        solstone_core_system_health::SegmentIdentity,
        solstone_core_system_health::SegmentProgress,
    >,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WinningSegment {
    pub day: String,
    pub key: String,
    pub stream: String,
    pub start_at: String,
    pub candidate_index: usize,
}

pub struct FirstMinutesInput {
    pub now_ms: i64,
    pub owner_zone: Tz,
    pub capture: Value,
    pub reported_platforms: Option<BTreeMap<String, Option<String>>>,
    pub ledger_platforms: BTreeMap<String, String>,
    pub bound_streams: Option<Vec<BoundStream>>,
    pub segments: Option<Vec<SegmentCandidate>>,
    pub earliest_progress: Option<ProgressView>,
    pub brain_state: String,
    pub brain_reason: Option<String>,
    pub brain_progressing: bool,
    pub ui_lane: Option<String>,
}

fn encode_path_component(val: &str) -> String {
    let mut encoded = String::new();
    for b in val.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-') {
            encoded.push(b as char);
        } else {
            encoded.push_str(&format!("%{:02X}", b));
        }
    }
    encoded
}

fn segment_wall_start(day: &str, key: &str) -> Option<chrono::NaiveDateTime> {
    let hhmmss = key.get(..6)?;
    let digits =
        |text: &str, len: usize| text.len() == len && text.bytes().all(|b| b.is_ascii_digit());
    if !digits(day, 8) || !digits(hhmmss, 6) {
        return None;
    }
    chrono::NaiveDateTime::parse_from_str(&format!("{day}{hhmmss}"), "%Y%m%d%H%M%S").ok()
}

pub fn winning_segment(input: &FirstMinutesInput) -> Option<WinningSegment> {
    let segments = input.segments.as_ref()?;
    let mut winner: Option<(chrono::DateTime<chrono::Utc>, usize)> = None;

    for (idx, candidate) in segments.iter().enumerate() {
        let wall = match segment_wall_start(&candidate.day, &candidate.key) {
            Some(w) => w,
            None => continue,
        };
        let offset_seconds = if let Some(view) = solstone_core_journal_config::capture_zone_view(
            &candidate.day,
            &candidate.key,
            candidate.reported_tz.as_deref(),
            candidate.reported_offset_seconds,
            &input.owner_zone,
        ) {
            view.utc_offset_seconds
        } else {
            match input
                .owner_zone
                .offset_from_local_datetime(&wall)
                .earliest()
            {
                Some(offset) => offset.fix().local_minus_utc(),
                None => continue,
            }
        };
        let utc_naive = wall - TimeDelta::seconds(i64::from(offset_seconds));
        let instant = chrono::DateTime::from_naive_utc_and_offset(utc_naive, chrono::Utc);

        match &winner {
            Some((best_instant, best_idx)) => {
                if instant < *best_instant || (instant == *best_instant && idx < *best_idx) {
                    winner = Some((instant, idx));
                }
            }
            None => {
                winner = Some((instant, idx));
            }
        }
    }

    winner.map(|(instant, idx)| {
        let cand = &segments[idx];
        WinningSegment {
            day: cand.day.clone(),
            key: cand.key.clone(),
            stream: cand.stream.clone(),
            start_at: instant.to_rfc3339(),
            candidate_index: idx,
        }
    })
}

pub fn project_first_minutes(input: &FirstMinutesInput) -> Value {
    let winner = winning_segment(input);

    let devices_known =
        input.capture.get("registry").and_then(Value::as_str) != Some("registry_unknown");

    let (devices_json, checks_phone) = if !devices_known {
        (serde_json::json!([]), Value::Null)
    } else {
        struct RawDevice {
            name: String,
            platform: Option<String>,
            state: &'static str,
            last_added_label: Option<String>,
            parsed_time: Option<i64>,
            original_index: usize,
        }

        let mut raw_devices = Vec::new();
        let mut idx = 0;

        if let Some(clients) = input.capture.get("clients").and_then(Value::as_array) {
            for c in clients {
                let name = c
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let cid = c.get("cid").and_then(Value::as_str).unwrap_or("");
                let status = c.get("status").and_then(Value::as_str);
                let state = match status {
                    Some("active") => "receiving",
                    Some("stale") | Some("offline") => "quiet",
                    Some("awaiting_first_delivery") => "awaiting_first",
                    _ => "other",
                };

                let reported_platform = input
                    .reported_platforms
                    .as_ref()
                    .and_then(|m| m.get(cid).and_then(|p| p.as_deref()));
                let ledger_platform = input
                    .ledger_platforms
                    .get(cid)
                    .and_then(|w| solstone_core_sol_link::Platform::from_wire(w));
                let allocation_input = input.bound_streams.as_ref().and_then(|streams| {
                    streams
                        .iter()
                        .find(|s| s.cid.as_deref() == Some(cid) && s.platform_allocation.is_some())
                        .and_then(|s| s.platform_allocation.as_deref())
                });
                let platform = crate::provenance::resolve_client_platform(
                    reported_platform,
                    ledger_platform,
                    allocation_input,
                );

                let parsed_time = c
                    .get("last_accepted_ingest_at")
                    .and_then(Value::as_str)
                    .and_then(|ts| chrono::DateTime::parse_from_rfc3339(ts).ok())
                    .map(|dt| dt.timestamp_millis());

                let last_added_label = parsed_time.map(|delivery_ms| {
                    let seconds = ((input.now_ms - delivery_ms) / 1000) as f64;
                    format!("{} ago", crate::formatting::relative_time(seconds))
                });

                raw_devices.push(RawDevice {
                    name,
                    platform,
                    state,
                    last_added_label,
                    parsed_time,
                    original_index: idx,
                });
                idx += 1;
            }
        }

        if let Some(unassessed) = input.capture.get("unassessed").and_then(Value::as_array) {
            for c in unassessed {
                let name = c
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let cid = c.get("cid").and_then(Value::as_str).unwrap_or("");
                let reason = c.get("reason").and_then(Value::as_str);
                let state = match reason {
                    Some("awaiting_first_delivery") => "awaiting_first",
                    _ => "other",
                };

                let reported_platform = input
                    .reported_platforms
                    .as_ref()
                    .and_then(|m| m.get(cid).and_then(|p| p.as_deref()));
                let ledger_platform = input
                    .ledger_platforms
                    .get(cid)
                    .and_then(|w| solstone_core_sol_link::Platform::from_wire(w));
                let allocation_input = input.bound_streams.as_ref().and_then(|streams| {
                    streams
                        .iter()
                        .find(|s| s.cid.as_deref() == Some(cid) && s.platform_allocation.is_some())
                        .and_then(|s| s.platform_allocation.as_deref())
                });
                let platform = crate::provenance::resolve_client_platform(
                    reported_platform,
                    ledger_platform,
                    allocation_input,
                );

                let parsed_time = c
                    .get("last_accepted_ingest_at")
                    .and_then(Value::as_str)
                    .and_then(|ts| chrono::DateTime::parse_from_rfc3339(ts).ok())
                    .map(|dt| dt.timestamp_millis());

                let last_added_label = parsed_time.map(|delivery_ms| {
                    let seconds = ((input.now_ms - delivery_ms) / 1000) as f64;
                    format!("{} ago", crate::formatting::relative_time(seconds))
                });

                raw_devices.push(RawDevice {
                    name,
                    platform,
                    state,
                    last_added_label,
                    parsed_time,
                    original_index: idx,
                });
                idx += 1;
            }
        }

        raw_devices.sort_by(|a, b| match (a.parsed_time, b.parsed_time) {
            (Some(ta), Some(tb)) => tb
                .cmp(&ta)
                .then_with(|| a.original_index.cmp(&b.original_index)),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => a.original_index.cmp(&b.original_index),
        });

        let has_phone = raw_devices.iter().any(|d| {
            d.platform.as_deref() == Some("ios") || d.platform.as_deref() == Some("android")
        });

        let devices_array: Vec<Value> = raw_devices
            .into_iter()
            .map(|d| {
                let mut obj = serde_json::json!({
                    "name": d.name,
                    "platform": d.platform,
                    "state": d.state,
                });
                if let Some(lbl) = d.last_added_label {
                    obj["last_added_label"] = Value::String(lbl);
                }
                obj
            })
            .collect();

        (Value::Array(devices_array), Value::Bool(has_phone))
    };

    let (thinking_state, thinking_step) = if input.brain_state == "ready" {
        ("ready", "none")
    } else if input.brain_state == "checking"
        || (input.brain_state == "blocked" && input.brain_progressing)
    {
        ("progressing", "none")
    } else if input.brain_reason.as_deref() == Some("thinking_engine_not_chosen") {
        ("not_chosen", "choose")
    } else if input.brain_reason.as_deref() == Some("gpu_unavailable") {
        ("cant_run_local", "cant_run")
    } else if input.brain_state == "blocked"
        && !input.brain_progressing
        && matches!(
            input.brain_reason.as_deref(),
            Some(
                "provider_key_missing"
                    | "endpoint_configuration_incomplete"
                    | "local_artifact_not_ready"
            )
        )
    {
        let step = if input.brain_reason.as_deref() == Some("local_artifact_not_ready") {
            "local"
        } else if input.ui_lane.as_deref() == Some("confidential") {
            "confidential"
        } else {
            "byo"
        };
        ("needs_step", step)
    } else {
        ("other", "none")
    };

    let check_thinking = thinking_step == "none";

    let first_segment = match (input.segments.as_ref(), winner) {
        (Some(segs), Some(w)) => {
            let cand = &segs[w.candidate_index];
            let seg_input = solstone_core_system_health::SegmentInput {
                key: cand.key.clone(),
                stream: cand.stream.clone(),
                data_state: cand.data_state.clone(),
            };
            let thinking_val =
                if !solstone_core_system_health::segment_requires_processing(&seg_input)
                    || !solstone_core_system_health::segment_fully_sensed(&cand.data_state)
                {
                    "none"
                } else if let Some(pv) = &input.earliest_progress {
                    let progress = solstone_core_system_health::lookup_segment_progress(
                        &pv.progress,
                        &cand.stream,
                        &cand.key,
                    );
                    if solstone_core_system_health::segment_fully_thought(progress)
                        == solstone_core_system_health::ThoughtVerdict::Complete
                    {
                        "read"
                    } else if thinking_state == "ready" || thinking_state == "progressing" {
                        "pending"
                    } else {
                        "none"
                    }
                } else {
                    "none"
                };

            serde_json::json!({
                "start_at": w.start_at,
                "href": format!(
                    "/app/transcripts/{}?stream={}#{}",
                    encode_path_component(&w.day),
                    encode_path_component(&w.stream),
                    encode_path_component(&w.key),
                ),
                "thinking": thinking_val,
            })
        }
        _ => Value::Null,
    };

    serde_json::json!({
        "window_open": true,
        "devices_known": devices_known,
        "devices": devices_json,
        "first_segment": first_segment,
        "checks": {
            "phone": checks_phone,
            "thinking": check_thinking,
        },
        "thinking": {
            "state": thinking_state,
            "lane": input.ui_lane,
            "step": thinking_step,
        }
    })
}

pub fn read_first_minutes(context: &HomeContext) -> Value {
    let config_path = context.journal_root().join("config/journal.json");
    let Ok(bytes) = std::fs::read(&config_path) else {
        return serde_json::json!({"window_open": false});
    };
    let Ok(value): Result<Value, _> = serde_json::from_slice(&bytes) else {
        return serde_json::json!({"window_open": false});
    };
    let completed_at = value
        .get("setup")
        .and_then(|s| s.get("completed_at"))
        .and_then(|v| {
            v.as_i64()
                .or_else(|| v.as_u64().and_then(|u| i64::try_from(u).ok()))
        });
    let Some(completed_at) = completed_at else {
        return serde_json::json!({"window_open": false});
    };
    let elapsed = context.now_ms() - completed_at;
    if elapsed >= 604_800_000 {
        return serde_json::json!({"window_open": false});
    }

    let capture = crate::readers::get_capture_health(context);

    let reported_platforms =
        match solstone_core_sol_link::client_description_store::read_descriptions(
            context.journal_root(),
        ) {
            Ok(descriptions) => {
                let map: BTreeMap<String, Option<String>> = descriptions
                    .into_iter()
                    .map(|(cid, desc)| (cid, desc.reported.and_then(|r| r.platform)))
                    .collect();
                Some(map)
            }
            Err(_) => None,
        };

    let ledger_path = context.journal_root().join("link/authorized_clients.json");
    let ledger_platforms: BTreeMap<String, String> =
        match solstone_core_sol_link::ledger::read_authorized_clients(&ledger_path) {
            solstone_core_sol_link::ledger::AuthorizedClientsRead::Present(clients) => clients
                .into_iter()
                .filter_map(|e| e.platform.map(|p| (e.fingerprint, p.as_wire().to_string())))
                .collect(),
            _ => BTreeMap::new(),
        };

    let bound_streams = match solstone_core_segment::list_stream_records(context.journal_root()) {
        Ok(records) => {
            let streams: Vec<BoundStream> = records
                .into_iter()
                .map(|(name, record)| {
                    let platform_allocation = record
                        .allocation
                        .as_ref()
                        .filter(|a| a.base == solstone_core_segment::StreamAllocationBase::Platform)
                        .map(|a| a.base_input.clone());
                    BoundStream {
                        name,
                        cid: record.cid,
                        platform_allocation,
                    }
                })
                .collect();
            Some(streams)
        }
        Err(_) => None,
    };

    let mut paired_cids = BTreeSet::new();
    if let Some(clients) = capture.get("clients").and_then(Value::as_array) {
        for c in clients {
            if let Some(cid) = c.get("cid").and_then(Value::as_str) {
                paired_cids.insert(cid.to_string());
            }
        }
    }
    if let Some(unassessed) = capture.get("unassessed").and_then(Value::as_array) {
        for c in unassessed {
            if let Some(cid) = c.get("cid").and_then(Value::as_str) {
                paired_cids.insert(cid.to_string());
            }
        }
    }

    let bound_stream_names: BTreeSet<String> = match &bound_streams {
        Some(streams) => streams
            .iter()
            .filter(|s| s.cid.as_ref().is_some_and(|cid| paired_cids.contains(cid)))
            .map(|s| s.name.clone())
            .collect(),
        None => BTreeSet::new(),
    };

    let days_result = solstone_core_segment::list_days(context.journal_root());
    let segments = match days_result {
        Ok(days) => {
            let mut candidates = Vec::new();
            let mut walk_ok = true;
            for (day, _) in days {
                match solstone_core_system_health::scan_day(
                    &solstone_core_system_health::FilesystemSegmentSource,
                    context.journal_root(),
                    &day,
                    context.now_utc,
                ) {
                    Ok((_, _, segs)) => {
                        for seg in segs {
                            if bound_stream_names.contains(&seg.stream) {
                                let (reported_tz, reported_offset_seconds) =
                                    if let Some(segment_dir) =
                                        solstone_core_system_health::find_segment_dir(
                                            context.journal_root(),
                                            &day,
                                            &seg.key,
                                            Some(&seg.stream),
                                        )
                                    {
                                        if let Some(zone) =
                                            solstone_core_callosum::read_reported_zone(&segment_dir)
                                        {
                                            (zone.tz, zone.utc_offset_seconds)
                                        } else {
                                            (None, None)
                                        }
                                    } else {
                                        (None, None)
                                    };

                                candidates.push(SegmentCandidate {
                                    day: day.clone(),
                                    key: seg.key,
                                    stream: seg.stream,
                                    data_state: seg.data_state,
                                    reported_tz,
                                    reported_offset_seconds,
                                });
                            }
                        }
                    }
                    Err(_) => {
                        walk_ok = false;
                        break;
                    }
                }
            }
            if walk_ok { Some(candidates) } else { None }
        }
        Err(_) => None,
    };

    let brain_snapshot = crate::readers::build_brain_snapshot(context);
    let brain_state = brain_snapshot["state"]
        .as_str()
        .unwrap_or("unknown")
        .to_string();
    let brain_reason = brain_snapshot["reason_code"]
        .as_str()
        .map(|s| s.to_string());
    let brain_progressing = brain_snapshot["progressing"].as_bool().unwrap_or(false);

    let ui_lane = match solstone_core_thinking::read_config(context.journal_root()) {
        Ok(config) => {
            let lane = solstone_core_thinking::providers::ui_lane(&config);
            if lane == "none" {
                None
            } else {
                Some(lane.to_string())
            }
        }
        Err(_) => None,
    };

    let mut input = FirstMinutesInput {
        now_ms: context.now_ms(),
        owner_zone: context.zone(),
        capture,
        reported_platforms,
        ledger_platforms,
        bound_streams,
        segments,
        earliest_progress: None,
        brain_state,
        brain_reason,
        brain_progressing,
        ui_lane,
    };

    let winner = winning_segment(&input);
    if let Some(w) = &winner {
        let log_source =
            solstone_core_system_health::FilesystemHealthLogSource::new(context.journal_root());
        let progress = solstone_core_system_health::read_segment_progress(&log_source, &w.day)
            .ok()
            .map(|fold| ProgressView {
                progress: fold.value,
            });
        input.earliest_progress = progress;
    }

    project_first_minutes(&input)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono_tz::Tz;
    use serde_json::json;
    use solstone_core_system_health::DataStateMap;

    #[test]
    fn devices_unknown_when_registry_unknown() {
        let input = FirstMinutesInput {
            now_ms: 1_000_000,
            owner_zone: Tz::UTC,
            capture: json!({
                "registry": "registry_unknown",
                "clients": [],
                "unassessed": [],
            }),
            reported_platforms: Some(BTreeMap::new()),
            ledger_platforms: BTreeMap::new(),
            bound_streams: Some(vec![]),
            segments: Some(vec![]),
            earliest_progress: None,
            brain_state: "ready".into(),
            brain_reason: None,
            brain_progressing: false,
            ui_lane: None,
        };
        let out = project_first_minutes(&input);
        assert_eq!(out["window_open"], true);
        assert_eq!(out["devices_known"], false);
        assert_eq!(out["devices"], json!([]));
        assert_eq!(out["checks"]["phone"], Value::Null);
    }

    #[test]
    fn devices_state_order_labels_and_platforms() {
        let mut reported = BTreeMap::new();
        reported.insert("cid_a".into(), Some("ios".into()));
        reported.insert("cid_b".into(), None);

        let mut ledger = BTreeMap::new();
        ledger.insert("cid_b".into(), "android".into());

        let bound = vec![BoundStream {
            name: "stream_c".into(),
            cid: Some("cid_c".into()),
            platform_allocation: Some("macos".into()),
        }];

        let capture = json!({
            "registry": "registry_complete",
            "clients": [
                {
                    "name": "Device A",
                    "cid": "cid_a",
                    "status": "active",
                    "last_accepted_ingest_at": "2026-09-29T12:00:00Z"
                },
                {
                    "name": "Device B",
                    "cid": "cid_b",
                    "status": "stale",
                    "last_accepted_ingest_at": "2026-09-29T12:05:00Z"
                },
                {
                    "name": "Device C",
                    "cid": "cid_c",
                    "status": "degraded"
                }
            ],
            "unassessed": [
                {
                    "name": "Device D",
                    "cid": "cid_d",
                    "reason": "awaiting_first_delivery"
                }
            ]
        });

        // 2026-09-29T12:10:00Z in ms is 1790683800000
        let now_dt = chrono::DateTime::parse_from_rfc3339("2026-09-29T12:10:00Z").unwrap();
        let now_ms = now_dt.timestamp_millis();

        let input = FirstMinutesInput {
            now_ms,
            owner_zone: Tz::UTC,
            capture,
            reported_platforms: Some(reported),
            ledger_platforms: ledger,
            bound_streams: Some(bound),
            segments: Some(vec![]),
            earliest_progress: None,
            brain_state: "ready".into(),
            brain_reason: None,
            brain_progressing: false,
            ui_lane: None,
        };

        let out = project_first_minutes(&input);
        assert_eq!(out["devices_known"], true);
        assert_eq!(out["checks"]["phone"], true);

        let devs = out["devices"].as_array().unwrap();
        assert_eq!(devs.len(), 4);
        // B (12:05) comes before A (12:00) because 12:05 > 12:00
        assert_eq!(devs[0]["name"], "Device B");
        assert_eq!(devs[0]["platform"], "android");
        assert_eq!(devs[0]["state"], "quiet");
        assert_eq!(devs[0]["last_added_label"], "5 minutes ago");

        assert_eq!(devs[1]["name"], "Device A");
        assert_eq!(devs[1]["platform"], "ios");
        assert_eq!(devs[1]["state"], "receiving");
        assert_eq!(devs[1]["last_added_label"], "10 minutes ago");

        // C has no time, D has no time; clients order first (C), then unassessed (D)
        assert_eq!(devs[2]["name"], "Device C");
        assert_eq!(devs[2]["platform"], "macos");
        assert_eq!(devs[2]["state"], "other");
        assert!(devs[2].get("last_added_label").is_none());

        assert_eq!(devs[3]["name"], "Device D");
        assert_eq!(devs[3]["state"], "awaiting_first");
        assert!(devs[3].get("last_added_label").is_none());
    }

    #[test]
    fn winning_segment_choice_and_thinking_verdicts() {
        let mut data_state_map = BTreeMap::new();
        data_state_map.insert("audio".into(), "analyzed".into());
        let sensed_state = DataStateMap(data_state_map);

        let candidate_early = SegmentCandidate {
            day: "20260929".into(),
            key: "090000_300".into(),
            stream: "stream/one".into(),
            data_state: sensed_state.clone(),
            reported_tz: Some("UTC".into()),
            reported_offset_seconds: Some(0),
        };
        let candidate_late = SegmentCandidate {
            day: "20260929".into(),
            key: "100000_300".into(),
            stream: "stream/two".into(),
            data_state: sensed_state.clone(),
            reported_tz: Some("UTC".into()),
            reported_offset_seconds: Some(0),
        };

        let mut progress_map = BTreeMap::new();
        let ident = solstone_core_system_health::SegmentIdentity {
            stream: Some("stream/one".into()),
            segment: "090000_300".into(),
        };
        let progress = solstone_core_system_health::SegmentProgress {
            sensed: true,
            density: Some("idle".into()),
            ..Default::default()
        };
        progress_map.insert(ident.clone(), progress);

        let mut input = FirstMinutesInput {
            now_ms: 1_790_683_800_000,
            owner_zone: Tz::UTC,
            capture: json!({"registry": "registry_complete", "clients": [], "unassessed": []}),
            reported_platforms: Some(BTreeMap::new()),
            ledger_platforms: BTreeMap::new(),
            bound_streams: Some(vec![]),
            segments: Some(vec![candidate_late.clone(), candidate_early.clone()]),
            earliest_progress: Some(ProgressView {
                progress: progress_map.clone(),
            }),
            brain_state: "ready".into(),
            brain_reason: None,
            brain_progressing: false,
            ui_lane: None,
        };

        let out = project_first_minutes(&input);
        assert_eq!(
            out["first_segment"]["start_at"],
            "2026-09-29T09:00:00+00:00"
        );
        assert_eq!(
            out["first_segment"]["href"],
            "/app/transcripts/20260929?stream=stream%2Fone#090000_300"
        );
        assert_eq!(out["first_segment"]["thinking"], "read");

        // When not fully thought, and brain is ready -> pending
        let pending_progress = solstone_core_system_health::SegmentProgress {
            sensed: true,
            ..Default::default()
        };
        let mut pending_map = BTreeMap::new();
        pending_map.insert(ident, pending_progress);
        input.earliest_progress = Some(ProgressView {
            progress: pending_map,
        });
        let out_pending = project_first_minutes(&input);
        assert_eq!(out_pending["first_segment"]["thinking"], "pending");

        // When not fully thought and brain is other -> none
        input.brain_state = "blocked".into();
        input.brain_reason = Some("thinking_engine_not_chosen".into());
        let out_none = project_first_minutes(&input);
        assert_eq!(out_none["first_segment"]["thinking"], "none");
    }

    #[test]
    fn thinking_state_and_step_mappings() {
        // 1. ready
        let mut input = FirstMinutesInput {
            now_ms: 1_000_000,
            owner_zone: Tz::UTC,
            capture: json!({"registry": "registry_unknown"}),
            reported_platforms: None,
            ledger_platforms: BTreeMap::new(),
            bound_streams: None,
            segments: None,
            earliest_progress: None,
            brain_state: "ready".into(),
            brain_reason: None,
            brain_progressing: false,
            ui_lane: None,
        };
        let out = project_first_minutes(&input);
        assert_eq!(out["thinking"]["state"], "ready");
        assert_eq!(out["thinking"]["step"], "none");
        assert_eq!(out["checks"]["thinking"], true);

        // 2. not chosen
        input.brain_state = "blocked".into();
        input.brain_reason = Some("thinking_engine_not_chosen".into());
        let out = project_first_minutes(&input);
        assert_eq!(out["thinking"]["state"], "not_chosen");
        assert_eq!(out["thinking"]["step"], "choose");
        assert_eq!(out["checks"]["thinking"], false);

        // 3. cant run local
        input.brain_reason = Some("gpu_unavailable".into());
        let out = project_first_minutes(&input);
        assert_eq!(out["thinking"]["state"], "cant_run_local");
        assert_eq!(out["thinking"]["step"], "cant_run");
        assert_eq!(out["checks"]["thinking"], false);

        // 4. needs step: local
        input.brain_reason = Some("local_artifact_not_ready".into());
        let out = project_first_minutes(&input);
        assert_eq!(out["thinking"]["state"], "needs_step");
        assert_eq!(out["thinking"]["step"], "local");
        assert_eq!(out["checks"]["thinking"], false);

        // 5. needs step: confidential
        input.brain_reason = Some("provider_key_missing".into());
        input.ui_lane = Some("confidential".into());
        let out = project_first_minutes(&input);
        assert_eq!(out["thinking"]["state"], "needs_step");
        assert_eq!(out["thinking"]["step"], "confidential");
        assert_eq!(out["checks"]["thinking"], false);

        // 6. needs step: byo
        input.ui_lane = Some("byo".into());
        let out = project_first_minutes(&input);
        assert_eq!(out["thinking"]["state"], "needs_step");
        assert_eq!(out["thinking"]["step"], "byo");
        assert_eq!(out["checks"]["thinking"], false);

        // 7. fallback snapshot
        input.brain_state = "unknown".into();
        input.brain_reason = Some("brain_record_unavailable".into());
        input.brain_progressing = false;
        input.ui_lane = None;
        let out = project_first_minutes(&input);
        assert_eq!(out["thinking"]["state"], "other");
        assert_eq!(out["thinking"]["step"], "none");
        assert_eq!(out["checks"]["thinking"], true);
    }

    #[test]
    fn reader_closed_window_proof() {
        let temp = tempfile::tempdir_in("/var/tmp").unwrap();
        let journal = temp.path();

        // 1. config/journal.json: {"identity":{"timezone":"UTC"},"setup":{"completed_at":1000000000000}}
        let completed_at = 1_000_000_000_000i64;
        std::fs::create_dir_all(journal.join("config")).unwrap();
        std::fs::write(
            journal.join("config/journal.json"),
            json!({
                "identity": {"timezone": "UTC"},
                "setup": {"completed_at": completed_at}
            })
            .to_string(),
        )
        .unwrap();

        // 2. link/authorized_clients.json
        std::fs::create_dir_all(journal.join("link")).unwrap();
        let ledger = json!([{
            "fingerprint": "sha256:phone",
            "device_label": "Pocket",
            "paired_at": "2026-10-10T00:00:00Z",
            "instance_id": "inst",
            "role": "",
            "kind": "cert",
            "platform": "ios"
        }]);
        std::fs::write(
            journal.join("link/authorized_clients.json"),
            ledger.to_string(),
        )
        .unwrap();

        // 3. streams/phone.json
        std::fs::create_dir_all(journal.join("streams")).unwrap();
        let stream = json!({
            "name": "phone",
            "kind": "observer",
            "host": null,
            "platform": null,
            "created_at": 1000000000000u64,
            "last_day": null,
            "last_segment": null,
            "seq": 0,
            "cid": "sha256:phone",
            "source": ""
        });
        std::fs::write(journal.join("streams/phone.json"), stream.to_string()).unwrap();

        // 4. chronicle/20261010/phone/120000_300/audio.jsonl
        let seg_dir = journal.join("chronicle/20261010/phone/120000_300");
        std::fs::create_dir_all(&seg_dir).unwrap();
        std::fs::write(
            seg_dir.join("audio.jsonl"),
            "{\"start\":\"12:00:00\",\"text\":\"hello\"}\n",
        )
        .unwrap();

        // Closed read: elapsed >= 604_800_000
        let now_closed_exact = completed_at + 604_800_000;
        let ctx_closed_exact = HomeContext::new(
            journal,
            chrono::DateTime::from_timestamp_millis(now_closed_exact).unwrap(),
        );
        assert_eq!(
            read_first_minutes(&ctx_closed_exact),
            json!({"window_open": false})
        );

        let now_closed_more = completed_at + 700_000_000;
        let ctx_closed_more = HomeContext::new(
            journal,
            chrono::DateTime::from_timestamp_millis(now_closed_more).unwrap(),
        );
        assert_eq!(
            read_first_minutes(&ctx_closed_more),
            json!({"window_open": false})
        );

        // Missing completed_at on copy of journal
        let temp_missing = tempfile::tempdir_in("/var/tmp").unwrap();
        let journal_missing = temp_missing.path();
        std::fs::create_dir_all(journal_missing.join("config")).unwrap();
        std::fs::write(
            journal_missing.join("config/journal.json"),
            json!({"identity": {"timezone": "UTC"}, "setup": {}}).to_string(),
        )
        .unwrap();
        let ctx_missing = HomeContext::new(
            journal_missing,
            chrono::DateTime::from_timestamp_millis(completed_at + 86_400_000).unwrap(),
        );
        assert_eq!(
            read_first_minutes(&ctx_missing),
            json!({"window_open": false})
        );

        // Non-numeric completed_at on copy of journal
        let temp_nonnum = tempfile::tempdir_in("/var/tmp").unwrap();
        let journal_nonnum = temp_nonnum.path();
        std::fs::create_dir_all(journal_nonnum.join("config")).unwrap();
        std::fs::write(
            journal_nonnum.join("config/journal.json"),
            json!({"identity": {"timezone": "UTC"}, "setup": {"completed_at": "yesterday"}})
                .to_string(),
        )
        .unwrap();
        let ctx_nonnum = HomeContext::new(
            journal_nonnum,
            chrono::DateTime::from_timestamp_millis(completed_at + 86_400_000).unwrap(),
        );
        assert_eq!(
            read_first_minutes(&ctx_nonnum),
            json!({"window_open": false})
        );

        // Open read: completed_at + 86_400_000
        let now_open_ms = completed_at + 86_400_000;
        let ctx_open = HomeContext::new(
            journal,
            chrono::DateTime::from_timestamp_millis(now_open_ms).unwrap(),
        );
        let out_open = read_first_minutes(&ctx_open);
        assert_eq!(out_open["window_open"], true);
        assert_eq!(out_open["devices"][0]["name"], "Pocket");
        assert_eq!(
            out_open["first_segment"]["href"],
            "/app/transcripts/20261010?stream=phone#120000_300"
        );
        if ctx_open.zone() == Tz::UTC {
            assert_eq!(
                out_open["first_segment"]["start_at"],
                "2026-10-10T12:00:00+00:00"
            );
        }
    }

    #[test]
    fn unassessed_activity_unavailable_and_registry_complete() {
        let input = FirstMinutesInput {
            now_ms: 1_000_000,
            owner_zone: Tz::UTC,
            capture: json!({
                "status": "unknown",
                "registry": "registry_complete",
                "clients": [],
                "unassessed": [{
                    "name": "Dev",
                    "cid": "c1",
                    "reason": "activity_unavailable",
                }]
            }),
            reported_platforms: None,
            ledger_platforms: BTreeMap::new(),
            bound_streams: None,
            segments: None,
            earliest_progress: None,
            brain_state: "ready".into(),
            brain_reason: None,
            brain_progressing: false,
            ui_lane: None,
        };
        let out = project_first_minutes(&input);
        assert_eq!(out["devices_known"], true);
        let devs = out["devices"].as_array().unwrap();
        assert_eq!(devs.len(), 1);
        assert_eq!(devs[0]["name"], "Dev");
        assert_eq!(devs[0]["state"], "other");
    }

    #[test]
    fn client_platform_resolution_and_phone_check() {
        // reported platform "iOS" on awaiting_first client with no bound stream
        let mut reported = BTreeMap::new();
        reported.insert("c_ios".into(), Some("iOS".into()));
        let input = FirstMinutesInput {
            now_ms: 1_000_000,
            owner_zone: Tz::UTC,
            capture: json!({
                "registry": "registry_complete",
                "clients": [{
                    "name": "Phone",
                    "cid": "c_ios",
                    "status": "awaiting_first_delivery",
                }],
                "unassessed": []
            }),
            reported_platforms: Some(reported),
            ledger_platforms: BTreeMap::new(),
            bound_streams: None,
            segments: None,
            earliest_progress: None,
            brain_state: "ready".into(),
            brain_reason: None,
            brain_progressing: false,
            ui_lane: None,
        };
        let out = project_first_minutes(&input);
        assert_eq!(out["checks"]["phone"], true);
        assert_eq!(out["devices"][0]["platform"], "ios");
        assert_eq!(out["devices"][0]["state"], "awaiting_first");

        // ledger-only "ios" with no stream does the same
        let mut ledger = BTreeMap::new();
        ledger.insert("c_ledger".into(), "ios".into());
        let input_ledger = FirstMinutesInput {
            now_ms: 1_000_000,
            owner_zone: Tz::UTC,
            capture: json!({
                "registry": "registry_complete",
                "clients": [{
                    "name": "Phone2",
                    "cid": "c_ledger",
                    "status": "awaiting_first_delivery",
                }],
                "unassessed": []
            }),
            reported_platforms: Some(BTreeMap::new()),
            ledger_platforms: ledger,
            bound_streams: None,
            segments: None,
            earliest_progress: None,
            brain_state: "ready".into(),
            brain_reason: None,
            brain_progressing: false,
            ui_lane: None,
        };
        let out_ledger = project_first_minutes(&input_ledger);
        assert_eq!(out_ledger["checks"]["phone"], true);
        assert_eq!(out_ledger["devices"][0]["platform"], "ios");
        assert_eq!(out_ledger["devices"][0]["state"], "awaiting_first");

        // Desktops (macos) and device with no platform -> checks.phone: false
        let mut ledger_desktop = BTreeMap::new();
        ledger_desktop.insert("c_mac".into(), "macos".into());
        let input_desktop = FirstMinutesInput {
            now_ms: 1_000_000,
            owner_zone: Tz::UTC,
            capture: json!({
                "registry": "registry_complete",
                "clients": [
                    {
                        "name": "Mac",
                        "cid": "c_mac",
                        "status": "active",
                    },
                    {
                        "name": "NoPlatform",
                        "cid": "c_none",
                        "status": "active",
                    }
                ],
                "unassessed": []
            }),
            reported_platforms: Some(BTreeMap::new()),
            ledger_platforms: ledger_desktop,
            bound_streams: None,
            segments: None,
            earliest_progress: None,
            brain_state: "ready".into(),
            brain_reason: None,
            brain_progressing: false,
            ui_lane: None,
        };
        let out_desktop = project_first_minutes(&input_desktop);
        assert_eq!(out_desktop["checks"]["phone"], false);
    }

    #[test]
    fn winning_segment_timezone_instant_comparison() {
        let mut data_state_map = BTreeMap::new();
        data_state_map.insert("audio".into(), "analyzed".into());
        let sensed_state = DataStateMap(data_state_map);

        // Candidate 1: wall clock 10:00:00, offset +05:00 -> UTC 05:00:00
        let candidate_offset = SegmentCandidate {
            day: "20261010".into(),
            key: "100000_300".into(),
            stream: "phone".into(),
            data_state: sensed_state.clone(),
            reported_tz: None,
            reported_offset_seconds: Some(5 * 3600),
        };
        // Candidate 2: wall clock 08:00:00, no reported zone (uses owner_zone UTC) -> UTC 08:00:00
        let candidate_owner = SegmentCandidate {
            day: "20261010".into(),
            key: "080000_300".into(),
            stream: "desk".into(),
            data_state: sensed_state.clone(),
            reported_tz: None,
            reported_offset_seconds: None,
        };

        let input = FirstMinutesInput {
            now_ms: 1_000_000,
            owner_zone: Tz::UTC,
            capture: json!({"registry": "registry_complete"}),
            reported_platforms: None,
            ledger_platforms: BTreeMap::new(),
            bound_streams: None,
            segments: Some(vec![candidate_owner.clone(), candidate_offset.clone()]),
            earliest_progress: None,
            brain_state: "ready".into(),
            brain_reason: None,
            brain_progressing: false,
            ui_lane: None,
        };
        let winner = winning_segment(&input).expect("winner");
        // Candidate offset wins because UTC 05:00:00 < 08:00:00
        assert_eq!(winner.stream, "phone");
        assert_eq!(winner.key, "100000_300");
    }

    #[test]
    fn segment_processing_and_sensed_verdicts() {
        let mut data_state_map = BTreeMap::new();
        data_state_map.insert("audio".into(), "analyzed".into());
        let sensed_state = DataStateMap(data_state_map);

        let candidate = SegmentCandidate {
            day: "20261010".into(),
            key: "120000_300".into(),
            stream: "phone".into(),
            data_state: sensed_state.clone(),
            reported_tz: Some("UTC".into()),
            reported_offset_seconds: Some(0),
        };

        // earliest_progress: None with fully sensed segment -> returns segment, thinking: "none"
        let input_no_progress = FirstMinutesInput {
            now_ms: 1_000_000,
            owner_zone: Tz::UTC,
            capture: json!({"registry": "registry_complete"}),
            reported_platforms: None,
            ledger_platforms: BTreeMap::new(),
            bound_streams: None,
            segments: Some(vec![candidate.clone()]),
            earliest_progress: None,
            brain_state: "ready".into(),
            brain_reason: None,
            brain_progressing: false,
            ui_lane: None,
        };
        let out_no_progress = project_first_minutes(&input_no_progress);
        assert_eq!(out_no_progress["first_segment"]["thinking"], "none");
        assert_eq!(
            out_no_progress["first_segment"]["href"],
            "/app/transcripts/20261010?stream=phone#120000_300"
        );

        // Not fully sensed segment -> thinking: "none"
        let mut empty_state = BTreeMap::new();
        empty_state.insert("audio".into(), "empty".into());
        let not_sensed_candidate = SegmentCandidate {
            day: "20261010".into(),
            key: "120000_300".into(),
            stream: "phone".into(),
            data_state: DataStateMap(empty_state),
            reported_tz: Some("UTC".into()),
            reported_offset_seconds: Some(0),
        };
        let input_not_sensed = FirstMinutesInput {
            now_ms: 1_000_000,
            owner_zone: Tz::UTC,
            capture: json!({"registry": "registry_complete"}),
            reported_platforms: None,
            ledger_platforms: BTreeMap::new(),
            bound_streams: None,
            segments: Some(vec![not_sensed_candidate]),
            earliest_progress: None,
            brain_state: "ready".into(),
            brain_reason: None,
            brain_progressing: false,
            ui_lane: None,
        };
        let out_not_sensed = project_first_minutes(&input_not_sensed);
        assert_eq!(out_not_sensed["first_segment"]["thinking"], "none");
    }

    #[test]
    fn additional_brain_state_and_step_mappings() {
        // brain unhealthy -> state: other, step: none, checks.thinking: true
        let mut input = FirstMinutesInput {
            now_ms: 1_000_000,
            owner_zone: Tz::UTC,
            capture: json!({"registry": "registry_complete"}),
            reported_platforms: None,
            ledger_platforms: BTreeMap::new(),
            bound_streams: None,
            segments: None,
            earliest_progress: None,
            brain_state: "unhealthy".into(),
            brain_reason: None,
            brain_progressing: false,
            ui_lane: None,
        };
        let out = project_first_minutes(&input);
        assert_eq!(out["thinking"]["state"], "other");
        assert_eq!(out["thinking"]["step"], "none");
        assert_eq!(out["checks"]["thinking"], true);

        // brain checking -> progressing / none
        input.brain_state = "checking".into();
        let out_checking = project_first_minutes(&input);
        assert_eq!(out_checking["thinking"]["state"], "progressing");
        assert_eq!(out_checking["thinking"]["step"], "none");
        assert_eq!(out_checking["checks"]["thinking"], true);

        // blocked + brain_progressing: true -> progressing / none even when reason is gpu_unavailable
        input.brain_state = "blocked".into();
        input.brain_reason = Some("gpu_unavailable".into());
        input.brain_progressing = true;
        let out_progressing = project_first_minutes(&input);
        assert_eq!(out_progressing["thinking"]["state"], "progressing");
        assert_eq!(out_progressing["thinking"]["step"], "none");
        assert_eq!(out_progressing["checks"]["thinking"], true);

        // endpoint_configuration_incomplete with ui_lane: null -> needs_step / byo
        input.brain_progressing = false;
        input.brain_reason = Some("endpoint_configuration_incomplete".into());
        input.ui_lane = None;
        let out_byo = project_first_minutes(&input);
        assert_eq!(out_byo["thinking"]["state"], "needs_step");
        assert_eq!(out_byo["thinking"]["step"], "byo");
        assert_eq!(out_byo["checks"]["thinking"], false);
    }
}
