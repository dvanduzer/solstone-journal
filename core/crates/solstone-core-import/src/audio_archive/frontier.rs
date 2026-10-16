// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Read-only inspection of an audio archive run's frontier across chronicle and imports.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};
use solstone_core_format::audio_archive::AUDIO_ARCHIVE_PLACEMENT_FILENAME;

use super::AudioArchiveError;
use super::plan::PartOutcome;
use super::record::read_audio_archive;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrontierPart {
    pub stored_name: String,
    pub sha256: String,
    pub outcome: PartOutcome,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrontierTile {
    pub day: String,
    pub segment_key: String,
    pub stream: String,
    pub parts: Vec<FrontierPart>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrontierUnavailablePart {
    pub day: String,
    pub segment_key: String,
    pub stored_name: String,
    pub sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioArchiveFrontier {
    pub import_id: String,
    pub published_tiles: Vec<FrontierTile>,
    pub pending_tiles: Vec<FrontierTile>,
    pub unavailable_parts: Vec<FrontierUnavailablePart>,
}

/// Read-only inspection of a run's published, pending, and unavailable parts.
///
/// Refuses missing run directory without creating any files.
pub fn load_frontier(
    journal_root: &Path,
    import_id: &str,
) -> Result<AudioArchiveFrontier, AudioArchiveError> {
    if crate::timestamp::validate_timestamp(import_id).is_err() {
        return Err(AudioArchiveError::Other(format!(
            "invalid import_id '{import_id}'"
        )));
    }
    let run_dir = journal_root.join("imports").join(import_id);
    if !run_dir.is_dir() {
        return Err(AudioArchiveError::MissingRunDirectory(run_dir));
    }

    let record = read_audio_archive(journal_root, import_id)?
        .ok_or_else(|| AudioArchiveError::MissingRunDirectory(run_dir.clone()))?;

    let stream = &record.settings.stream;

    let mut tile_parts: BTreeMap<(String, String), Vec<&super::record::AudioArchiveRecordPart>> =
        BTreeMap::new();
    for part in &record.parts {
        tile_parts
            .entry((part.day.clone(), part.segment_key.clone()))
            .or_default()
            .push(part);
    }

    let mut published_tiles = Vec::new();
    let mut pending_tiles = Vec::new();
    let mut unavailable_parts = Vec::new();

    for ((day, segment_key), parts) in tile_parts {
        let segment_dir = journal_root
            .join("chronicle")
            .join(&day)
            .join(stream)
            .join(&segment_key);

        let placement_file = segment_dir.join(AUDIO_ARCHIVE_PLACEMENT_FILENAME);
        let placement_durable = fs::symlink_metadata(&placement_file)
            .map(|m| m.file_type().is_file())
            .unwrap_or(false);

        let mut tile_is_published = placement_durable;
        let mut frontier_parts = Vec::new();

        for part in &parts {
            let raw_path = segment_dir.join(&part.stored_name);
            let raw_is_file = fs::symlink_metadata(&raw_path)
                .map(|m| m.file_type().is_file())
                .unwrap_or(false);

            let is_saved = part.outcome == PartOutcome::Saved
                || part.outcome == PartOutcome::SavedButUnavailable;

            if is_saved && !raw_is_file {
                unavailable_parts.push(FrontierUnavailablePart {
                    day: day.clone(),
                    segment_key: segment_key.clone(),
                    stored_name: part.stored_name.clone(),
                    sha256: part.member_sha256.clone(),
                });
                frontier_parts.push(FrontierPart {
                    stored_name: part.stored_name.clone(),
                    sha256: part.member_sha256.clone(),
                    outcome: PartOutcome::SavedButUnavailable,
                });
            } else if (placement_durable || is_saved) && raw_is_file {
                frontier_parts.push(FrontierPart {
                    stored_name: part.stored_name.clone(),
                    sha256: part.member_sha256.clone(),
                    outcome: PartOutcome::Saved,
                });
            } else {
                tile_is_published = false;
                let staged_path = run_dir
                    .join("staged")
                    .join(&day)
                    .join(&segment_key)
                    .join(&part.stored_name);
                let staged_is_file = fs::symlink_metadata(&staged_path)
                    .map(|m| m.file_type().is_file())
                    .unwrap_or(false);
                let outcome = if staged_is_file {
                    PartOutcome::Staged
                } else {
                    PartOutcome::Planned
                };
                frontier_parts.push(FrontierPart {
                    stored_name: part.stored_name.clone(),
                    sha256: part.member_sha256.clone(),
                    outcome,
                });
            }
        }

        let tile = FrontierTile {
            day,
            segment_key,
            stream: stream.clone(),
            parts: frontier_parts,
        };

        if tile_is_published {
            published_tiles.push(tile);
        } else {
            pending_tiles.push(tile);
        }
    }

    Ok(AudioArchiveFrontier {
        import_id: import_id.to_owned(),
        published_tiles,
        pending_tiles,
        unavailable_parts,
    })
}
