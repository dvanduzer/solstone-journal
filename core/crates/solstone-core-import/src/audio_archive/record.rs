// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Run record persistence and resume settings verification for indexed audio archives.
//!
//! Stored in `imports/<id>/audio-archive.json`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use solstone_core_journal_io::{AtomicWriteOptions, atomic_replace};

use super::AudioArchiveError;
use super::plan::{
    AUDIO_ARCHIVE_LEN_SECONDS, AUDIO_ARCHIVE_WINDOW_MS, AudioArchivePlan, AudioArchiveSettings,
    AudioArchiveSize, MemberDisposition, PartOutcome,
};

pub const AUDIO_ARCHIVE_RECORD_VERSION: u32 = 1;
pub const AUDIO_ARCHIVE_RECORD_FILENAME: &str = "audio-archive.json";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioArchiveRecordSettings {
    pub owner_zone: String,
    pub window_ms: i64,
    pub stream: String,
    pub qualifier: String,
    pub archive_identity: String,
    pub len: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioArchiveRecordMember {
    pub byte_len: u64,
    pub sha256: String,
    pub canonical_occurrence: String,
    pub original_relative_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_note: Option<String>,
    pub disposition: MemberDisposition,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioArchiveRecordPart {
    pub member_sha256: String,
    pub member_occurrence: String,
    pub day: String,
    pub segment_key: String,
    pub offset_ms: i64,
    pub origin_ms: i64,
    pub length_ms: i64,
    pub stored_name: String,
    pub outcome: PartOutcome,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioArchiveRecord {
    pub version: u32,
    pub settings: AudioArchiveRecordSettings,
    pub members: Vec<AudioArchiveRecordMember>,
    pub parts: Vec<AudioArchiveRecordPart>,
}

pub fn audio_archive_record_path(journal_root: &Path, import_id: &str) -> PathBuf {
    journal_root
        .join("imports")
        .join(import_id)
        .join(AUDIO_ARCHIVE_RECORD_FILENAME)
}

/// Convert a computed plan into the serializable initial run record.
pub fn plan_to_record(plan: &AudioArchivePlan) -> AudioArchiveRecord {
    let settings = AudioArchiveRecordSettings {
        owner_zone: plan.owner_zone.name().to_owned(),
        window_ms: AUDIO_ARCHIVE_WINDOW_MS,
        stream: plan.stream.clone(),
        qualifier: plan.settings.qualifier.clone(),
        archive_identity: plan.settings.archive_identity.clone(),
        len: AUDIO_ARCHIVE_LEN_SECONDS,
    };

    let members = plan
        .members
        .iter()
        .map(|m| AudioArchiveRecordMember {
            byte_len: m.identity.byte_len,
            sha256: m.identity.sha256.clone(),
            canonical_occurrence: m.identity.occurrence.clone(),
            original_relative_name: m.original_relative_name.clone(),
            run_note: m.run_note.clone(),
            disposition: m.disposition.clone(),
        })
        .collect();

    let mut parts = Vec::new();
    for tile in &plan.tiles {
        for part in &tile.parts {
            parts.push(AudioArchiveRecordPart {
                member_sha256: part.member_identity.sha256.clone(),
                member_occurrence: part.member_identity.occurrence.clone(),
                day: part.day.clone(),
                segment_key: part.segment_key.clone(),
                offset_ms: part.offset_ms,
                origin_ms: part.origin_ms,
                length_ms: part.length_ms,
                stored_name: part.stored_name.clone(),
                outcome: part.outcome,
            });
        }
    }

    AudioArchiveRecord {
        version: AUDIO_ARCHIVE_RECORD_VERSION,
        settings,
        members,
        parts,
    }
}

/// Atomically write the audio archive record.
pub fn save_audio_archive_record(
    journal_root: &Path,
    import_id: &str,
    record: &AudioArchiveRecord,
) -> Result<PathBuf, AudioArchiveError> {
    if crate::timestamp::validate_timestamp(import_id).is_err() {
        return Err(AudioArchiveError::Other(format!(
            "invalid import_id '{import_id}'"
        )));
    }
    let path = audio_archive_record_path(journal_root, import_id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(AudioArchiveError::Io)?;
    }

    let bytes = serde_json::to_vec_pretty(record).map_err(AudioArchiveError::Json)?;
    atomic_replace(&path, &bytes, AtomicWriteOptions::default()).map_err(|error| {
        AudioArchiveError::Io(std::io::Error::other(format!(
            "{}: {error}",
            path.display()
        )))
    })?;

    Ok(path)
}

/// Read the audio archive record if present. Read-only: does not create directories.
pub fn read_audio_archive(
    journal_root: &Path,
    import_id: &str,
) -> Result<Option<AudioArchiveRecord>, AudioArchiveError> {
    if crate::timestamp::validate_timestamp(import_id).is_err() {
        return Err(AudioArchiveError::Other(format!(
            "invalid import_id '{import_id}'"
        )));
    }
    let path = audio_archive_record_path(journal_root, import_id);
    match fs::read(&path) {
        Ok(bytes) => {
            let record: AudioArchiveRecord = serde_json::from_slice(&bytes).map_err(|e| {
                AudioArchiveError::ClaimDatabaseCorrupt(format!("{}: {e}", path.display()))
            })?;
            for part in &record.parts {
                if part.day.len() != 8 || !part.day.bytes().all(|b| b.is_ascii_digit()) {
                    return Err(AudioArchiveError::ClaimDatabaseCorrupt(format!(
                        "invalid day in {}: '{}'",
                        path.display(),
                        part.day
                    )));
                }
                if solstone_core_format::segment::segment_parse(&part.segment_key).is_none() {
                    return Err(AudioArchiveError::ClaimDatabaseCorrupt(format!(
                        "invalid segment key in {}: '{}'",
                        path.display(),
                        part.segment_key
                    )));
                }
                let Some((_time, len_str)) = part.segment_key.split_once('_') else {
                    return Err(AudioArchiveError::ClaimDatabaseCorrupt(format!(
                        "invalid segment key in {}: '{}'",
                        path.display(),
                        part.segment_key
                    )));
                };
                if len_str != "300" {
                    return Err(AudioArchiveError::ClaimDatabaseCorrupt(format!(
                        "invalid segment key length in {}: expected 300, got '{len_str}'",
                        path.display()
                    )));
                }
                solstone_core_format::audio_archive::validate_stored_name(&part.stored_name)
                    .map_err(|e| {
                        AudioArchiveError::ClaimDatabaseCorrupt(format!(
                            "invalid stored name in {}: '{e}'",
                            path.display()
                        ))
                    })?;
            }
            Ok(Some(record))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(AudioArchiveError::Io(e)),
    }
}

/// Validate that a resumed run's settings match caller inputs exactly.
pub fn validate_resume_settings(
    record: &AudioArchiveRecord,
    owner_zone: Tz,
    settings: &AudioArchiveSettings,
    stream: &str,
) -> Result<(), AudioArchiveError> {
    if record.settings.owner_zone != owner_zone.name() {
        return Err(AudioArchiveError::SettingsMismatch(format!(
            "owner zone mismatch: saved '{}', requested '{}'",
            record.settings.owner_zone,
            owner_zone.name()
        )));
    }
    if record.settings.stream != stream {
        return Err(AudioArchiveError::SettingsMismatch(format!(
            "stream mismatch: saved '{}', requested '{}'",
            record.settings.stream, stream
        )));
    }
    if record.settings.qualifier != settings.qualifier {
        return Err(AudioArchiveError::SettingsMismatch(format!(
            "qualifier mismatch: saved '{}', requested '{}'",
            record.settings.qualifier, settings.qualifier
        )));
    }
    if record.settings.archive_identity != settings.archive_identity {
        return Err(AudioArchiveError::SettingsMismatch(format!(
            "archive identity mismatch: saved '{}', requested '{}'",
            record.settings.archive_identity, settings.archive_identity
        )));
    }
    if record.settings.window_ms != AUDIO_ARCHIVE_WINDOW_MS {
        return Err(AudioArchiveError::SettingsMismatch(format!(
            "window_ms mismatch: saved '{}', expected '{AUDIO_ARCHIVE_WINDOW_MS}'",
            record.settings.window_ms
        )));
    }
    if record.settings.len != AUDIO_ARCHIVE_LEN_SECONDS {
        return Err(AudioArchiveError::SettingsMismatch(format!(
            "len mismatch: saved '{}', expected '{AUDIO_ARCHIVE_LEN_SECONDS}'",
            record.settings.len
        )));
    }
    Ok(())
}

/// Compute summary size metrics from an existing record without re-running the planner.
pub fn compute_size_from_record(record: &AudioArchiveRecord) -> AudioArchiveSize {
    let mut bytes = 0u64;
    let mut days_set = BTreeSet::new();
    let mut left_out_map = BTreeMap::new();

    for member in &record.members {
        match &member.disposition {
            MemberDisposition::Included => {
                bytes += member.byte_len;
            }
            MemberDisposition::LeftOut(reason) => {
                *left_out_map
                    .entry(reason.kind_key().to_owned())
                    .or_insert(0) += 1;
            }
        }
    }

    let mut tiles_set = BTreeSet::new();
    for part in &record.parts {
        days_set.insert(part.day.clone());
        tiles_set.insert((part.day.clone(), part.segment_key.clone()));
    }

    let span =
        if let (Some(first), Some(last)) = (days_set.iter().next(), days_set.iter().next_back()) {
            Some((first.clone(), last.clone()))
        } else {
            None
        };

    AudioArchiveSize {
        members: record.members.len(),
        bytes,
        days: days_set.len(),
        span,
        tiles: tiles_set.len(),
        left_out: left_out_map,
    }
}
