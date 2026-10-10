// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Read-only media-offload status projection.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde_json::{Value, json};
use solstone_core_backup::get_backup_config;
use solstone_core_journal_io::paths::{PathOrDay, iter_segments, segment_path};

use crate::ledger::{ArchiveWitness, JournalCatalog, summarize_journal};
use crate::measurement::{
    RawMediaPopulation, RawMediaUsage, SuggestedOffloadDefaults, device_free_bytes,
    device_total_bytes, measure_raw_media_usage, suggest_offload_defaults,
};

/// Advisory metadata presence, never hash equality or restore/release authority.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CatalogLocalPresence {
    Present { local_bytes: u64 },
    Absent,
    Unknown { known_local_bytes: Option<u64> },
    Unbound,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogMediaObservation {
    pub day: String,
    pub stream: String,
    pub segment: String,
    pub witness: ArchiveWitness,
    pub presence: CatalogLocalPresence,
}

/// Coverage is retained even when no active witness could be decoded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogMediaStatus {
    pub files: Vec<CatalogMediaObservation>,
    pub local_complete: bool,
    pub catalog_degraded: bool,
}

/// Join active catalog witnesses to checked local metadata by exact locator/name.
///
/// Historical unstored locators remain unbound. An absent observation requires complete
/// coverage; none of these observations certify backup safety or proposed pending relief.
pub fn observe_catalog_media(
    catalog: &JournalCatalog,
    local: &RawMediaPopulation,
) -> CatalogMediaStatus {
    let files = local
        .files
        .iter()
        .map(|file| {
            (
                (
                    file.key.day.as_str(),
                    file.key.stream.as_str(),
                    file.key.segment_dir.as_str(),
                    file.key.layout,
                    file.key.name.as_str(),
                ),
                file.bytes,
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut observations = Vec::new();
    for day in &catalog.days {
        for segment in &day.segments {
            for witness in &segment.active {
                let presence = match (witness.layout, witness.segment_dir.as_ref()) {
                    (Some(layout), Some(segment_dir)) => {
                        let key = (
                            segment.day.as_str(),
                            segment.stream.as_str(),
                            segment_dir.as_str(),
                            layout,
                            witness.file.name.as_str(),
                        );
                        let known = files.get(&key).copied();
                        if catalog.degraded()
                            || day.degraded()
                            || segment.degraded()
                            || !local.complete()
                        {
                            CatalogLocalPresence::Unknown {
                                known_local_bytes: known,
                            }
                        } else if let Some(local_bytes) = known {
                            CatalogLocalPresence::Present { local_bytes }
                        } else {
                            CatalogLocalPresence::Absent
                        }
                    }
                    _ => CatalogLocalPresence::Unbound,
                };
                observations.push(CatalogMediaObservation {
                    day: segment.day.clone(),
                    stream: segment.stream.clone(),
                    segment: segment.segment.clone(),
                    witness: witness.clone(),
                    presence,
                });
            }
        }
    }
    CatalogMediaStatus {
        files: observations,
        local_complete: local.complete(),
        catalog_degraded: catalog.degraded()
            || catalog
                .days
                .iter()
                .any(|day| day.degraded() || day.segments.iter().any(|segment| segment.degraded())),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OffloadStatusMeasurement {
    pub usage: RawMediaUsage,
    pub free_bytes: u64,
    pub total_bytes: u64,
    pub suggested_defaults: SuggestedOffloadDefaults,
}

#[derive(Clone, Debug, PartialEq)]
pub struct OffloadStatus {
    pub value: Value,
}

pub fn measure_offload_status(journal: &Path) -> Result<OffloadStatusMeasurement, String> {
    let total_bytes = device_total_bytes(journal)?;
    Ok(OffloadStatusMeasurement {
        usage: measure_raw_media_usage(journal),
        free_bytes: device_free_bytes(journal)?,
        total_bytes,
        suggested_defaults: suggest_offload_defaults(total_bytes)?,
    })
}

pub fn build_offload_status(journal: &Path) -> Result<OffloadStatus, String> {
    build_offload_status_with(journal, measure_offload_status(journal)?)
}

fn build_offload_status_with(
    journal: &Path,
    measurement: OffloadStatusMeasurement,
) -> Result<OffloadStatus, String> {
    let config = get_backup_config(journal).map_err(|error| error.to_string())?;
    let ledger = summarize_journal(journal);
    let mut raw_by_day = measurement
        .usage
        .per_day
        .iter()
        .map(|day| (day.day.clone(), (day.bytes, day.files)))
        .collect::<BTreeMap<_, _>>();
    let mut backup_only = BTreeMap::<String, (u64, u64, u64, bool, u64, Vec<String>)>::new();
    let mut pending = BTreeMap::<String, (u64, u64, u64)>::new();
    let mut ambiguous = BTreeMap::<String, Vec<String>>::new();
    for day in &ledger.days {
        let mut backup = (0, 0, 0);
        let mut pending_counts = (0, 0, 0);
        for segment in &day.segments {
            if !segment.currently_offloaded {
                continue;
            }
            let listed = iter_segments(journal, PathOrDay::Day(&segment.day))
                .map_err(|error| error.to_string())?;
            let matches: Vec<_> = listed
                .iter()
                .filter(|found| {
                    found.record_identity().ok().is_some_and(|identity| {
                        identity.stream == segment.stream && identity.key == segment.segment
                    })
                })
                .collect();
            if matches.len() > 1 {
                ambiguous
                    .entry(day.day.clone())
                    .or_default()
                    .push(format!("{}/{}", segment.stream, segment.segment));
                continue;
            }
            let directory = match matches.as_slice() {
                [found] => found.path().to_path_buf(),
                _ => segment_path(
                    journal,
                    &segment.day,
                    &segment.segment,
                    &segment.stream,
                    false,
                )
                .map_err(|error| error.to_string())?,
            };
            let (mut has_backup, mut has_pending) = (false, false);
            for file in &segment.files {
                if directory.join(&file.name).is_file() {
                    pending_counts.0 += file.bytes;
                    pending_counts.1 += 1;
                    has_pending = true;
                } else {
                    backup.0 += file.bytes;
                    backup.1 += 1;
                    has_backup = true;
                }
            }
            backup.2 += u64::from(has_backup);
            pending_counts.2 += u64::from(has_pending);
        }
        backup_only.insert(
            day.day.clone(),
            (
                backup.0,
                backup.1,
                backup.2,
                day.degraded(),
                day.skipped_records,
                day.unreadable_ledgers.clone(),
            ),
        );
        pending.insert(day.day.clone(), pending_counts);
    }
    let days = raw_by_day
        .keys()
        .chain(backup_only.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    let pending_bytes: u64 = pending.values().map(|value| value.0).sum();
    let pending_files: u64 = pending.values().map(|value| value.1).sum();
    let backup_bytes: u64 = backup_only.values().map(|value| value.0).sum();
    let backup_files: u64 = backup_only.values().map(|value| value.1).sum();
    let backup_segments: u64 = backup_only.values().map(|value| value.2).sum();
    let payload_days = days.into_iter().map(|day| { let raw = raw_by_day.remove(&day).unwrap_or((0, 0)); let backup = backup_only.remove(&day).unwrap_or((0, 0, 0, false, 0, vec![])); let pending = pending.remove(&day).unwrap_or((0, 0, 0)); let ambiguous_matches = ambiguous.remove(&day).unwrap_or_default(); json!({"day":day,"raw_media_bytes":raw.0.saturating_sub(pending.0),"raw_media_files":raw.1.saturating_sub(pending.1),"backup_only_bytes":backup.0,"backup_only_files":backup.1,"backup_only_segments":backup.2,"degraded":backup.3,"skipped_records":backup.4,"unreadable_ledgers":backup.5,"pending_release_bytes":pending.0,"pending_release_files":pending.1,"pending_release_segments":pending.2,"ambiguous_ledger_matches":ambiguous_matches}) }).collect::<Vec<_>>();
    Ok(OffloadStatus {
        value: json!({
            "offload":config.get("offload").cloned().unwrap_or(Value::Null),
            "last_offload":config.get("last_offload").cloned().unwrap_or(Value::Null),
            "last_verification":config.get("last_verification").cloned().unwrap_or(Value::Null),
            "last_restore":config.get("last_restore").cloned().unwrap_or(Value::Null),
            "device":{"free_bytes":measurement.free_bytes,"total_bytes":measurement.total_bytes},
            "suggested_defaults":{"budget_bytes":measurement.suggested_defaults.budget_bytes,"floor_bytes":measurement.suggested_defaults.floor_bytes},
            "raw_media":{"total_bytes":measurement.usage.total_bytes.saturating_sub(pending_bytes),"total_files":measurement.usage.total_files.saturating_sub(pending_files)},
            "backup_only":{"total_bytes":backup_bytes,"total_files":backup_files,"total_segments":backup_segments,"total_days":ledger.days.iter().filter(|day|day.offloaded_segments>0).count(),"degraded":ledger.degraded(),"skipped_records":ledger.skipped_records,"unreadable_ledgers":ledger.unreadable_ledgers},
            "pending_release":{"total_bytes":pending_bytes,"total_files":pending_files,"total_segments":payload_days.iter().map(|value|value["pending_release_segments"].as_u64().unwrap_or(0)).sum::<u64>(),"total_days":payload_days.iter().filter(|value|value["pending_release_segments"].as_u64().unwrap_or(0)>0).count()},
            "has_ambiguous_ledger_matches": payload_days.iter().any(|value| value["ambiguous_ledger_matches"].as_array().is_some_and(|items| !items.is_empty())),
            "days":payload_days
        }),
    })
}

#[cfg(test)]
mod catalog_tests {
    use super::*;
    use crate::ledger::{DayCatalog, OffloadFile, SegmentCatalog};
    use crate::measurement::{LocalMediaFile, LocalMediaKey};
    use solstone_core_journal_io::paths::SegmentLayout;

    fn fixture() -> (JournalCatalog, RawMediaPopulation) {
        let witness = ArchiveWitness {
            manifest_id: "manifest".into(),
            witness_id: "witness".into(),
            snapshot_id: "a".repeat(64),
            repository_id: Some("repository".into()),
            archive_path: Some("/exact/archive/raw.flac".into()),
            journal_id: Some("journal".into()),
            layout: Some(SegmentLayout::Direct),
            segment_dir: Some("010000_001_full".into()),
            file: OffloadFile {
                name: "raw.flac".into(),
                bytes: 17,
                sha256: "b".repeat(64),
            },
        };
        let catalog = JournalCatalog {
            days: vec![DayCatalog {
                day: "20260101".into(),
                segments: vec![SegmentCatalog {
                    day: "20260101".into(),
                    stream: "_default".into(),
                    segment: "010000_001".into(),
                    active: vec![witness],
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        };
        let local = RawMediaPopulation {
            files: vec![LocalMediaFile {
                key: LocalMediaKey {
                    day: "20260101".into(),
                    stream: "_default".into(),
                    segment: "010000_001".into(),
                    segment_dir: "010000_001_full".into(),
                    layout: SegmentLayout::Direct,
                    name: "raw.flac".into(),
                },
                bytes: 7,
            }],
            unknown_paths: Vec::new(),
        };
        (catalog, local)
    }

    #[test]
    fn exact_presence_is_metadata_only_and_distinguishes_layout_and_full_basename() {
        let (mut catalog, local) = fixture();
        assert_eq!(
            observe_catalog_media(&catalog, &local).files[0].presence,
            CatalogLocalPresence::Present { local_bytes: 7 }
        );
        // Archived size differs: local presence grants no content identity or AlreadyPresent authority.
        assert_eq!(
            observe_catalog_media(&catalog, &local).files[0]
                .witness
                .file
                .bytes,
            17
        );
        catalog.days[0].segments[0].segment = "grouping-only".into();
        assert_eq!(
            observe_catalog_media(&catalog, &local).files[0].presence,
            CatalogLocalPresence::Present { local_bytes: 7 }
        );
        catalog.days[0].segments[0].active[0].layout = Some(SegmentLayout::Named);
        assert_eq!(
            observe_catalog_media(&catalog, &local).files[0].presence,
            CatalogLocalPresence::Absent
        );
        catalog.days[0].segments[0].active[0].layout = Some(SegmentLayout::Direct);
        catalog.days[0].segments[0].active[0].segment_dir = Some("010000_001_other".into());
        assert_eq!(
            observe_catalog_media(&catalog, &local).files[0].presence,
            CatalogLocalPresence::Absent
        );
    }

    #[test]
    fn incomplete_and_degraded_coverage_stays_unknown_with_known_bytes_advisory() {
        let (mut catalog, mut local) = fixture();
        local.unknown_paths.push("unreadable".into());
        assert_eq!(
            observe_catalog_media(&catalog, &local).files[0].presence,
            CatalogLocalPresence::Unknown {
                known_local_bytes: Some(7)
            }
        );
        local.files.clear();
        assert_eq!(
            observe_catalog_media(&catalog, &local).files[0].presence,
            CatalogLocalPresence::Unknown {
                known_local_bytes: None
            }
        );
        local.unknown_paths.clear();
        catalog.days[0].skipped_records = 1;
        assert_eq!(
            observe_catalog_media(&catalog, &local).files[0].presence,
            CatalogLocalPresence::Unknown {
                known_local_bytes: None
            }
        );
        catalog.days[0].skipped_records = 0;
        assert_eq!(
            observe_catalog_media(&catalog, &local).files[0].presence,
            CatalogLocalPresence::Absent
        );
    }

    #[test]
    fn legacy_locator_is_unbound_and_inactive_history_is_not_observed() {
        let (mut catalog, local) = fixture();
        catalog.days[0].segments[0].active[0].layout = None;
        catalog.days[0].segments[0].active[0].segment_dir = None;
        assert_eq!(
            observe_catalog_media(&catalog, &local).files[0].presence,
            CatalogLocalPresence::Unbound
        );
        catalog.days[0].segments[0].active.clear();
        assert!(observe_catalog_media(&catalog, &local).files.is_empty());
    }

    #[test]
    fn empty_observations_preserve_unknown_coverage() {
        let catalog = JournalCatalog {
            unreadable_ledgers: vec!["unreadable-ledger".into()],
            ..Default::default()
        };
        let local = RawMediaPopulation {
            files: Vec::new(),
            unknown_paths: vec!["unreadable-media".into()],
        };
        let status = observe_catalog_media(&catalog, &local);
        assert!(status.files.is_empty());
        assert!(!status.local_complete);
        assert!(status.catalog_degraded);
    }
}

#[cfg(all(test, feature = "full-tests"))]
mod tests {
    use super::*;
    use crate::ledger::{OffloadFile, append_offload_event};
    use std::fs;

    #[test]
    fn status_folds_distinct_multi_day_pending_and_backup_only_media() {
        let journal = tempfile::tempdir().unwrap();
        let first = journal.path().join("chronicle/20260101/010000_001");
        let second = journal.path().join("chronicle/20260102/020000_001");
        fs::create_dir_all(&first).unwrap();
        fs::create_dir_all(&second).unwrap();
        fs::write(first.join("pending.webm"), b"abc").unwrap();
        fs::write(first.join("other.webm"), b"0123456789").unwrap();
        fs::write(second.join("raw.webm"), b"01234567890123456789").unwrap();
        append_offload_event(
            journal.path(),
            "20260101",
            "_default",
            "010000_001",
            "snapshot-a",
            &[OffloadFile {
                name: "pending.webm".into(),
                bytes: 3,
                sha256: "a".repeat(64),
            }],
            1,
        )
        .unwrap();
        append_offload_event(
            journal.path(),
            "20260102",
            "_default",
            "020000_001",
            "snapshot-b",
            &[OffloadFile {
                name: "backup.webm".into(),
                bytes: 7,
                sha256: "b".repeat(64),
            }],
            2,
        )
        .unwrap();
        let value = build_offload_status(journal.path()).unwrap().value;
        assert!(value["offload"]["enabled"].is_boolean());
        let numbers = [
            value["raw_media"]["total_bytes"].as_u64().unwrap(),
            value["pending_release"]["total_bytes"].as_u64().unwrap(),
            value["backup_only"]["total_bytes"].as_u64().unwrap(),
            value["device"]["total_bytes"].as_u64().unwrap(),
        ];
        assert_eq!(numbers[..3], [30, 3, 7]);
        assert_ne!(numbers[0], numbers[1]);
        assert_ne!(numbers[1], numbers[2]);
        assert_ne!(numbers[2], numbers[3]);
    }

    #[test]
    fn status_surfaces_ambiguous_ledger_matches_apart_from_not_offloaded() {
        let journal = tempfile::tempdir().unwrap();
        let first = journal.path().join("chronicle/20260101/other/010000_001_a");
        let second = journal.path().join("chronicle/20260101/other/010000_001_b");
        fs::create_dir_all(&first).unwrap();
        fs::create_dir_all(&second).unwrap();
        append_offload_event(
            journal.path(),
            "20260101",
            "other",
            "010000_001",
            "snapshot-a",
            &[OffloadFile {
                name: "pending.webm".into(),
                bytes: 3,
                sha256: "a".repeat(64),
            }],
            1,
        )
        .unwrap();
        let value = build_offload_status(journal.path()).unwrap().value;
        assert_eq!(value["has_ambiguous_ledger_matches"], true);
        let days = value["days"].as_array().unwrap();
        let day = days.iter().find(|row| row["day"] == "20260101").unwrap();
        assert_eq!(
            day["ambiguous_ledger_matches"],
            serde_json::json!(["other/010000_001"])
        );
        assert_eq!(day["backup_only_segments"].as_u64().unwrap(), 0);
        assert_eq!(day["pending_release_segments"].as_u64().unwrap(), 0);
    }
}
