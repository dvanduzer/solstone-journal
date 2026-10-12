// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Owner-voice candidate detection from the speaker candidate pool.
//!
//! Detection picks the largest pool cluster that could be the owner, checks
//! it against the evidence gates, and saves it as a candidate the owner
//! reviews before anything is confirmed. The Speakers page and the daily
//! pool catch-up both run it.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use chrono::Utc;
use ndarray::Array2;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use solstone_core_journal_io::SegmentLayout;
use solstone_core_speaker_id::calibration::OWNER_THRESHOLD;

use crate::candidate_tracker::{
    CandidateProfile, CandidateTracker, MERGE_THRESHOLD, trim_solo_cluster_rows,
};
use crate::owner_admission::{OwnerAdmission, admitted_owner_id};
use crate::owner_candidate::{
    OwnerCandidate, clear_owner_candidate_in_lock, hold_owner_candidate_lock,
    hydrate_owner_candidate_samples, load_owner_candidate, write_owner_candidate_in_lock,
};
use crate::owner_centroid::load_owner_centroid;
use crate::owner_voice::{cooldown_days_remaining, update_voiceprint_state, voiceprint_state};
use crate::segment_catalog::{SegmentLookup, decode_stream_layout_value, lookup_segment};

/// Fewest statements a voice needs before it can be offered as the owner's.
pub const MIN_STATEMENTS: usize = 30;
/// Statements at which the evidence counts as strong.
pub const STRONG_STATEMENTS: usize = 100;
/// Shortest median statement length, in seconds.
pub const MIN_MEDIAN_DURATION: f64 = 1.5;
/// Consistency floor for standard evidence.
pub const STANDARD_P25: f64 = 0.30;
/// Consistency floor for strong evidence.
pub const STRONG_P25: f64 = 0.15;
/// Most embeddings one detection reads from the pool.
pub const CANDIDATE_EXPANSION_MAX_EMBEDDINGS: usize = 3000;

/// Identity failure reason shared with the HTTP owner-write surface.
pub const OWNER_IDENTITY_INVALID: &str = "speaker_owner_identity_invalid";

/// Look for an owner-voice candidate, or report why there is none.
///
/// A confirmed voice is reported as confirmed. Rejected voices are never
/// offered again, and a recent rejection waits out its cooldown unless
/// `force` is set.
pub fn detect_owner_candidate(root: &Path, force: bool) -> Result<Value, String> {
    let principal = match admitted_owner_id(root) {
        OwnerAdmission::Admitted(id) => id,
        OwnerAdmission::Invalid => return Err(OWNER_IDENTITY_INVALID.to_owned()),
    };
    let _lock = hold_owner_candidate_lock(root).map_err(|error| error.to_string())?;

    if let Ok(Some(centroid)) = load_owner_centroid(root, &principal) {
        // Confirmation saved how many streams the voice was heard in.
        let streams_represented = voiceprint_state(root)
            .get("streams_represented")
            .cloned()
            .unwrap_or_else(|| json!(0));
        update_voiceprint_state(
            root,
            json!({"status":"confirmed","cluster_size":centroid.cluster_size,"confirmed_at":Utc::now().to_rfc3339(),"evidence_tier":centroid.evidence_tier.clone().unwrap_or_else(|| "standard".to_owned())}),
        )?;
        return Ok(
            json!({"status":"confirmed","recommendation":"confirmed","cluster_size":centroid.cluster_size,"streams_represented":streams_represented,"samples":[],"evidence_tier":centroid.evidence_tier.unwrap_or_else(|| "standard".to_owned())}),
        );
    }
    if force {
        clear_owner_candidate_in_lock(root).map_err(|error| error.to_string())?;
        update_voiceprint_state(root, json!({"rejected_at":Value::Null}))?;
    }
    let state = voiceprint_state(root);
    if !force && let Some(days_remaining) = cooldown_days_remaining(&state) {
        return Ok(
            json!({"status":"no_cluster","reason":"cooldown","segments_checked":0,"segments_available":0,"embeddings_available":0,"recommendation":"no_cluster","days_remaining":days_remaining,"next_step":"wait_for_cooldown","guidance":"Wait for the owner voice rejection cooldown before running detection again, or run solstone call speakers detect --force to look now."}),
        );
    }
    if let Ok(Some(candidate)) = load_owner_candidate(root)
        && state.get("status").and_then(Value::as_str) == Some("candidate")
    {
        let raw_samples = state
            .get("samples")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let samples = hydrate_owner_candidate_samples(root, &candidate, &raw_samples);
        return Ok(
            json!({"status":"candidate","cluster_size":candidate.cluster_size,"streams_represented":state.get("streams_represented").cloned().unwrap_or_else(|| json!(0)),"recommendation":state.get("recommendation").cloned().unwrap_or_else(|| json!("single_stream")),"samples":samples,"evidence_tier":candidate.evidence_tier}),
        );
    }
    let pool_exists = root.join("awareness/speaker_candidates.json").exists();
    let candidates = CandidateTracker::new(root).candidates();
    let rejected = rejected_voices(&state);
    let candidate = match select_candidate(pool_exists, candidates, Some(&principal), &rejected) {
        Ok(candidate) => candidate,
        Err(reason) => return no_cluster(root, reason, 0, 0, 0),
    };
    if candidate.n_intervals < MIN_STATEMENTS {
        return candidate_low_quality(
            root,
            CandidateLowQuality {
                reason: "too_few_stmts",
                observed: candidate.n_intervals as f64,
                threshold: MIN_STATEMENTS as f64,
                segments: candidate.n_segments,
                embeddings: candidate.n_intervals,
                tier: "standard",
                bound: STANDARD_P25,
            },
        );
    }
    let expansion = expand_candidate(root, &candidate)?;
    if expansion.rows.is_empty() {
        return no_cluster(
            root,
            "candidate_no_usable_embeddings",
            expansion.checked,
            expansion.available,
            0,
        );
    }
    let quality = candidate_quality(&expansion.rows);
    if let Some(reason) = quality.reason {
        return candidate_low_quality(
            root,
            CandidateLowQuality {
                reason,
                observed: if reason == "median_duration_too_short" {
                    quality.median
                } else {
                    quality.p25
                },
                threshold: if reason == "median_duration_too_short" {
                    MIN_MEDIAN_DURATION
                } else {
                    quality.bound
                },
                segments: expansion.checked,
                embeddings: expansion.rows.len(),
                tier: quality.tier,
                bound: quality.bound,
            },
        );
    }
    let Some(centroid) = mean(
        &expansion
            .rows
            .iter()
            .map(|row| row.embedding.clone())
            .collect::<Vec<_>>(),
    )
    .and_then(|row| solstone_core_entity::normalize_embedding(&row)) else {
        return no_cluster(
            root,
            "candidate_centroid_unusable",
            expansion.checked,
            expansion.available,
            expansion.rows.len(),
        );
    };
    let streams = expansion
        .rows
        .iter()
        .map(|row| row.stream.clone())
        .collect::<BTreeSet<_>>()
        .len();
    let recommendation = if streams > 1 {
        "ready"
    } else {
        "single_stream"
    };
    let version = Utc::now().to_rfc3339();
    let samples = candidate_samples(&expansion.rows, &centroid, &version);
    let snapshot = OwnerCandidate {
        centroid,
        cluster_size: expansion.rows.len() as i32,
        threshold: OWNER_THRESHOLD,
        version: version.clone(),
        evidence_tier: quality.tier.to_owned(),
    };
    write_owner_candidate_in_lock(root, &snapshot).map_err(|error| error.to_string())?;
    update_voiceprint_state(
        root,
        json!({"status":"candidate","cluster_size":expansion.rows.len(),"streams_represented":streams,"recommendation":recommendation,"samples":samples,"detected_at":version,"evidence_tier":quality.tier,"candidate_cluster_id":candidate.cand_id}),
    )?;
    let hydrated_samples = hydrate_owner_candidate_samples(root, &snapshot, &samples);
    Ok(
        json!({"status":"candidate","cluster_size":expansion.rows.len(),"streams_represented":streams,"recommendation":recommendation,"samples":hydrated_samples,"evidence_tier":quality.tier}),
    )
}

/// A voice the owner said was not theirs.
struct RejectedVoice {
    cluster_id: Option<i64>,
    centroid: Vec<f32>,
}

fn rejected_voices(state: &BTreeMap<String, Value>) -> Vec<RejectedVoice> {
    state
        .get("rejected_candidates")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|row| RejectedVoice {
            cluster_id: row.get("cluster_id").and_then(Value::as_i64),
            centroid: row
                .get("centroid")
                .and_then(Value::as_array)
                .map(|values| {
                    values
                        .iter()
                        .filter_map(Value::as_f64)
                        .map(|value| value as f32)
                        .collect()
                })
                .unwrap_or_default(),
        })
        .collect()
}

/// A pool cluster is the rejected voice if it carries the same id, or if the
/// pool has since merged it under another id and it still sounds the same.
fn is_rejected(candidate: &CandidateProfile, rejected: &[RejectedVoice]) -> bool {
    rejected.iter().any(|voice| {
        voice.cluster_id == Some(candidate.cand_id)
            || (voice.centroid.len() == candidate.centroid.len()
                && dot(&voice.centroid, &candidate.centroid) >= MERGE_THRESHOLD)
    })
}

fn select_candidate(
    pool_exists: bool,
    candidates: Vec<CandidateProfile>,
    principal: Option<&str>,
    rejected: &[RejectedVoice],
) -> Result<CandidateProfile, &'static str> {
    if !pool_exists {
        return Err("pool_missing");
    }
    if candidates.is_empty() {
        return Err("pool_empty");
    }
    candidates
        .into_iter()
        .filter(|candidate| {
            candidate.status != "rejected"
                && (candidate.confirmed_entity.is_none()
                    || candidate.confirmed_entity.as_deref() == principal)
                && !is_rejected(candidate, rejected)
        })
        .max_by(|left, right| {
            left.n_intervals
                .cmp(&right.n_intervals)
                .then_with(|| left.total_duration_s.total_cmp(&right.total_duration_s))
                .then_with(|| right.cand_id.cmp(&left.cand_id))
        })
        .ok_or("no_eligible_candidate")
}

struct CandidateRow {
    embedding: Vec<f32>,
    day: String,
    stream: String,
    segment_key: String,
    source: String,
    sentence_id: i64,
    jsonl_path: PathBuf,
    layout: SegmentLayout,
}
struct CandidateExpansion {
    rows: Vec<CandidateRow>,
    checked: usize,
    available: usize,
}
fn expand_candidate(
    root: &Path,
    candidate: &CandidateProfile,
) -> Result<CandidateExpansion, String> {
    let mut groups = BTreeMap::<(String, String), Vec<&Value>>::new();
    for source in &candidate.source_segments {
        groups
            .entry((
                source
                    .get("stream_layout")
                    .and_then(Value::as_str)
                    .unwrap_or("named")
                    .to_owned(),
                source
                    .get("stream")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            ))
            .or_default()
            .push(source);
    }
    let max = groups.values().map(Vec::len).max().unwrap_or(0);
    let mut rows = Vec::new();
    let mut checked = BTreeSet::new();
    'rounds: for index in 0..max {
        for group in groups.values() {
            let Some(value) = group.get(index) else {
                continue;
            };
            let (Some(day), Some(stream), Some(segment_key), Some(source), Some(label)) = (
                value.get("day").and_then(Value::as_str),
                value.get("stream").and_then(Value::as_str),
                value.get("segment_key").and_then(Value::as_str),
                value.get("source").and_then(Value::as_str),
                value.get("cluster_label").and_then(Value::as_i64),
            ) else {
                return Err("invalid candidate source segment".to_owned());
            };
            let layout = decode_stream_layout_value(value.get("stream_layout"))
                .map_err(|_| "invalid stream_layout on candidate source segment".to_owned())?;
            checked.insert((
                day.to_owned(),
                layout_flag(layout),
                stream.to_owned(),
                segment_key.to_owned(),
            ));
            let segment = match lookup_segment(root, day, stream, segment_key, Ok(layout)) {
                SegmentLookup::Present(path) => path,
                SegmentLookup::Absent => continue,
                SegmentLookup::MalformedLayout => {
                    return Err("invalid stream_layout on candidate source segment".to_owned());
                }
                SegmentLookup::Failed(error) => return Err(error.to_string()),
            };
            let jsonl = segment.join(format!("{source}.jsonl"));
            if !segment.is_dir() || segment_overlap_fraction(&jsonl) > 0.10 {
                continue;
            }
            let embeddings_path = segment.join(format!("{source}.npz"));
            let file = match solstone_core_speaker_id::embeddings::load_embeddings_file(
                &embeddings_path,
            ) {
                Ok(Some(file)) => file,
                Ok(None) => continue,
                Err(error) => {
                    return Err(format!(
                        "failed to load {}: {error}",
                        embeddings_path.display()
                    ));
                }
            };
            // A row written before membership was stored has no ids to expand;
            // it is skipped. Ids that are present but unreadable are an error.
            if value.get("sentence_ids").is_none() {
                continue;
            }
            let ids = source_sentence_ids(value)
                .ok_or_else(|| "invalid candidate sentence ids".to_owned())?;
            let mut selected = ids
                .into_iter()
                .filter_map(|id| {
                    file.statements
                        .iter()
                        .find(|(current, _)| *current == id)
                        .and_then(|(_, embedding)| {
                            solstone_core_entity::normalize_embedding(embedding).map(|embedding| {
                                CandidateRow {
                                    embedding,
                                    day: day.to_owned(),
                                    stream: stream.to_owned(),
                                    segment_key: segment_key.to_owned(),
                                    source: source.to_owned(),
                                    sentence_id: id,
                                    jsonl_path: jsonl.clone(),
                                    layout,
                                }
                            })
                        })
                })
                .collect::<Vec<_>>();
            if label == -1 {
                let before = selected
                    .iter()
                    .map(|row| row.embedding.clone())
                    .collect::<Vec<_>>();
                let (kept, _, _) = trim_solo_cluster_rows(&before);
                selected.retain(|row| kept.contains(&row.embedding));
            }
            for row in selected {
                if rows.len() >= CANDIDATE_EXPANSION_MAX_EMBEDDINGS {
                    break 'rounds;
                }
                rows.push(row);
            }
        }
    }
    Ok(CandidateExpansion {
        rows,
        checked: checked.len(),
        available: candidate.n_segments,
    })
}
fn source_sentence_ids(value: &Value) -> Option<Vec<i64>> {
    let mut ids = value
        .get("sentence_ids")?
        .as_array()?
        .iter()
        .map(|value| match value {
            Value::Bool(_) => None,
            Value::Number(value) => value
                .as_i64()
                .or_else(|| value.as_f64().map(|number| number as i64)),
            Value::String(value) => value.parse().ok(),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()?;
    ids.sort_unstable();
    ids.dedup();
    Some(ids)
}

/// Evidence-gate measurements for a set of owner statements.
pub struct Quality {
    pub reason: Option<&'static str>,
    pub median: f64,
    pub p25: f64,
    pub tier: &'static str,
    pub bound: f64,
}

/// Apply the owner evidence gates to statement embeddings and durations.
pub fn evidence_quality(embeddings: &[Vec<f32>], mut durations: Vec<f64>) -> Quality {
    let tier = if embeddings.len() >= STRONG_STATEMENTS {
        "strong"
    } else {
        "standard"
    };
    let bound = if tier == "strong" {
        STRONG_P25
    } else {
        STANDARD_P25
    };
    durations.sort_by(f64::total_cmp);
    let median = durations
        .get((durations.len().saturating_sub(1)) / 2)
        .copied()
        .unwrap_or(0.0);
    let p25 = intra_cosine_p25(embeddings).unwrap_or(0.0);
    let reason = if embeddings.len() < MIN_STATEMENTS {
        Some("too_few_stmts")
    } else if median < MIN_MEDIAN_DURATION {
        Some("median_duration_too_short")
    } else if p25 < bound {
        Some("cluster_too_diffuse")
    } else {
        None
    };
    Quality {
        reason,
        median,
        p25,
        tier,
        bound,
    }
}

fn candidate_quality(rows: &[CandidateRow]) -> Quality {
    evidence_quality(
        &rows
            .iter()
            .map(|row| row.embedding.clone())
            .collect::<Vec<_>>(),
        rows.iter()
            .filter_map(|row| fallback_duration(&row.jsonl_path, row.sentence_id))
            .collect(),
    )
}
fn candidate_samples(rows: &[CandidateRow], centroid: &[f32], version: &str) -> Vec<Value> {
    let mut ordered = rows.iter().collect::<Vec<_>>();
    ordered.sort_by(|left, right| {
        dot(&right.embedding, centroid).total_cmp(&dot(&left.embedding, centroid))
    });
    let mut seen = BTreeSet::new();
    ordered
        .into_iter()
        .filter(|row| {
            seen.insert((
                row.day.as_str(),
                layout_flag(row.layout),
                row.stream.as_str(),
                row.segment_key.as_str(),
            ))
        })
        .take(3)
        .map(|row| {
            let segment = row
                .jsonl_path
                .parent()
                .unwrap_or_else(|| Path::new(""))
                .to_path_buf();

            let mut audio_sha256 = Value::Null;
            let mut audio_url = None;

            for (ext, _) in crate::audio_sample::AUDIO_FORMATS {
                let path = segment.join(format!("{}{}", row.source, ext));
                if path.is_file() {
                    if let Ok(bytes) = fs::read(&path)
                        && !bytes.is_empty()
                    {
                        audio_sha256 = Value::String(format!("{:x}", Sha256::digest(&bytes)));
                    }
                    let (url, _) = crate::audio_sample::audio_info(
                        &segment,
                        &row.day,
                        &row.stream,
                        &row.segment_key,
                        &row.source,
                        row.layout,
                    );
                    audio_url = url;
                    break;
                }
            }

            let mut emb_le = Vec::with_capacity(row.embedding.len() * 4);
            for val in &row.embedding {
                emb_le.extend_from_slice(&val.to_le_bytes());
            }
            let embedding_sha256 = format!("{:x}", Sha256::digest(&emb_le));

            json!({
                "day": row.day,
                "stream_layout": layout_flag(row.layout),
                "stream": row.stream,
                "segment_key": row.segment_key,
                "source": row.source,
                "sentence_id": row.sentence_id,
                "duration_s": fallback_duration(&row.jsonl_path, row.sentence_id),
                "audio_url": audio_url,
                "version": version,
                "embedding_sha256": embedding_sha256,
                "audio_sha256": audio_sha256,
            })
        })
        .collect()
}

fn layout_flag(layout: SegmentLayout) -> &'static str {
    match layout {
        SegmentLayout::Direct => "direct",
        SegmentLayout::Named => "named",
    }
}

fn fallback_duration(path: &Path, sentence_id: i64) -> Option<f64> {
    let starts = fs::read_to_string(path)
        .ok()?
        .lines()
        .skip(1)
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter_map(|value| {
            value
                .get("start")
                .and_then(Value::as_str)
                .and_then(parse_time)
        })
        .collect::<Vec<_>>();
    let index = usize::try_from(sentence_id.checked_sub(1)?).ok()?;
    Some((starts.get(index + 1)? - starts.get(index)?) as f64)
}
fn dot(left: &[f32], right: &[f32]) -> f32 {
    left.iter()
        .zip(right)
        .map(|(left, right)| left * right)
        .sum()
}
struct CandidateLowQuality<'a> {
    reason: &'a str,
    observed: f64,
    threshold: f64,
    segments: usize,
    embeddings: usize,
    tier: &'a str,
    bound: f64,
}
fn candidate_low_quality(root: &Path, value: CandidateLowQuality<'_>) -> Result<Value, String> {
    update_voiceprint_state(
        root,
        json!({"status":"low_quality","source":"candidate_pool","low_quality_reason":value.reason,"observed_value":value.observed,"threshold_value":value.threshold,"evidence_tier":value.tier,"intra_cosine_p25_bound":value.bound,"segments_checked":value.segments,"attempted_at":Utc::now().to_rfc3339()}),
    )?;
    Ok(
        json!({"status":"low_quality","source":"candidate_pool","recommendation":"low_quality","segments_available":value.segments,"embeddings_available":value.embeddings,"low_quality_reason":value.reason,"observed_value":value.observed,"threshold_value":value.threshold,"evidence_tier":value.tier,"intra_cosine_p25_bound":value.bound,"next_step":"seed_manual_tags","guidance":"Use solstone call speakers tag-owner to add validated owner tags."}),
    )
}
fn no_cluster(
    root: &Path,
    reason: &str,
    checked: usize,
    segments: usize,
    embeddings: usize,
) -> Result<Value, String> {
    update_voiceprint_state(
        root,
        json!({"status":"no_cluster","reason":reason,"attempted_at":Utc::now().to_rfc3339()}),
    )?;
    Ok(
        json!({"status":"no_cluster","reason":reason,"segments_checked":checked,"segments_available":segments,"embeddings_available":embeddings,"recommendation":"no_cluster","next_step":"seed_manual_tags","guidance":"Use solstone call speakers tag-owner to add validated owner tags."}),
    )
}

/// Parse an `HH:MM:SS` transcript start time into seconds.
pub fn parse_time(value: &str) -> Option<i64> {
    let mut parts = value.split(':').map(|part| part.parse::<i64>().ok());
    Some(parts.next()?? * 3600 + parts.next()?? * 60 + parts.next()??)
}

/// Element-wise mean of equal-width rows.
pub fn mean(rows: &[Vec<f32>]) -> Option<Vec<f32>> {
    let first = rows.first()?;
    let mut mean = vec![0.0; first.len()];
    for row in rows {
        for (out, value) in mean.iter_mut().zip(row) {
            *out += *value;
        }
    }
    for value in &mut mean {
        *value /= rows.len() as f32;
    }
    Some(mean)
}

/// The fraction of a transcript the transcriber marked as overlapping speech.
pub fn segment_overlap_fraction(path: &Path) -> f64 {
    fs::read_to_string(path)
        .ok()
        .and_then(|contents| contents.lines().next().map(str::to_owned))
        .and_then(|line| serde_json::from_str::<Value>(&line).ok())
        .and_then(|header| header.get("overlap_fraction").and_then(Value::as_f64))
        .unwrap_or(0.0)
}

/// The 25th-percentile pairwise cosine between embeddings: how consistent a
/// voice is.
pub fn intra_cosine_p25(embeddings: &[Vec<f32>]) -> Option<f64> {
    if embeddings.len() < 2 {
        return None;
    }
    let normalized = embeddings
        .iter()
        .map(|embedding| {
            let norm = embedding
                .iter()
                .map(|value| value * value)
                .sum::<f32>()
                .sqrt();
            let norm = if norm < 1e-9 { 1.0 } else { norm };
            embedding
                .iter()
                .map(|value| value / norm)
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let columns = normalized.first()?.len();
    if columns == 0
        || normalized
            .iter()
            .any(|embedding| embedding.len() != columns)
    {
        return None;
    }
    let rows = normalized.len();
    let values = normalized.into_iter().flatten().collect::<Vec<_>>();
    let matrix = Array2::from_shape_vec((rows, columns), values).ok()?;
    // NumPy uses a float32 matrix multiply (`e_norm @ e_norm.T`) before
    // percentile interpolation, so use a GEMM rather than a scalar dot loop.
    let similarities = matrix.dot(&matrix.t());
    let mut cosines = Vec::new();
    for row in 0..rows {
        cosines.extend((row + 1..rows).map(|column| similarities[(row, column)]));
    }
    cosines.sort_by(|left, right| left.partial_cmp(right).unwrap_or(Ordering::Equal));
    let rank = (cosines.len() - 1) as f64 * 0.25;
    let lower = rank.floor() as usize;
    let upper = rank.ceil() as usize;
    let fraction = rank - lower as f64;
    Some(f64::from(cosines[lower]) + fraction * f64::from(cosines[upper] - cosines[lower]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::owner_centroid::{OwnerCentroidWriteInput, write_owner_centroid};
    use std::path::PathBuf;

    #[test]
    fn detect_reports_the_confirmed_voice_streams_saved_at_confirmation() {
        let root = PathBuf::from("/var/tmp").join(format!(
            "solstone-convey-owner-confirmed-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("entities/owner")).unwrap();
        fs::write(
            root.join("entities/owner/entity.json"),
            serde_json::json!({"id":"owner","name":"Owner","type":"Person","is_principal":true})
                .to_string(),
        )
        .unwrap();
        let mut centroid = vec![0.0_f32; 256];
        centroid[0] = 1.0;
        write_owner_centroid(
            &root,
            "owner",
            &OwnerCentroidWriteInput {
                centroid,
                cluster_size: 40,
                timestamp: Utc::now().to_rfc3339(),
                evidence_tier: "standard".to_owned(),
            },
        )
        .unwrap();
        update_voiceprint_state(&root, serde_json::json!({"streams_represented": 3})).unwrap();

        let detected = detect_owner_candidate(&root, false).unwrap();
        assert_eq!(detected["status"], "confirmed");
        assert_eq!(detected["streams_represented"], 3);
        let _ = fs::remove_dir_all(&root);
    }

    /// A journal with an owner and one transcribed source fed into the pool.
    fn fed_owner_journal(name: &str) -> PathBuf {
        let dir_path = PathBuf::from("/var/tmp").join(format!(
            "solstone-convey-owner-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir_path);
        fs::create_dir_all(dir_path.join("entities/owner")).unwrap();
        fs::write(
            dir_path.join("entities/owner/entity.json"),
            serde_json::json!({
                "id": "owner",
                "name": "Owner",
                "type": "Person",
                "is_principal": true
            })
            .to_string(),
        )
        .unwrap();

        let seg_dir = dir_path.join("chronicle/20260101/093000_300_summary");
        fs::create_dir_all(&seg_dir).unwrap();
        let jsonl_path = seg_dir.join("audio.jsonl");
        let npz_path = seg_dir.join("audio.npz");

        let mut unit_vec = vec![0.0_f32; 256];
        unit_vec[0] = 1.0;
        let mut raw_bytes = Vec::new();
        for _ in 0..30 {
            for col in &unit_vec {
                raw_bytes.extend_from_slice(&col.to_le_bytes());
            }
        }
        let payload_path = seg_dir.join("audio_payload.f32");
        fs::write(&payload_path, &raw_bytes).unwrap();

        let sids: Vec<i64> = (1..=30).collect();
        let statements: Vec<serde_json::Value> = sids
            .iter()
            .enumerate()
            .map(|(i, &sid)| {
                serde_json::json!({
                    "id": sid,
                    "sentence_id": sid,
                    "start_offset_us": (i as i64) * 1_000_000,
                    "text": format!("statement {sid}"),
                })
            })
            .collect();

        let req = serde_json::json!({
            "schema": "solstone-speaker-transcript-write-request-v1",
            "output": {
                "jsonl_path": jsonl_path.display().to_string(),
                "npz_path": npz_path.display().to_string(),
                "redo": true,
            },
            "base_time_us_of_day": 100_000_u64,
            "source": "audio",
            "statements": statements,
            "header": {
                "raw": "audio.wav",
                "model": "model",
                "device": "cpu",
                "compute_type": "int8",
                "speaker_evidence": "single",
                "speaker_evidence_version": "windowed-slots-v1",
                "speaker_evidence_multi_fraction": 0.0,
            },
            "embeddings": {
                "payload_path": payload_path,
                "payload_format": "raw-f32le-row-major-v1",
                "dtype": "float32-le",
                "shape": [30, 256],
                "byte_count": 30 * 256 * 4,
                "statement_ids": sids,
                "durations_s": vec![1.5; 30],
                "encoder": "test",
            }
        });
        solstone_core_speaker_id::writer::write_request(
            serde_json::to_vec(&req).unwrap().as_slice(),
        )
        .unwrap();

        let inputs = match crate::transcribed_clusters::load_transcribed_cluster_inputs(
            &dir_path,
            &jsonl_path,
            &npz_path,
        )
        .unwrap()
        {
            crate::transcribed_clusters::TranscribedClusterLoad::Clusters(inputs) => inputs,
            other => panic!("expected clusters, got {other:?}"),
        };
        assert!(!inputs.is_empty());

        let mut tracker = crate::candidate_tracker::CandidateTracker::new(&dir_path);
        tracker.add_transcribed_clusters(&inputs).unwrap();
        dir_path
    }

    #[test]
    fn detect_owner_candidate_accepts_transcribed_pool() {
        let dir_path = fed_owner_journal("fed");
        let res = detect_owner_candidate(&dir_path, true)
            .expect("detection should succeed without error");
        let reason = res.get("reason").and_then(Value::as_str);
        assert_ne!(reason, Some("pool_missing"));
        assert_ne!(reason, Some("pool_empty"));

        let _ = fs::remove_dir_all(dir_path);
    }

    fn rewrite_pool_sentence_ids(root: &Path, value: Option<Value>) {
        let path = root.join("awareness/speaker_candidates.json");
        let mut pool: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        for candidate in pool["candidates"].as_array_mut().unwrap() {
            for row in candidate["source_segments"].as_array_mut().unwrap() {
                let row = row.as_object_mut().unwrap();
                match &value {
                    Some(value) => row.insert("sentence_ids".to_owned(), value.clone()),
                    None => row.remove("sentence_ids"),
                };
            }
        }
        fs::write(&path, serde_json::to_vec(&pool).unwrap()).unwrap();
    }

    #[test]
    fn detect_owner_candidate_skips_legacy_rows_without_sentence_ids() {
        let dir_path = fed_owner_journal("legacy");
        rewrite_pool_sentence_ids(&dir_path, None);
        detect_owner_candidate(&dir_path, true)
            .expect("a legacy row without sentence ids is skipped, not an error");
        let _ = fs::remove_dir_all(dir_path);
    }

    #[test]
    fn detect_owner_candidate_refuses_malformed_sentence_ids() {
        let dir_path = fed_owner_journal("malformed");
        rewrite_pool_sentence_ids(&dir_path, Some(serde_json::json!("not ids")));
        assert_eq!(
            detect_owner_candidate(&dir_path, true).unwrap_err(),
            "invalid candidate sentence ids"
        );
        let _ = fs::remove_dir_all(dir_path);
    }

    #[test]
    fn detect_owner_candidate_after_pool_backfill() {
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir_path = PathBuf::from("/var/tmp").join(format!(
            "solstone-convey-owner-backfill-test-{}-{seq}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir_path);
        fs::create_dir_all(dir_path.join("entities/owner")).unwrap();
        fs::write(
            dir_path.join("entities/owner/entity.json"),
            serde_json::json!({
                "id": "owner",
                "name": "Owner",
                "type": "Person",
                "is_principal": true
            })
            .to_string(),
        )
        .unwrap();

        // 1. Before: detect_owner_candidate returns pool_missing
        let res_before =
            detect_owner_candidate(&dir_path, true).expect("detection before backfill");
        assert_eq!(
            res_before.get("reason").and_then(Value::as_str),
            Some("pool_missing")
        );

        // Setup 3 days with 10 statement IDs each (30 total statements, 256-d unit vectors, 1.5s durations)
        let days = ["20260101", "20260102", "20260103"];
        let mut all_statement_ids = Vec::new();
        let old_mtime = std::time::SystemTime::now() - std::time::Duration::from_secs(11 * 60);

        for day in days {
            let seg_dir = dir_path.join("chronicle").join(day).join("100000_300");
            fs::create_dir_all(&seg_dir).unwrap();
            let jsonl_path = seg_dir.join("audio.jsonl");
            let npz_path = seg_dir.join("audio.npz");
            let payload_path = seg_dir.join("payload.f32");

            let sids: Vec<i32> = (1..=10).collect();
            all_statement_ids.extend(sids.iter().copied());

            let mut raw_bytes = Vec::new();
            for _ in 0..10 {
                let mut vec_unit = vec![0.0_f32; 256];
                vec_unit[0] = 1.0;
                for val in vec_unit {
                    raw_bytes.extend_from_slice(&val.to_le_bytes());
                }
            }
            fs::write(&payload_path, raw_bytes).unwrap();

            let statements: Vec<Value> = (1..=11)
                .map(|id| {
                    json!({
                        "id": id,
                        "start_offset_us": (id as i64 - 1) * 2_000_000,
                        "text": format!("statement {id}"),
                    })
                })
                .collect();

            let req = json!({
                "schema": "solstone-speaker-transcript-write-request-v1",
                "output": {
                    "jsonl_path": jsonl_path.display().to_string(),
                    "npz_path": npz_path.display().to_string(),
                    "redo": true,
                },
                "base_time_us_of_day": 100_000_u64,
                "source": "audio",
                "statements": statements,
                "header": {
                    "raw": "audio.wav",
                    "model": "model",
                    "device": "cpu",
                    "compute_type": "int8",
                    "speaker_evidence": "single",
                    "speaker_evidence_version": "windowed-slots-v1",
                    "speaker_evidence_multi_fraction": 0.0,
                },
                "embeddings": {
                    "payload_path": payload_path,
                    "payload_format": "raw-f32le-row-major-v1",
                    "dtype": "float32-le",
                    "shape": [10, 256],
                    "byte_count": 10 * 256 * 4,
                    "statement_ids": sids,
                    "durations_s": vec![1.5; 10],
                    "encoder": "test",
                }
            });
            solstone_core_speaker_id::writer::write_request(
                serde_json::to_vec(&req).unwrap().as_slice(),
            )
            .unwrap();

            // Set modified times older than 10 minutes settle window
            let jsonl_file = fs::File::open(&jsonl_path).unwrap();
            jsonl_file.set_modified(old_mtime).unwrap();
            let npz_file = fs::File::open(&npz_path).unwrap();
            npz_file.set_modified(old_mtime).unwrap();
        }

        // Run production backfill_speaker_pool
        let catch_up = crate::candidate_tracker::backfill_speaker_pool(&dir_path);
        let crate::candidate_tracker::SpeakerPoolCatchUp::Done(report) = catch_up else {
            panic!("backfill_speaker_pool refused: {catch_up:?}");
        };
        assert_eq!(report["complete"], true);

        // Verify pool loaded strictly and carries all statements
        let tracker = crate::candidate_tracker::CandidateTracker::new(&dir_path);
        let candidates = tracker.candidates();
        assert!(!candidates.is_empty());
        let candidate = &candidates[0];
        assert_eq!(candidate.n_intervals, 30);
        assert!((candidate.total_duration_s - 45.0).abs() < 1e-3);

        let mut gathered_sids: Vec<i64> = Vec::new();
        for seg in &candidate.source_segments {
            if let Some(arr) = seg.get("sentence_ids").and_then(Value::as_array) {
                for v in arr {
                    if let Some(sid) = v.as_i64() {
                        gathered_sids.push(sid);
                    }
                }
            }
        }
        gathered_sids.sort();
        let mut expected_sids: Vec<i64> = all_statement_ids.into_iter().map(i64::from).collect();
        expected_sids.sort();
        assert_eq!(gathered_sids, expected_sids);

        // 2. After: detect_owner_candidate status is candidate
        let res_after = detect_owner_candidate(&dir_path, true).expect("detection after backfill");
        assert_eq!(
            res_after.get("status").and_then(Value::as_str),
            Some("candidate")
        );

        let _ = fs::remove_dir_all(dir_path);
    }

    #[test]
    fn p25_is_none_for_one_voiceprint() {
        assert_eq!(intra_cosine_p25(&[vec![1.0, 0.0]]), None);
    }

    #[test]
    fn frozen_voiceprint_batches_stay_close_to_python_p25() {
        let voices: std::collections::BTreeMap<String, Vec<Vec<f32>>> = serde_json::from_str(
            include_str!("../../../fixtures/populated_journal_voiceprints.json"),
        )
        .expect("fixture parses");
        for (name, expected) in [
            ("grace_hopper", -0.041_334_249_079_227_45),
            ("alan_turing", 0.074_594_385_921_955_11),
        ] {
            let actual = intra_cosine_p25(&voices[name]).expect("multiple voiceprints have p25");
            assert!(
                (actual - expected).abs() < 1e-4,
                "{name}: {actual} vs {expected}"
            );
        }
    }
}
