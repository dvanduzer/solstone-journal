// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Atomic publication of complete audio archive tiles, coordination, and fault seams.

use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use chrono::Utc;
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use solstone_core_format::audio_archive::{
    AUDIO_ARCHIVE_PLACEMENT_FILENAME, AUDIO_ARCHIVE_PLACEMENT_VERSION, AudioArchivePlacement,
    AudioArchivePlacementPart, validate_stored_name,
};
use solstone_core_journal_io::{AtomicWriteOptions, LockOptions, atomic_replace};
use solstone_core_segment::output_attempt::{
    OutputClaims, discover_output_claims, prepare_segment,
};
use solstone_core_segment::{
    ImportSource, Kind, StreamHints, UnboundStreamAdvanceError, advance_unbound_stream,
};

use super::AudioArchiveError;
use super::claims::{DEFAULT_WRITE_BUSY_TIMEOUT, find_held_members, insert_tile_claims};
use super::identity::MemberIdentity;
use super::plan::{
    AudioArchiveMemberInput, AudioArchiveSettings, AudioArchiveSize, MemberDisposition,
    PartOutcome, apply_held_members, plan_audio_archive,
};
use super::record::{
    AudioArchiveRecord, compute_size_from_record, plan_to_record, read_audio_archive,
    save_audio_archive_record, validate_resume_settings,
};
use crate::allocate::allocate_import_id;
use crate::heartbeat::{HeartbeatSpawner, ImportHeartbeat};
use crate::metadata::{
    AttemptState, admit_running_attempt, record_completed_attempt, resume_running_attempt,
};
use crate::staging::ensure_import_private_chain;
use crate::stream_name::canonicalize_stream_name;
use crate::timestamp::{Timestamp, validate_timestamp};

/// Injectable fault injection points for crash/recovery testing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArchiveFault {
    /// Fails after the run record is first written to disk.
    AfterRecordFirstSaved,
    /// Fails after raw member bytes for a tile are staged to disk.
    AfterTileBytesStaged,
    /// Fails after claim transaction commits and intent is written, before prepare_segment.
    AfterClaimAndIntent,
    /// Fails after chronicle raws are written, before segment exclusion commit.
    AfterChronicleRawsWritten,
    /// Fails after placement is durable and stream advanced, before marking parts Saved.
    AfterPublishBeforeSavedMark,
    /// Fails after one tile of a multi-tile member is fully bookkept, before the next tile begins.
    BetweenMultiTileParts,
    /// Fails after marking parts Saved, before the emit seam.
    AfterSavedMarkBeforeEmit,
}

pub type EmitObservedFn = Box<dyn Fn(&str, &str, &str) + Send + Sync>;

pub struct AudioArchivePublishSeams {
    pub fault: Option<ArchiveFault>,
    pub emit_observed: Option<EmitObservedFn>,
    pub heartbeat_spawner: Option<HeartbeatSpawner>,
}

pub struct AudioArchivePublishOptions<'a> {
    pub import_id: Option<&'a str>,
    pub members: Vec<AudioArchiveMemberInput>,
    /// Raw member bytes provided by caller, keyed by sha256 or identity.
    pub member_bytes: HashMap<String, Vec<u8>>,
    pub owner_zone: Tz,
    pub settings: AudioArchiveSettings,
    pub source_timestamp: Option<Timestamp>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioArchivePublishResult {
    pub import_id: String,
    pub plan_size: AudioArchiveSize,
    pub tiles_saved: usize,
    pub parts_saved: usize,
    pub record: AudioArchiveRecord,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct TileIntentPart {
    stored_name: String,
    sha256: String,
    byte_len: u64,
    staged_rel_path: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct TileIntent {
    day: String,
    segment_key: String,
    stream: String,
    parts: Vec<TileIntentPart>,
}

struct PublishPart {
    stored_name: String,
    member_identity: MemberIdentity,
    origin_ms: i64,
    offset_ms: i64,
    length_ms: i64,
    original_occurrence: String,
    original_time_ms: Option<i64>,
}

struct PublishTile {
    day: String,
    segment_key: String,
    parts: Vec<PublishPart>,
}

fn validate_day(day: &str) -> Result<(), AudioArchiveError> {
    if day.len() == 8 && day.bytes().all(|b| b.is_ascii_digit()) {
        Ok(())
    } else {
        Err(AudioArchiveError::Other(format!("invalid day '{day}'")))
    }
}

fn validate_segment_key(segment_key: &str) -> Result<(), AudioArchiveError> {
    if solstone_core_format::segment::segment_parse(segment_key).is_none() {
        return Err(AudioArchiveError::Other(format!(
            "invalid segment key '{segment_key}'"
        )));
    }
    let Some((_time, len_str)) = segment_key.split_once('_') else {
        return Err(AudioArchiveError::Other(format!(
            "invalid segment key '{segment_key}'"
        )));
    };
    if len_str != "300" {
        return Err(AudioArchiveError::Other(format!(
            "segment key '{segment_key}' length must be 300"
        )));
    }
    Ok(())
}

fn scan_segment_dir(segment_dir: &Path, tile: &PublishTile) -> Result<(), AudioArchiveError> {
    if !segment_dir.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(segment_dir).map_err(AudioArchiveError::Io)? {
        let entry = entry.map_err(AudioArchiveError::Io)?;
        let name = entry.file_name();
        if solstone_core_journal_io::is_publication_candidate_name(&name) {
            continue;
        }

        let name_str = match name.to_str() {
            Some(s) => s,
            None => {
                return Err(AudioArchiveError::ChronicleSegmentRefused(format!(
                    "segment directory {} contains non-utf8 entry",
                    segment_dir.display()
                )));
            }
        };

        let is_placement = name_str == AUDIO_ARCHIVE_PLACEMENT_FILENAME;
        let is_stream = name_str == "stream.json";
        let is_tile_raw = tile.parts.iter().any(|p| p.stored_name == name_str);
        let is_tile_partial = tile
            .parts
            .iter()
            .any(|p| format!("{}.partial", p.stored_name) == name_str);
        let is_tile_lock = tile
            .parts
            .iter()
            .any(|p| format!("{}.lock", p.stored_name) == name_str);

        if !is_placement && !is_stream && !is_tile_raw && !is_tile_partial && !is_tile_lock {
            return Err(AudioArchiveError::ChronicleSegmentRefused(format!(
                "segment directory {} contains foreign entry '{}'",
                segment_dir.display(),
                name_str
            )));
        }

        let file_type = entry.file_type().map_err(AudioArchiveError::Io)?;
        if !file_type.is_file() {
            return Err(AudioArchiveError::ChronicleSegmentRefused(format!(
                "segment directory {} entry '{}' is not a regular file",
                segment_dir.display(),
                name_str
            )));
        }
    }
    Ok(())
}

/// Orchestrate execution of an indexed audio archive run.
pub fn publish_audio_archive(
    journal_root: &Path,
    options: AudioArchivePublishOptions<'_>,
    seams: Option<AudioArchivePublishSeams>,
) -> Result<AudioArchivePublishResult, AudioArchiveError> {
    let (import_id, stream, plan_size, tiles_to_publish, mut record, generation, _heartbeat) =
        match options.import_id {
            None => {
                let mut plan = plan_audio_archive(
                    &options.members,
                    options.owner_zone,
                    options.settings.clone(),
                )?;

                let held = find_held_members(
                    journal_root,
                    None,
                    &plan.members,
                    DEFAULT_WRITE_BUSY_TIMEOUT,
                )?;
                apply_held_members(&mut plan, &held);

                let stamp = match options.source_timestamp {
                    Some(ts) => ts,
                    None => {
                        let now_str = Utc::now().format("%Y%m%d_%H%M%S").to_string();
                        validate_timestamp(&now_str)
                            .map_err(|e| AudioArchiveError::Other(e.to_string()))?
                    }
                };

                let allocated = allocate_import_id(journal_root, &stamp)
                    .map_err(|e| AudioArchiveError::Other(e.to_string()))?;
                let id = allocated.import_id.as_str().to_owned();

                ensure_import_private_chain(journal_root, &id)
                    .map_err(|e| AudioArchiveError::Other(e.to_string()))?;

                let facts = admit_running_attempt(journal_root, &id, now_ms(), None)
                    .map_err(|e| AudioArchiveError::Other(e.to_string()))?;

                let record = plan_to_record(&plan);
                save_audio_archive_record(journal_root, &id, &record)?;

                check_fault(&seams, ArchiveFault::AfterRecordFirstSaved)?;

                let heartbeat = start_run_heartbeat(
                    journal_root,
                    &id,
                    facts.generation,
                    &facts.attempt_id,
                    &seams,
                );

                let tiles: Vec<PublishTile> = plan
                    .tiles
                    .iter()
                    .map(|tile| {
                        let parts = tile
                            .parts
                            .iter()
                            .map(|p| {
                                let member = plan
                                    .members
                                    .iter()
                                    .find(|m| m.identity == p.member_identity);
                                let orig_time = member
                                    .and_then(|m| m.resolved_occurrence.as_ref())
                                    .and_then(|o| o.original_time_ms());
                                PublishPart {
                                    stored_name: p.stored_name.clone(),
                                    member_identity: p.member_identity.clone(),
                                    origin_ms: p.origin_ms,
                                    offset_ms: p.offset_ms,
                                    length_ms: p.length_ms,
                                    original_occurrence: p.member_identity.occurrence.clone(),
                                    original_time_ms: orig_time,
                                }
                            })
                            .collect();
                        PublishTile {
                            day: tile.day.clone(),
                            segment_key: tile.segment_key.clone(),
                            parts,
                        }
                    })
                    .collect();

                (
                    id,
                    plan.stream,
                    plan.size,
                    tiles,
                    record,
                    facts.generation,
                    heartbeat,
                )
            }
            Some(existing_id) => {
                if validate_timestamp(existing_id).is_err() {
                    return Err(AudioArchiveError::Other(format!(
                        "invalid resume import_id '{existing_id}'"
                    )));
                }
                let id = existing_id.to_owned();
                let existing_record = read_audio_archive(journal_root, &id)?.ok_or_else(|| {
                    AudioArchiveError::MissingRunDirectory(journal_root.join("imports").join(&id))
                })?;

                let stream = canonicalize_stream_name("import", Some(&options.settings.qualifier))
                    .map_err(|e| AudioArchiveError::InvalidStreamName(e.to_string()))?;
                if stream == "import.audio" || options.settings.qualifier == "audio" {
                    return Err(AudioArchiveError::InvalidStreamName(
                        "stream 'import.audio' is refused for indexed audio archives".to_owned(),
                    ));
                }

                validate_resume_settings(
                    &existing_record,
                    options.owner_zone,
                    &options.settings,
                    &stream,
                )?;

                let provenance = crate::metadata::read_provenance(journal_root, &id)
                    .map_err(|e| AudioArchiveError::Other(e.to_string()))?
                    .and_then(|m| crate::metadata::get_attempt_facts(&m));
                let is_running = provenance
                    .as_ref()
                    .map(|p| p.state == AttemptState::Running)
                    .unwrap_or(false);

                let (generation, attempt_id) = if is_running {
                    resume_running_attempt(journal_root, &id)
                        .map_err(|e| AudioArchiveError::Other(e.to_string()))?;
                    let p = provenance.unwrap();
                    (p.generation, p.attempt_id)
                } else {
                    let facts = admit_running_attempt(journal_root, &id, now_ms(), None)
                        .map_err(|e| AudioArchiveError::Other(e.to_string()))?;
                    (facts.generation, facts.attempt_id)
                };

                let heartbeat =
                    start_run_heartbeat(journal_root, &id, generation, &attempt_id, &seams);

                let plan_size = compute_size_from_record(&existing_record);

                let mut tile_map: BTreeMap<(String, String), Vec<PublishPart>> = BTreeMap::new();
                let mut tile_order: Vec<(String, String)> = Vec::new();

                for part in &existing_record.parts {
                    let key = (part.day.clone(), part.segment_key.clone());
                    if !tile_map.contains_key(&key) {
                        tile_order.push(key.clone());
                    }
                    let orig_time = if let Some(rest) = part.member_occurrence.strip_prefix("utc|")
                    {
                        rest.parse::<i64>().ok()
                    } else {
                        None
                    };
                    let member = existing_record
                        .members
                        .iter()
                        .find(|m| {
                            m.sha256 == part.member_sha256
                                && m.canonical_occurrence == part.member_occurrence
                        })
                        .ok_or_else(|| {
                            AudioArchiveError::ClaimDatabaseCorrupt(format!(
                                "part with sha256 '{}' and occurrence '{}' matches no saved member in record",
                                part.member_sha256, part.member_occurrence
                            ))
                        })?;
                    let member_identity = MemberIdentity::new(
                        member.byte_len,
                        &part.member_sha256,
                        &part.member_occurrence,
                    );

                    tile_map.entry(key).or_default().push(PublishPart {
                        stored_name: part.stored_name.clone(),
                        member_identity,
                        origin_ms: part.origin_ms,
                        offset_ms: part.offset_ms,
                        length_ms: part.length_ms,
                        original_occurrence: part.member_occurrence.clone(),
                        original_time_ms: orig_time,
                    });
                }

                let mut tiles = Vec::with_capacity(tile_order.len());
                for key in tile_order {
                    let parts = tile_map.remove(&key).ok_or_else(|| {
                        AudioArchiveError::Other(format!(
                            "missing tile parts for key ({}, {})",
                            key.0, key.1
                        ))
                    })?;
                    tiles.push(PublishTile {
                        day: key.0,
                        segment_key: key.1,
                        parts,
                    });
                }

                (
                    id,
                    stream,
                    plan_size,
                    tiles,
                    existing_record,
                    generation,
                    heartbeat,
                )
            }
        };

    let run_dir = journal_root.join("imports").join(&import_id);
    let mut tiles_saved = 0;
    let mut parts_saved = 0;

    for tile in &tiles_to_publish {
        validate_day(&tile.day)?;
        validate_segment_key(&tile.segment_key)?;
        for part in &tile.parts {
            validate_stored_name(&part.stored_name)
                .map_err(|e| AudioArchiveError::Other(e.to_string()))?;
        }

        let segment_dir = journal_root
            .join("chronicle")
            .join(&tile.day)
            .join(&stream)
            .join(&tile.segment_key);

        scan_segment_dir(&segment_dir, tile)?;

        let placement_path = segment_dir.join(AUDIO_ARCHIVE_PLACEMENT_FILENAME);
        let placement_is_file = fs::symlink_metadata(&placement_path)
            .map(|m| m.file_type().is_file())
            .unwrap_or(false);

        let placement_matches = if placement_is_file {
            let bytes = fs::read(&placement_path).map_err(AudioArchiveError::Io)?;
            let placement =
                serde_json::from_slice::<AudioArchivePlacement>(&bytes).map_err(|e| {
                    AudioArchiveError::ChronicleSegmentRefused(format!(
                        "invalid placement.json in {}: {e}",
                        segment_dir.display()
                    ))
                })?;

            if placement.version != AUDIO_ARCHIVE_PLACEMENT_VERSION
                || placement.parts.len() != tile.parts.len()
            {
                return Err(AudioArchiveError::ChronicleSegmentRefused(format!(
                    "segment directory {} placement.json mismatch: version or part count",
                    segment_dir.display()
                )));
            }

            for planned in &tile.parts {
                let matching = placement.parts.iter().any(|pl| {
                    pl.stored_name == planned.stored_name
                        && pl.original_occurrence == planned.original_occurrence
                        && pl.original_time_ms == planned.original_time_ms
                        && pl.origin_ms == planned.origin_ms
                        && pl.offset_ms == planned.offset_ms
                        && pl.length_ms == planned.length_ms
                });
                if !matching {
                    return Err(AudioArchiveError::ChronicleSegmentRefused(format!(
                        "segment directory {} placement.json part mismatch for stored_name '{}'",
                        segment_dir.display(),
                        planned.stored_name
                    )));
                }
            }
            true
        } else if placement_path.exists() {
            return Err(AudioArchiveError::ChronicleSegmentRefused(format!(
                "placement.json in {} is not a regular file",
                segment_dir.display()
            )));
        } else {
            false
        };

        if placement_matches {
            prepare_segment(&segment_dir).map_err(AudioArchiveError::Io)?;

            let raw_targets: Vec<PathBuf> = tile
                .parts
                .iter()
                .map(|p| segment_dir.join(&p.stored_name))
                .collect();
            let _ = discover_output_claims(&segment_dir).map_err(AudioArchiveError::Io)?;
            let claims = OutputClaims::acquire(raw_targets, LockOptions::default())
                .map_err(AudioArchiveError::Io)?;
            {
                let _commit = claims.commit(&segment_dir).map_err(AudioArchiveError::Io)?;

                let hints = StreamHints {
                    kind: Some(Kind::Imported(ImportSource::Named(
                        options.settings.qualifier.clone(),
                    ))),
                    host: None,
                    platform: None,
                };
                advance_unbound_stream(journal_root, &stream, &tile.day, &tile.segment_key, hints)
                    .map_err(|e| match e {
                        UnboundStreamAdvanceError::Advance(err) => {
                            AudioArchiveError::Other(err.to_string())
                        }
                        UnboundStreamAdvanceError::MarkerWrite { source, .. } => {
                            AudioArchiveError::Io(std::io::Error::other(source.to_string()))
                        }
                    })?;
            }

            let mut all_raws_present = true;
            for part in &tile.parts {
                let raw_path = segment_dir.join(&part.stored_name);
                let raw_is_file = fs::symlink_metadata(&raw_path)
                    .map(|m| m.file_type().is_file())
                    .unwrap_or(false);
                if !raw_is_file {
                    all_raws_present = false;
                }
                for rp in &mut record.parts {
                    if rp.day == tile.day
                        && rp.segment_key == tile.segment_key
                        && rp.stored_name == part.stored_name
                    {
                        rp.outcome = if raw_is_file {
                            PartOutcome::Saved
                        } else {
                            PartOutcome::SavedButUnavailable
                        };
                    }
                }
                parts_saved += 1;
            }
            tiles_saved += 1;
            save_audio_archive_record(journal_root, &import_id, &record)?;

            if all_raws_present
                && let Some(seams_ref) = &seams
                && let Some(emit) = &seams_ref.emit_observed
            {
                emit(&tile.day, &tile.segment_key, &stream);
            }

            continue;
        }

        let tile_staged_dir = run_dir
            .join("staged")
            .join(&tile.day)
            .join(&tile.segment_key);
        fs::create_dir_all(&tile_staged_dir).map_err(AudioArchiveError::Io)?;

        let mut intent_parts = Vec::with_capacity(tile.parts.len());
        let mut tile_member_identities = Vec::with_capacity(tile.parts.len());

        for part in &tile.parts {
            validate_stored_name(&part.stored_name)
                .map_err(|e| AudioArchiveError::Other(e.to_string()))?;
            tile_member_identities.push(part.member_identity.clone());
            let staged_path = tile_staged_dir.join(&part.stored_name);

            let staged_valid = if fs::symlink_metadata(&staged_path)
                .map(|m| m.file_type().is_file())
                .unwrap_or(false)
            {
                let meta = fs::metadata(&staged_path).map_err(AudioArchiveError::Io)?;
                if meta.len() == part.member_identity.byte_len {
                    hash_file(&staged_path)
                        .map(|h| h == part.member_identity.sha256)
                        .unwrap_or(false)
                } else {
                    false
                }
            } else {
                false
            };

            if !staged_valid {
                let bytes = options
                    .member_bytes
                    .get(&part.member_identity.sha256)
                    .or_else(|| {
                        record
                            .members
                            .iter()
                            .find(|m| {
                                m.sha256 == part.member_identity.sha256
                                    && m.canonical_occurrence == part.member_identity.occurrence
                            })
                            .and_then(|m| options.member_bytes.get(&m.original_relative_name))
                    })
                    .ok_or_else(|| AudioArchiveError::HashMismatch {
                        expected: part.member_identity.sha256.clone(),
                        actual: "missing".to_owned(),
                    })?;

                if bytes.len() as u64 != part.member_identity.byte_len {
                    return Err(AudioArchiveError::HashMismatch {
                        expected: format!("len {}", part.member_identity.byte_len),
                        actual: format!("len {}", bytes.len()),
                    });
                }
                let actual_hash = to_hex_lower(&Sha256::digest(bytes));
                if actual_hash != part.member_identity.sha256 {
                    return Err(AudioArchiveError::HashMismatch {
                        expected: part.member_identity.sha256.clone(),
                        actual: actual_hash,
                    });
                }

                let temp_staged_path =
                    tile_staged_dir.join(format!(".tmp_stage_{}.tmp", part.stored_name));
                fs::write(&temp_staged_path, bytes).map_err(AudioArchiveError::Io)?;
                fs::rename(&temp_staged_path, &staged_path).map_err(AudioArchiveError::Io)?;
            }

            intent_parts.push(TileIntentPart {
                stored_name: part.stored_name.clone(),
                sha256: part.member_identity.sha256.clone(),
                byte_len: part.member_identity.byte_len,
                staged_rel_path: format!(
                    "staged/{}/{}/{}",
                    tile.day, tile.segment_key, part.stored_name
                ),
            });

            for rp in &mut record.parts {
                if rp.day == tile.day
                    && rp.segment_key == tile.segment_key
                    && rp.stored_name == part.stored_name
                    && rp.outcome == PartOutcome::Planned
                {
                    rp.outcome = PartOutcome::Staged;
                }
            }
        }

        save_audio_archive_record(journal_root, &import_id, &record)?;
        check_fault(&seams, ArchiveFault::AfterTileBytesStaged)?;

        insert_tile_claims(
            journal_root,
            &import_id,
            &tile_member_identities,
            DEFAULT_WRITE_BUSY_TIMEOUT,
        )?;

        let tiles_dir = run_dir.join("tiles");
        fs::create_dir_all(&tiles_dir).map_err(AudioArchiveError::Io)?;
        let intent = TileIntent {
            day: tile.day.clone(),
            segment_key: tile.segment_key.clone(),
            stream: stream.clone(),
            parts: intent_parts,
        };
        let intent_bytes = serde_json::to_vec_pretty(&intent).map_err(AudioArchiveError::Json)?;
        let intent_path = tiles_dir.join(format!("{}_{}.intent.json", tile.day, tile.segment_key));
        atomic_replace(&intent_path, &intent_bytes, AtomicWriteOptions::default()).map_err(
            |e| {
                AudioArchiveError::Io(std::io::Error::other(format!(
                    "{}: {e}",
                    intent_path.display()
                )))
            },
        )?;

        check_fault(&seams, ArchiveFault::AfterClaimAndIntent)?;

        prepare_segment(&segment_dir).map_err(AudioArchiveError::Io)?;

        let raw_targets: Vec<PathBuf> = tile
            .parts
            .iter()
            .map(|p| segment_dir.join(&p.stored_name))
            .collect();
        let _ = discover_output_claims(&segment_dir).map_err(AudioArchiveError::Io)?;
        let claims = OutputClaims::acquire(raw_targets, LockOptions::default())
            .map_err(AudioArchiveError::Io)?;

        for part in &tile.parts {
            let staged_path = tile_staged_dir.join(&part.stored_name);
            let temp_name = format!("{}.partial", part.stored_name);
            let temp_path = segment_dir.join(&temp_name);

            fs::copy(&staged_path, &temp_path).map_err(AudioArchiveError::Io)?;

            let hash = hash_file(&temp_path)?;
            if hash != part.member_identity.sha256 {
                return Err(AudioArchiveError::HashMismatch {
                    expected: part.member_identity.sha256.clone(),
                    actual: hash,
                });
            }
        }

        check_fault(&seams, ArchiveFault::AfterChronicleRawsWritten)?;

        {
            let _commit = claims.commit(&segment_dir).map_err(AudioArchiveError::Io)?;

            scan_segment_dir(&segment_dir, tile)?;

            for part in &tile.parts {
                let temp_path = segment_dir.join(format!("{}.partial", part.stored_name));
                let final_path = segment_dir.join(&part.stored_name);
                fs::rename(&temp_path, &final_path).map_err(AudioArchiveError::Io)?;
            }

            let placement_parts = tile
                .parts
                .iter()
                .map(|p| AudioArchivePlacementPart {
                    stored_name: p.stored_name.clone(),
                    original_occurrence: p.original_occurrence.clone(),
                    original_time_ms: p.original_time_ms,
                    origin_ms: p.origin_ms,
                    offset_ms: p.offset_ms,
                    length_ms: p.length_ms,
                })
                .collect();

            let placement = AudioArchivePlacement {
                version: AUDIO_ARCHIVE_PLACEMENT_VERSION,
                parts: placement_parts,
            };
            let placement_bytes =
                serde_json::to_vec_pretty(&placement).map_err(AudioArchiveError::Json)?;
            let placement_dest = segment_dir.join(AUDIO_ARCHIVE_PLACEMENT_FILENAME);
            atomic_replace(
                &placement_dest,
                &placement_bytes,
                AtomicWriteOptions::default(),
            )
            .map_err(|e| {
                AudioArchiveError::Io(std::io::Error::other(format!(
                    "{}: {e}",
                    placement_dest.display()
                )))
            })?;

            let hints = StreamHints {
                kind: Some(Kind::Imported(ImportSource::Named(
                    options.settings.qualifier.clone(),
                ))),
                host: None,
                platform: None,
            };
            advance_unbound_stream(journal_root, &stream, &tile.day, &tile.segment_key, hints)
                .map_err(|e| match e {
                    UnboundStreamAdvanceError::Advance(err) => {
                        AudioArchiveError::Other(err.to_string())
                    }
                    UnboundStreamAdvanceError::MarkerWrite { source, .. } => {
                        AudioArchiveError::Io(std::io::Error::other(source.to_string()))
                    }
                })?;
        } // h. Release exclusion

        check_fault(&seams, ArchiveFault::AfterPublishBeforeSavedMark)?;

        for part in &tile.parts {
            for rp in &mut record.parts {
                if rp.day == tile.day
                    && rp.segment_key == tile.segment_key
                    && rp.stored_name == part.stored_name
                {
                    rp.outcome = PartOutcome::Saved;
                }
            }
            parts_saved += 1;
        }
        tiles_saved += 1;
        save_audio_archive_record(journal_root, &import_id, &record)?;

        let has_later_parts = tile.parts.iter().any(|part| {
            tiles_to_publish
                .iter()
                .skip_while(|t| t.day != tile.day || t.segment_key != tile.segment_key)
                .skip(1)
                .any(|later_tile| {
                    later_tile
                        .parts
                        .iter()
                        .any(|later_part| later_part.member_identity == part.member_identity)
                })
        });
        if has_later_parts {
            check_fault(&seams, ArchiveFault::BetweenMultiTileParts)?;
        }

        check_fault(&seams, ArchiveFault::AfterSavedMarkBeforeEmit)?;

        if let Some(seams_ref) = &seams
            && let Some(emit) = &seams_ref.emit_observed
        {
            emit(&tile.day, &tile.segment_key, &stream);
        }
    }

    record_completed_attempt(journal_root, &import_id, generation, now_ms(), None, None)
        .map_err(|e| AudioArchiveError::Other(e.to_string()))?;

    log::info!(
        "audio archive import {import_id} finished: {tiles_saved} tiles saved, {} members included, {} members left out",
        record
            .members
            .iter()
            .filter(|m| m.disposition == MemberDisposition::Included)
            .count(),
        record
            .members
            .iter()
            .filter(|m| m.disposition != MemberDisposition::Included)
            .count()
    );

    Ok(AudioArchivePublishResult {
        import_id,
        plan_size,
        tiles_saved,
        parts_saved,
        record,
    })
}

fn check_fault(
    seams: &Option<AudioArchivePublishSeams>,
    expected: ArchiveFault,
) -> Result<(), AudioArchiveError> {
    if let Some(seams_ref) = seams
        && seams_ref.fault == Some(expected)
    {
        return Err(AudioArchiveError::Fault(expected));
    }
    Ok(())
}

fn start_run_heartbeat(
    journal_root: &Path,
    import_id: &str,
    generation: u64,
    attempt_id: &str,
    seams: &Option<AudioArchivePublishSeams>,
) -> ImportHeartbeat {
    if let Some(seams_ref) = seams
        && let Some(spawner) = seams_ref.heartbeat_spawner
    {
        return ImportHeartbeat::start_injected(
            None,
            Arc::new(|_| {}),
            spawner,
            Arc::new(|| {}),
            Arc::new(|| {}),
            import_id.to_owned(),
            generation,
            attempt_id.to_owned(),
            "audio_archive".to_owned(),
            Instant::now(),
        );
    }

    ImportHeartbeat::start(
        Some(Duration::from_secs(1)),
        journal_root.to_path_buf(),
        import_id.to_owned(),
        generation,
        attempt_id.to_owned(),
        "audio_archive".to_owned(),
        Instant::now(),
    )
}

fn hash_file(path: &Path) -> Result<String, AudioArchiveError> {
    let mut file = File::open(path).map_err(AudioArchiveError::Io)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 8192];
    loop {
        let count = file.read(&mut buffer).map_err(AudioArchiveError::Io)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(to_hex_lower(&hasher.finalize()))
}

fn to_hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(all(unix, test, feature = "full-tests"))]
mod crash_tests {
    use super::*;
    use crate::audio_archive::claims::claim_db_path;
    use crate::audio_archive::identity::MemberStartEvidence;
    use crate::audio_archive::plan::DurationInput;
    use tempfile::TempDir;

    #[test]
    fn crash_seam_recovery_leaves_one_claim_and_staged_bytes() {
        let temp = TempDir::new().unwrap();
        let journal = temp.path();

        let member_bytes = b"synthetic test audio bytes".to_vec();
        let sha256 = format!("{:x}", Sha256::digest(&member_bytes));
        let member = AudioArchiveMemberInput {
            original_relative_name: "archive/test_crash.ogg".to_owned(),
            byte_len: member_bytes.len() as u64,
            sha256: sha256.clone(),
            duration: DurationInput::Milliseconds(10_000),
            start: MemberStartEvidence {
                instant: Some("2026-01-14T21:00:00.500Z".to_owned()),
                local_civil: None,
                source_zone: None,
            },
            run_note: None,
        };
        let mut map = HashMap::new();
        map.insert(sha256, member_bytes.clone());

        let seams = AudioArchivePublishSeams {
            fault: Some(ArchiveFault::AfterClaimAndIntent),
            emit_observed: None,
            heartbeat_spawner: Some(|_, _| Ok(std::thread::spawn(|| {}))),
        };
        let fixed_ts = validate_timestamp("20260114_210000").unwrap();
        let res = publish_audio_archive(
            journal,
            AudioArchivePublishOptions {
                import_id: None,
                members: vec![member],
                member_bytes: map,
                owner_zone: Tz::America__New_York,
                settings: AudioArchiveSettings {
                    qualifier: "clips".to_owned(),
                    archive_identity: "crash_test_archive".to_owned(),
                },
                source_timestamp: Some(fixed_ts),
            },
            Some(seams),
        );
        assert!(matches!(
            res,
            Err(AudioArchiveError::Fault(ArchiveFault::AfterClaimAndIntent))
        ));

        let claim_path = claim_db_path(journal);
        let conn = rusqlite::Connection::open(&claim_path).unwrap();
        let count: i64 = conn
            .query_row("SELECT count(*) FROM audio_archive_claim", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 1);

        let record = read_audio_archive(journal, "20260114_210000")
            .unwrap()
            .expect("audio-archive.json should exist");
        assert_eq!(record.members.len(), 1);

        let part = &record.parts[0];
        let staged_path = journal
            .join("imports")
            .join("20260114_210000")
            .join("staged")
            .join(&part.day)
            .join(&part.segment_key)
            .join(&part.stored_name);
        assert!(staged_path.is_file());
        assert_eq!(std::fs::read(&staged_path).unwrap(), member_bytes);
    }
}
