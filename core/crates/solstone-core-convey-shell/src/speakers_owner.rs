// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::path::Path;
use std::sync::Arc;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde_json::{Value, json};
use solstone_core_convey_http::envelope::error_envelope;

use crate::JournalRoot;
use crate::speakers_calendar::audio_embedding_sources;
use crate::speakers_npz::{
    OwnerCentroidSummary, load_voiceprints, npz_row_count, owner_centroid_summary,
};
use crate::speakers_quality::{ManualOwnerTagStats, manual_owner_tag_stats, principal_folder};
use solstone_core_speaker_resolve::owner_detection::{
    CANDIDATE_EXPANSION_MAX_EMBEDDINGS, intra_cosine_p25, segment_overlap_fraction,
};
use solstone_core_speaker_resolve::owner_voice::{
    cooldown_days_remaining, next_look_after, relabel_progress, voiceprint_state,
};
use solstone_core_speaker_resolve::segment_catalog::{CatalogBuildError, catalog_journal};

const OWNER_BOOTSTRAP_MIN_STATEMENTS: usize = 30;
const OWNER_DETECT_CANDIDATE_GUIDANCE: &str =
    "Analyze available voice patterns to look for an owner voice candidate.";
const OWNER_REJECTION_COOLDOWN_GUIDANCE: &str = "Wait for the owner voice rejection cooldown before running detection again, \
or run solstone call speakers detect --force to look now.";

pub async fn status(Extension(root): Extension<Arc<JournalRoot>>) -> Response {
    solstone_core_convey_http::owner_read::spawn_blocking_response(
        solstone_core_convey_http::owner_read::OwnerReadRole::SpeakersOwnerStatus,
        move || match owner_status(&root.0) {
            Ok(status) => Json(status).into_response(),
            Err(OwnerStatusError::IdentityInvalid) => error_envelope(
                "speaker_owner_identity_invalid",
                "your owner voice couldn't be loaded because your configured owner identity needs attention.",
                "configured owner identity is not admitted",
                StatusCode::BAD_REQUEST,
            )
            .into_response(),
            Err(OwnerStatusError::Catalog(error)) => error_envelope(
                "speaker_command_failed",
                "that speaker command didn't finish.",
                error.to_string(),
                StatusCode::INTERNAL_SERVER_ERROR,
            )
            .into_response(),
            Err(OwnerStatusError::Failed(error)) => error_envelope(
                "speaker_command_failed",
                "that speaker command didn't finish.",
                error,
                StatusCode::INTERNAL_SERVER_ERROR,
            )
            .into_response(),
        },
    )
    .await
}

enum OwnerStatusError {
    IdentityInvalid,
    Catalog(CatalogBuildError),
    Failed(String),
}

fn owner_status(root: &Path) -> Result<Value, OwnerStatusError> {
    let mut status = owner_setup_status(root)?;
    if let Some(relabel) = relabel_progress(root) {
        status["relabel"] = relabel;
    }
    Ok(status)
}

fn owner_setup_status(root: &Path) -> Result<Value, OwnerStatusError> {
    let principal_id = match solstone_core_speaker_resolve::owner_admission::admitted_owner_id(root)
    {
        solstone_core_speaker_resolve::owner_admission::OwnerAdmission::Admitted(id) => id,
        solstone_core_speaker_resolve::owner_admission::OwnerAdmission::Invalid => {
            return Err(OwnerStatusError::IdentityInvalid);
        }
    };
    let lock = solstone_core_speaker_resolve::owner_candidate::hold_owner_candidate_lock(root)
        .map_err(|e| OwnerStatusError::Failed(e.to_string()))?;
    let voiceprint = voiceprint_state(root);
    let status = match voiceprint.get("status") {
        Some(Value::String(status)) => status.as_str(),
        Some(_) => "invalid",
        None => "none",
    };
    // Snapshot the status with the candidate or saved voice it describes.
    // Recording inventory and manual-tag validation can take much longer.
    let candidate_opt = if status == "candidate" {
        solstone_core_speaker_resolve::owner_candidate::load_owner_candidate(root)
            .map_err(|e| OwnerStatusError::Failed(e.to_string()))?
    } else {
        None
    };
    let centroid = if status == "confirmed" {
        solstone_core_speaker_resolve::owner_centroid::owner_centroid_file(root, &principal_id)
            .ok()
            .and_then(|path| owner_centroid_summary(&path))
    } else {
        None
    };
    drop(lock);
    let manual_stats = manual_owner_tag_stats(root, &principal_id);
    if status == "confirmed" {
        return Ok(confirmed_status(
            root,
            &principal_id,
            &manual_stats,
            centroid,
        ));
    }
    let diagnostics = diagnostics(root, &manual_stats).map_err(OwnerStatusError::Catalog)?;

    Ok(match status {
        "candidate" => {
            let detected_at = voiceprint.get("detected_at").and_then(Value::as_str);

            match (candidate_opt, detected_at) {
                (Some(candidate), Some(ver)) if candidate.version == ver => {
                    let raw_samples = voiceprint
                        .get("samples")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    let hydrated = solstone_core_speaker_resolve::owner_candidate::hydrate_owner_candidate_samples(
                        root,
                        &candidate,
                        &raw_samples,
                    );
                    json!({
                        "status": "candidate",
                        "version": candidate.version,
                        "cluster_size": voiceprint.get("cluster_size").cloned().unwrap_or_else(|| json!(candidate.cluster_size)),
                        "samples": hydrated,
                        "evidence_tier": candidate.evidence_tier,
                        "manual_tags_count": diagnostics.manual_tags_count,
                        "segments_available": diagnostics.segments_available,
                        "embeddings_available": diagnostics.embeddings_available,
                        "streams_represented": diagnostics.streams_represented,
                        "can_build_from_tags": diagnostics.can_build_from_tags,
                        "segments_with_embeddings": diagnostics.segments_with_embeddings,
                    })
                }
                _ => json!({
                    "status": "candidate",
                    "review": "incomplete",
                    "version": voiceprint.get("detected_at").cloned().unwrap_or(Value::Null),
                    "cluster_size": voiceprint.get("cluster_size").cloned().unwrap_or(Value::Null),
                    "samples": [],
                    "evidence_tier": voiceprint.get("evidence_tier").cloned().unwrap_or(Value::Null),
                    "manual_tags_count": diagnostics.manual_tags_count,
                    "segments_available": diagnostics.segments_available,
                    "embeddings_available": diagnostics.embeddings_available,
                    "streams_represented": diagnostics.streams_represented,
                    "can_build_from_tags": diagnostics.can_build_from_tags,
                    "segments_with_embeddings": diagnostics.segments_with_embeddings,
                }),
            }
        }
        "low_quality" => {
            let guidance = manual_guidance(&manual_stats);
            json!({
                "status": "low_quality",
                "source": voiceprint.get("source").cloned().unwrap_or_else(|| json!("candidate_pool")),
                "low_quality_reason": voiceprint.get("low_quality_reason").cloned().unwrap_or_else(|| json!("")),
                "observed_value": voiceprint.get("observed_value").cloned().unwrap_or_else(|| json!(0.0)),
                "threshold_value": voiceprint.get("threshold_value").cloned().unwrap_or_else(|| json!(0.0)),
                "evidence_tier": voiceprint.get("evidence_tier").cloned().unwrap_or(Value::Null),
                "intra_cosine_p25_bound": voiceprint.get("intra_cosine_p25_bound").cloned().unwrap_or(Value::Null),
                "manual_tags_count": diagnostics.manual_tags_count,
                "segments_available": diagnostics.segments_available,
                "embeddings_available": diagnostics.embeddings_available,
                "streams_represented": diagnostics.streams_represented,
                "can_build_from_tags": diagnostics.can_build_from_tags,
                "segments_with_embeddings": diagnostics.segments_with_embeddings,
                "next_step": guidance.next_step,
                "guidance": guidance.guidance,
            })
        }
        "no_cluster" => {
            let guidance = manual_guidance(&manual_stats);
            json!({
                "status": "no_cluster",
                "manual_tags_count": diagnostics.manual_tags_count,
                "segments_available": diagnostics.segments_available,
                "embeddings_available": diagnostics.embeddings_available,
                "streams_represented": diagnostics.streams_represented,
                "can_build_from_tags": diagnostics.can_build_from_tags,
                "segments_with_embeddings": diagnostics.segments_with_embeddings,
                "next_step": guidance.next_step,
                "guidance": guidance.guidance,
            })
        }
        "none" | "rejected" => {
            if let Some(cooldown) = cooldown_days_remaining(&voiceprint) {
                return Ok(json!({
                    "status": "none",
                    "manual_tags_count": diagnostics.manual_tags_count,
                    "segments_available": diagnostics.segments_available,
                    "embeddings_available": diagnostics.embeddings_available,
                    "streams_represented": diagnostics.streams_represented,
                    "can_build_from_tags": diagnostics.can_build_from_tags,
                    "segments_with_embeddings": diagnostics.segments_with_embeddings,
                    "reason": "cooldown",
                    "days_remaining": cooldown,
                    "next_look_after": next_look_after(&voiceprint).map(|time| time.to_rfc3339()),
                    "next_step": "wait_for_cooldown",
                    "guidance": OWNER_REJECTION_COOLDOWN_GUIDANCE,
                }));
            }
            if diagnostics.segments_available > 0 {
                return Ok(json!({
                    "status": "needs_detection",
                    "manual_tags_count": diagnostics.manual_tags_count,
                    "segments_available": diagnostics.segments_available,
                    "embeddings_available": diagnostics.embeddings_available,
                    "streams_represented": diagnostics.streams_represented,
                    "can_build_from_tags": diagnostics.can_build_from_tags,
                    "segments_with_embeddings": diagnostics.segments_with_embeddings,
                    "next_step": "detect_candidate",
                    "guidance": OWNER_DETECT_CANDIDATE_GUIDANCE,
                }));
            }
            manual_fallback(&diagnostics, &manual_stats)
        }
        _ => manual_fallback(&diagnostics, &manual_stats),
    })
}

fn confirmed_status(
    root: &Path,
    principal_id: &str,
    manual_stats: &ManualOwnerTagStats,
    centroid: Option<OwnerCentroidSummary>,
) -> Value {
    let folder = principal_folder(root, principal_id);
    let (streams, intra_cosine_p25) = if centroid.is_some() {
        solstone_core_entity::entity_voiceprints_path(root, &folder)
            .ok()
            .and_then(|path| load_voiceprints(&path))
            .map(|voiceprints| {
                let mut streams = voiceprints
                    .metadata
                    .iter()
                    .filter_map(|row| row.get("stream").and_then(Value::as_str))
                    .filter(|stream| !stream.is_empty())
                    .map(str::to_owned)
                    .collect::<Vec<_>>();
                streams.sort();
                streams.dedup();
                (streams, intra_cosine_p25(&voiceprints.embeddings))
            })
            .unwrap_or_default()
    } else {
        (Vec::new(), None)
    };
    json!({
        "status": "confirmed",
        "centroid_metadata": {
            "cluster_size": centroid.as_ref().map_or(0, |centroid| centroid.cluster_size),
            "cluster_size_capped": centroid.as_ref().is_some_and(|centroid| {
                centroid.cluster_size
                    >= CANDIDATE_EXPANSION_MAX_EMBEDDINGS as i32
            }),
            "streams": streams,
            "created_at": centroid.as_ref().and_then(|centroid| centroid.created_at.clone()),
            "last_refreshed_at": centroid.as_ref().map_or("", |centroid| &centroid.last_refreshed_at),
            "threshold": centroid.as_ref().map(|centroid| centroid.threshold),
            "margin": centroid.as_ref().and_then(|centroid| centroid.margin),
            "intra_cosine_p25": intra_cosine_p25,
            "evidence_hash": centroid.as_ref().and_then(|centroid| centroid.evidence_hash.clone()),
            "evidence_intra_cosine_p25": centroid.as_ref().and_then(|centroid| centroid.evidence_intra_cosine_p25),
            "evidence_tier": centroid.as_ref().map(|centroid| centroid.evidence_tier.clone()),
        },
        "manual_tags_count": manual_stats.manual_tags_count,
    })
}

struct Diagnostics {
    manual_tags_count: usize,
    segments_available: usize,
    embeddings_available: usize,
    streams_represented: usize,
    can_build_from_tags: bool,
    segments_with_embeddings: usize,
}

fn diagnostics(
    root: &Path,
    manual_stats: &ManualOwnerTagStats,
) -> Result<Diagnostics, CatalogBuildError> {
    let (segments_available, embeddings_available) = owner_embedding_inventory(root)?;
    Ok(Diagnostics {
        manual_tags_count: manual_stats.manual_tags_count,
        segments_available,
        embeddings_available,
        streams_represented: manual_stats.streams_represented,
        can_build_from_tags: manual_stats.manual_tags_count >= OWNER_BOOTSTRAP_MIN_STATEMENTS,
        segments_with_embeddings: segments_available,
    })
}

struct ManualGuidance {
    next_step: &'static str,
    guidance: String,
}

fn manual_guidance(stats: &ManualOwnerTagStats) -> ManualGuidance {
    if stats.manual_tags_count >= OWNER_BOOTSTRAP_MIN_STATEMENTS {
        return ManualGuidance {
            next_step: "build_from_tags",
            guidance: format!(
                "You have {} validated owner tags (minimum {}). Run solstone call speakers build-from-tags to save your owner voice; add more with solstone call speakers tag-owner <day> <stream> <segment> <source> <sentence-id> if needed.",
                stats.manual_tags_count, OWNER_BOOTSTRAP_MIN_STATEMENTS,
            ),
        };
    }
    ManualGuidance {
        next_step: "seed_manual_tags",
        guidance: format!(
            "Use solstone call speakers tag-owner <day> <stream> <segment> <source> <sentence-id> on owner sentences in raw media until you have {OWNER_BOOTSTRAP_MIN_STATEMENTS} validated owner tags; {} more needed. Then run solstone call speakers build-from-tags.",
            OWNER_BOOTSTRAP_MIN_STATEMENTS - stats.manual_tags_count,
        ),
    }
}

fn manual_fallback(diagnostics: &Diagnostics, manual_stats: &ManualOwnerTagStats) -> Value {
    let guidance = manual_guidance(manual_stats);
    json!({
        "status": "none",
        "manual_tags_count": diagnostics.manual_tags_count,
        "segments_available": diagnostics.segments_available,
        "embeddings_available": diagnostics.embeddings_available,
        "streams_represented": diagnostics.streams_represented,
        "can_build_from_tags": diagnostics.can_build_from_tags,
        "segments_with_embeddings": diagnostics.segments_with_embeddings,
        "next_step": guidance.next_step,
        "guidance": guidance.guidance,
    })
}

fn owner_embedding_inventory(root: &Path) -> Result<(usize, usize), CatalogBuildError> {
    let mut segments_available = 0;
    let mut embeddings_available = 0;
    for segment in catalog_journal(root)? {
        let sources = audio_embedding_sources(&segment.path);
        if sources.is_empty() {
            continue;
        }
        segments_available += 1;
        for source in sources {
            if segment_overlap_fraction(&segment.path.join(format!("{source}.jsonl"))) > 0.10 {
                continue;
            }
            embeddings_available +=
                npz_row_count(&segment.path.join(format!("{source}.npz")), "embeddings")
                    .unwrap_or_default();
        }
    }
    Ok((segments_available, embeddings_available))
}
