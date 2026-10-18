// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Bounded processing discovery. Saving and processing are independent durable facts.

use super::claims::{
    claim_db_path, retire_pending_tile_checked, update_pending_tiles_checked,
    update_pending_tiles_for_run,
};
use super::publish::recover_published_tile_checked;
use super::record::ArchiveStore;
use super::{AudioArchiveError, PartOutcome};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrontierPart {
    pub stored_name: String,
    /// Original member digest. Sensing hashes the actual independently playable raw part.
    pub sha256: String,
    pub outcome: PartOutcome,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrontierTile {
    pub import_id: String,
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
/// A bounded batch, not a full run snapshot. Pending means published but not yet processed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioArchiveFrontier {
    pub import_id: String,
    pub published_tiles: Vec<FrontierTile>,
    pub pending_tiles: Vec<FrontierTile>,
    pub unavailable_parts: Vec<FrontierUnavailablePart>,
}
#[derive(Clone, Debug, Default)]
pub struct PendingArchiveBatch {
    pub tiles: Vec<FrontierTile>,
    pub errors: Vec<(String, String)>,
}

fn collect_tile(
    journal: &Path,
    id: &str,
    store: &mut ArchiveStore,
    day: &str,
    key: &str,
    unavailable: &mut Vec<FrontierUnavailablePart>,
    checkpoint: &dyn Fn() -> Result<(), String>,
) -> Result<Option<FrontierTile>, AudioArchiveError> {
    checkpoint().map_err(AudioArchiveError::Other)?;
    if !store.has_tile(day, key)? {
        // A valid replacement run at a reused timestamp does not inherit old work.
        retire_pending_tile_checked(journal, id, day, key, checkpoint)?;
        return Ok(None);
    }
    if !recover_published_tile_checked(journal, id, store, day, key, checkpoint)? {
        return Ok(None);
    }
    let stream = store.settings()?.stream;
    let names: std::collections::HashSet<_> =
        store.pending_part_names(day, key)?.into_iter().collect();
    let mut parts = Vec::new();
    for part in store.tile_parts(day, key)? {
        if part.outcome == PartOutcome::SavedButUnavailable {
            unavailable.push(FrontierUnavailablePart {
                day: day.to_owned(),
                segment_key: key.to_owned(),
                stored_name: part.stored_name.clone(),
                sha256: part.member_sha256.clone(),
            });
        }
        if !names.contains(&part.stored_name) {
            continue;
        }
        let raw = journal
            .join("chronicle")
            .join(day)
            .join(&stream)
            .join(key)
            .join(&part.stored_name);
        let available = match fs::symlink_metadata(raw) {
            Ok(metadata) => metadata.file_type().is_file(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(error.into()),
        };
        if available {
            parts.push(FrontierPart {
                stored_name: part.stored_name,
                sha256: part.member_sha256,
                outcome: PartOutcome::Saved,
            });
        } else {
            store.set_tile_outcomes_checked(
                day,
                key,
                &[(part.stored_name.clone(), PartOutcome::SavedButUnavailable)],
                2,
                checkpoint,
            )?;
            store.finish_processing_checked(day, key, &part.stored_name, true, checkpoint)?;
            unavailable.push(FrontierUnavailablePart {
                day: day.to_owned(),
                segment_key: key.to_owned(),
                stored_name: part.stored_name,
                sha256: part.member_sha256,
            });
        }
    }
    if parts.is_empty() {
        if claim_db_path(journal).try_exists()? {
            retire_pending_tile_checked(journal, id, day, key, checkpoint)?;
        }
        return Ok(None);
    }
    Ok(Some(FrontierTile {
        import_id: id.to_owned(),
        day: day.to_owned(),
        segment_key: key.to_owned(),
        stream,
        parts,
    }))
}

/// Reconcile at most 64 owed tiles. One damaged run cannot suppress other runs' work.
/// Call from the existing blocking discovery worker, never the supervisor event loop.
pub fn update_pending_archive_tiles(
    journal: &Path,
    limit: u32,
) -> Result<PendingArchiveBatch, AudioArchiveError> {
    update_pending_archive_tiles_checked(journal, limit, &|| Ok(()))
}

pub fn update_pending_archive_tiles_checked(
    journal: &Path,
    limit: u32,
    checkpoint: &dyn Fn() -> Result<(), String>,
) -> Result<PendingArchiveBatch, AudioArchiveError> {
    checkpoint().map_err(AudioArchiveError::Other)?;
    let mut batch = PendingArchiveBatch::default();
    let mut stores = HashMap::new();
    for (id, day, key) in update_pending_tiles_checked(journal, limit, checkpoint)? {
        let result = (|| {
            let run = journal.join("imports").join(&id);
            match fs::symlink_metadata(&run) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    retire_pending_tile_checked(journal, &id, &day, &key, checkpoint)?;
                    return Ok(None);
                }
                Err(error) => return Err(error.into()),
                Ok(metadata) if !metadata.file_type().is_dir() => {
                    return Err(AudioArchiveError::ClaimDatabaseCorrupt(format!(
                        "run {id} is not a directory"
                    )));
                }
                Ok(_) => {}
            }
            if !stores.contains_key(&id) {
                stores.insert(id.clone(), ArchiveStore::open(journal, &id, true)?);
            }
            collect_tile(
                journal,
                &id,
                stores.get_mut(&id).expect("opened run store"),
                &day,
                &key,
                &mut Vec::new(),
                checkpoint,
            )
        })();
        match result {
            Ok(Some(tile)) => batch.tiles.push(tile),
            Ok(None) => {}
            Err(error) => batch.errors.push((id, error.to_string())),
        }
    }
    Ok(batch)
}

pub fn update_archive_frontier(
    journal: &Path,
    id: &str,
) -> Result<AudioArchiveFrontier, AudioArchiveError> {
    crate::timestamp::validate_timestamp(id)
        .map_err(|error| AudioArchiveError::Other(error.to_string()))?;
    let run = journal.join("imports").join(id);
    if !run.is_dir() {
        return Err(AudioArchiveError::MissingRunDirectory(run));
    }
    let mut store = ArchiveStore::open(journal, id, true)?;
    let mut coordinates: Vec<_> = update_pending_tiles_for_run(journal, 64, Some(id))?
        .into_iter()
        .map(|(_, day, key)| (day, key))
        .collect();
    if coordinates.is_empty() {
        coordinates = store.pending_tiles(64)?;
    }
    let mut pending = Vec::new();
    let mut unavailable_parts = Vec::new();
    for (day, key) in coordinates {
        if let Some(tile) = collect_tile(
            journal,
            id,
            &mut store,
            &day,
            &key,
            &mut unavailable_parts,
            &|| Ok(()),
        )? {
            pending.push(tile);
        }
    }
    Ok(AudioArchiveFrontier {
        import_id: id.to_owned(),
        published_tiles: pending.clone(),
        pending_tiles: pending,
        unavailable_parts,
    })
}

/// Acknowledge exactly one saved part after the native processing/indexing authorities finish it.
pub fn finish_archive_part(
    journal: &Path,
    id: &str,
    day: &str,
    key: &str,
    name: &str,
    unavailable: bool,
) -> Result<(), AudioArchiveError> {
    finish_archive_part_checked(journal, id, day, key, name, unavailable, &|| Ok(()))
}

pub fn finish_archive_part_checked(
    journal: &Path,
    id: &str,
    day: &str,
    key: &str,
    name: &str,
    unavailable: bool,
    checkpoint: &dyn Fn() -> Result<(), String>,
) -> Result<(), AudioArchiveError> {
    checkpoint().map_err(AudioArchiveError::Other)?;
    let mut store = ArchiveStore::open(journal, id, true)?;
    store.finish_processing_checked(day, key, name, unavailable, checkpoint)?;
    if store.tile_phase(day, key)? == 2 && store.pending_part_names(day, key)?.is_empty() {
        retire_pending_tile_checked(journal, id, day, key, checkpoint)?;
    }
    Ok(())
}
