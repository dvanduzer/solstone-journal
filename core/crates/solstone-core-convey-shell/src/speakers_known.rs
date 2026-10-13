// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

use axum::extract::Query;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde::Deserialize;
use serde::Serialize;
use serde_json::{Value, json};
use solstone_core_convey_http::envelope::error_envelope;

use crate::JournalRoot;
use crate::speakers_calendar::live_entities;
use crate::speakers_npz::load_voiceprints;
use crate::speakers_review::is_admissible_speaker_entity;
use solstone_core_speaker_resolve::owner_detection::intra_cosine_p25;

const SORT_RECENT: &str = "recent";
const SORT_MOST_SAMPLES: &str = "most samples";
const SORT_ALPHABETICAL: &str = "alphabetical";

#[derive(Deserialize)]
pub struct KnownQuery {
    sort: Option<String>,
}

pub async fn known(
    Extension(root): Extension<Arc<JournalRoot>>,
    Query(query): Query<KnownQuery>,
) -> Response {
    let sort = query
        .sort
        .filter(|sort| !sort.is_empty())
        .unwrap_or_else(|| SORT_RECENT.to_owned())
        .replace('_', " ");
    if !matches!(
        sort.as_str(),
        SORT_RECENT | SORT_MOST_SAMPLES | SORT_ALPHABETICAL
    ) {
        return error_envelope(
            "invalid_request_value",
            "one of those values couldn't be used.",
            "Invalid sort parameter",
            StatusCode::BAD_REQUEST,
        )
        .into_response();
    }
    solstone_core_convey_http::owner_read::spawn_blocking_response(
        solstone_core_convey_http::owner_read::OwnerReadRole::SpeakersKnown,
        move || {
            let mut speakers = known_speakers(&root.0);
            sort_speakers(&mut speakers, &sort);
            Json(json!({"speakers": speakers, "total": speakers.len(), "sort": sort}))
                .into_response()
        },
    )
    .await
}

#[derive(Serialize)]
struct Speaker {
    entity_id: String,
    name: String,
    embedding_count: usize,
    segment_count: usize,
    streams: Vec<String>,
    last_seen_ts: Option<i64>,
    intra_cosine_p25: Option<f64>,
}

fn known_speakers(root: &Path) -> Vec<Speaker> {
    live_entities(root)
        .into_iter()
        .filter_map(|(dir, entity)| {
            let entity_id = entity.id;
            let entity = entity.value;
            if !is_admissible_speaker_entity(&entity) {
                return None;
            }
            let voiceprints =
                load_voiceprints(&solstone_core_entity::entity_voiceprints_path(root, &dir).ok()?)?;
            let mut streams = BTreeSet::new();
            let mut segments = BTreeSet::new();
            let mut last_seen = None;
            for metadata in &voiceprints.metadata {
                if let Some(stream) = metadata.get("stream").and_then(Value::as_str)
                    && !stream.is_empty()
                {
                    streams.insert(stream.to_owned());
                }
                let day = metadata
                    .get("day")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let segment_key = metadata
                    .get("segment_key")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let segment = (day.to_owned(), segment_key.to_owned());
                segments.insert(segment);
                if let Some(last_seen_ts) = metadata.get("last_seen_ts").and_then(Value::as_i64) {
                    last_seen = Some(
                        last_seen.map_or(last_seen_ts, |previous: i64| previous.max(last_seen_ts)),
                    );
                }
            }
            let name = entity_name(&entity, &entity_id);
            Some(Speaker {
                entity_id,
                name,
                embedding_count: voiceprints.embeddings.len(),
                segment_count: segments.len(),
                streams: streams.into_iter().collect(),
                last_seen_ts: last_seen,
                intra_cosine_p25: intra_cosine_p25(&voiceprints.embeddings),
            })
        })
        .collect()
}

fn entity_name(entity: &Value, entity_id: &str) -> String {
    entity
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| entity_id.to_owned())
}

fn sort_speakers(speakers: &mut [Speaker], sort: &str) {
    speakers.sort_by(|left, right| match sort {
        SORT_MOST_SAMPLES => right
            .embedding_count
            .cmp(&left.embedding_count)
            .then_with(|| name_key(left).cmp(&name_key(right)))
            .then_with(|| left.entity_id.cmp(&right.entity_id)),
        SORT_ALPHABETICAL => name_key(left)
            .cmp(&name_key(right))
            .then_with(|| left.entity_id.cmp(&right.entity_id)),
        _ => left
            .last_seen_ts
            .is_none()
            .cmp(&right.last_seen_ts.is_none())
            .then_with(|| {
                right
                    .last_seen_ts
                    .unwrap_or_default()
                    .cmp(&left.last_seen_ts.unwrap_or_default())
            })
            .then_with(|| name_key(left).cmp(&name_key(right)))
            .then_with(|| left.entity_id.cmp(&right.entity_id)),
    });
}

fn name_key(speaker: &Speaker) -> String {
    speaker.name.to_lowercase()
}

/// Compute p25 with a float32 GEMM, matching Python's algorithm.
///
/// This is the fourth permanent narrow frozen-oracle deviation, alongside the
/// ETag value, unregistered-extension 500→refusal, and chat-bar placeholder:
/// BLAS backends may reduce float32 products in different non-associative
/// orders. The resulting last-bit variance is numerical, not behavioral.
#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use serde_json::json;

    use super::known_speakers;

    #[test]
    fn known_speakers_excludes_non_person_and_blocked_voiceprints() {
        let root = std::env::temp_dir().join(format!(
            "solstone-known-speaker-admission-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        for (id, entity) in [
            ("person", json!({"id":"person","type":"Person"})),
            ("tool", json!({"id":"tool","type":"Tool"})),
            (
                "blocked",
                json!({"id":"blocked","type":"Person","blocked":true}),
            ),
        ] {
            let entity_dir = root.join("entities").join(id);
            fs::create_dir_all(&entity_dir).expect("entity directory");
            fs::write(
                entity_dir.join("entity.json"),
                serde_json::to_vec(&entity).expect("entity json"),
            )
            .expect("entity writes");
            solstone_core_entity::save_voiceprints_batch(
                &root,
                id,
                &[solstone_core_entity::VoiceprintItem {
                    embedding: vec![1.0; 256],
                    metadata: json!({"day":"20260808","stream":"main","segment_key":"120000_1","source":"audio","sentence_id":1}),
                }],
                &solstone_core_entity::EncoderIdentity {
                    id: "test".to_owned(),
                    sha256: "0".repeat(64),
                    width: 256,
                },
            )
            .expect("voiceprint writes");
        }

        let ids = known_speakers(&root)
            .into_iter()
            .map(|speaker| speaker.entity_id)
            .collect::<Vec<_>>();
        assert_eq!(ids, vec!["person"]);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn known_speakers_are_keyed_by_id_with_the_voice_from_the_folder_holding_them() {
        let root = std::env::temp_dir().join(format!(
            "solstone-known-speaker-folders-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        for (folder, entity) in [
            // Its file names another id than its folder.
            ("a", json!({"id":"bee","name":"Bee","type":"Person"})),
            // No id written: the folder is its id.
            ("cee", json!({"name":"Cee","type":"Person"})),
            // Claims `bee` too, and loses to `a`, the first folder in order.
            ("zz", json!({"id":"bee","name":"Impostor","type":"Person"})),
        ] {
            let entity_dir = root.join("entities").join(folder);
            fs::create_dir_all(&entity_dir).expect("entity directory");
            fs::write(
                entity_dir.join("entity.json"),
                serde_json::to_vec(&entity).expect("entity json"),
            )
            .expect("entity writes");
        }
        for id in ["bee", "cee"] {
            solstone_core_entity::save_voiceprints_batch(
                &root,
                id,
                &[solstone_core_entity::VoiceprintItem {
                    embedding: vec![1.0; 256],
                    metadata: json!({"day":"20260808","stream":"main","segment_key":"120000_1","source":"audio","sentence_id":1}),
                }],
                &solstone_core_entity::EncoderIdentity {
                    id: "test".to_owned(),
                    sha256: "0".repeat(64),
                    width: 256,
                },
            )
            .expect("voiceprint writes");
        }
        // The loser has a voice too, so only the one-per-id rule leaves it out.
        fs::copy(
            root.join("entities/a/voiceprints.npz"),
            root.join("entities/zz/voiceprints.npz"),
        )
        .expect("loser voice");

        let speakers = known_speakers(&root)
            .into_iter()
            .map(|speaker| (speaker.entity_id, speaker.name))
            .collect::<Vec<_>>();
        assert_eq!(
            speakers,
            vec![
                ("bee".to_owned(), "Bee".to_owned()),
                ("cee".to_owned(), "Cee".to_owned())
            ]
        );
        let _ = fs::remove_dir_all(root);
    }
}
