// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! The location write owner: one derived document per day, for search.
//!
//! `chronicle/<day>/location.md` holds the day's area words as plain text —
//! *"readings near Golden, Colorado, United States, 14:00 to 14:09"* — so the
//! existing search finds the day, and the owner's agents read the words through
//! the existing fetch. It never holds a coordinate. Beside it,
//! `chronicle/<day>/health/location.json` records which state of the day's
//! segments the document was built from: the day's stream marker, which every
//! accepted upload, every segment removal and every archive merge advances.
//!
//! Rebuilding is deterministic. An unchanged day writes nothing; a day whose
//! segments changed is rebuilt from the segments that survive, and a day left
//! with no location loses both files and its rows in the index.

use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use chrono::NaiveDate;
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use solstone_core_journal_io::{
    AtomicWriteOptions, HealthMarkerKind, HealthMarkerState, PathOrDay, atomic_replace, day_dirs,
    iter_segments, read_health_marker,
};

use crate::area::{AreaName, AreaTier};
use crate::asset::AREA_TIER_SHA256;
use crate::day::{DayLocation, DeviceDirectory, Entry, LOCATION_FILE, LocalTime, read_day};

/// The day's search document, under `chronicle/<day>/`.
pub const DAY_DOCUMENT: &str = "location.md";
/// The build record, under `chronicle/<day>/health/`.
pub const BUILD_RECORD: &str = "location.json";
const RECORD_VERSION: u32 = 1;
/// Bumped whenever [`render_day_document`] changes what it writes, so every
/// day's document is rewritten once by the next pass.
const DOCUMENT_VERSION: u32 = 1;

/// What a day's document was built from, and what it counted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildRecord {
    pub version: u32,
    /// The day's stream marker when the build read its segments.
    pub source: String,
    /// The area tier the names came from.
    pub area_tier: String,
    /// The version of the document's wording.
    pub document: u32,
    pub readings: u64,
    pub visits: u64,
    pub devices: u64,
}

/// A rebuild could not finish.
#[derive(Debug)]
pub enum ProjectionError {
    Write { path: PathBuf, detail: String },
    Index { path: PathBuf, detail: String },
}

impl fmt::Display for ProjectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Write { path, detail } => {
                write!(formatter, "could not write {}: {detail}", path.display())
            }
            Self::Index { path, detail } => write!(
                formatter,
                "wrote {} but the search index could not take it: {detail}",
                path.display()
            ),
        }
    }
}

impl std::error::Error for ProjectionError {}

/// What a rebuild did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rebuilt {
    /// The document already said exactly this.
    Unchanged,
    Written,
    /// The day no longer holds location; its files are gone.
    Removed,
    /// The day never held location.
    Nothing,
}

pub fn document_path(journal: &Path, day: &str) -> PathBuf {
    journal.join("chronicle").join(day).join(DAY_DOCUMENT)
}

pub fn record_path(journal: &Path, day: &str) -> PathBuf {
    journal
        .join("chronicle")
        .join(day)
        .join("health")
        .join(BUILD_RECORD)
}

/// The day's stream marker as a comparable token. Unreadable reads as its own
/// token, so it is retried rather than mistaken for unchanged.
pub fn source_token(journal: &Path, day: &str) -> String {
    let nanos = |modified: std::time::SystemTime| {
        modified
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos())
    };
    match read_health_marker(journal, day, HealthMarkerKind::Stream) {
        Ok(HealthMarkerState::Absent) => "absent".to_owned(),
        Ok(HealthMarkerState::LegacyEmpty { modified }) => format!("legacy:{}", nanos(modified)),
        Ok(HealthMarkerState::MalformedNonEmpty { modified }) => {
            format!("malformed:{}", nanos(modified))
        }
        Ok(HealthMarkerState::Versioned { marker, .. }) => format!(
            "g{}:{}",
            marker.generation,
            marker.fingerprint.unwrap_or_default()
        ),
        Err(error) => format!("unreadable:{error}"),
    }
}

/// Read a day's build record, if it has a current one.
pub fn read_build_record(journal: &Path, day: &str) -> Option<BuildRecord> {
    let bytes = fs::read(record_path(journal, day)).ok()?;
    serde_json::from_slice::<BuildRecord>(&bytes)
        .ok()
        .filter(|record| record.version == RECORD_VERSION)
}

fn long_name(area: &AreaName) -> Option<String> {
    let label = area.label()?;
    let Some(town) = &area.town else {
        return Some(format!("with no named town nearby ({label})"));
    };
    let mut name = format!("near {}", town.name);
    if let Some(ascii) = &town.ascii_name {
        name.push_str(&format!(" ({ascii})"));
    }
    if let Some(region) = &town.region
        && region != &town.name
    {
        name.push_str(&format!(", {region}"));
    }
    name.push_str(&format!(", {}", town.country));
    Some(name)
}

fn moment(time: &LocalTime, day: &str) -> String {
    let local_day = time.day();
    if local_day == day {
        return time.clock();
    }
    match NaiveDate::parse_from_str(&local_day, "%Y%m%d") {
        Ok(date) => format!("{} at {}", date.format("%B %-d"), time.clock()),
        Err(_) => time.clock(),
    }
}

/// The day's search text. It names areas and times, never a coordinate.
pub fn render_day_document(day: &DayLocation) -> String {
    let heading = NaiveDate::parse_from_str(&day.day, "%Y%m%d")
        .map(|date| date.format("%A, %B %-d, %Y").to_string())
        .unwrap_or_else(|_| day.day.clone());
    let mut text = format!("# location on {heading}\n\n");
    let mut lines = 0;
    for entry in &day.entries {
        match entry {
            Entry::Readings {
                device,
                area,
                precision,
                first,
                last,
                points,
                ..
            } => {
                let Some(name) = long_name(area) else {
                    continue;
                };
                let when = if first.utc == last.utc {
                    format!("at {}", moment(first, &day.day))
                } else {
                    format!("{} to {}", moment(first, &day.day), moment(last, &day.day))
                };
                let approximate = matches!(
                    precision,
                    crate::record::Precision::Approximate | crate::record::Precision::Reduced
                );
                let count = match (points.len(), approximate) {
                    (1, false) => "1 reading".to_owned(),
                    (1, true) => "1 approximate reading".to_owned(),
                    (count, false) => format!("{count} readings"),
                    (count, true) => format!("{count} approximate readings"),
                };
                text.push_str(&format!(
                    "- readings {name}, {when}: {count} from {}.\n",
                    day.device_phrase(*device)
                ));
                lines += 1;
            }
            Entry::Visit {
                device,
                area,
                arrival,
                departure,
                ..
            } => {
                let Some(name) = long_name(area) else {
                    continue;
                };
                let arrived = arrival.as_ref().map_or_else(
                    || "arrival not known".to_owned(),
                    |time| format!("arrived {}", moment(time, &day.day)),
                );
                let left = departure.as_ref().map_or_else(
                    || "no departure reported".to_owned(),
                    |time| format!("left {}", moment(time, &day.day)),
                );
                text.push_str(&format!(
                    "- a visit {} reported {name}: {arrived}, {left}.\n",
                    day.device_phrase(*device)
                ));
                lines += 1;
            }
            Entry::Unreadable { .. } => {}
        }
    }
    if lines == 0 {
        text.push_str("- location was kept, but no reading could be named.\n");
    }
    text
}

fn write_file(path: &Path, bytes: &[u8]) -> Result<(), ProjectionError> {
    atomic_replace(path, bytes, AtomicWriteOptions::default()).map_err(|error| {
        ProjectionError::Write {
            path: path.to_path_buf(),
            detail: error.to_string(),
        }
    })
}

fn remove(path: &Path) -> Result<bool, ProjectionError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(ProjectionError::Write {
            path: path.to_path_buf(),
            detail: error.to_string(),
        }),
    }
}

fn index_document(journal: &Path, path: &Path) -> Result<(), ProjectionError> {
    solstone_core_indexer_store::scan::rescan_file(journal, path)
        .map(|_| ())
        .map_err(|error| ProjectionError::Index {
            path: path.to_path_buf(),
            detail: error.to_string(),
        })
}

fn forget_document(journal: &Path, day: &str) -> Result<(), ProjectionError> {
    let rel = format!("{day}/{DAY_DOCUMENT}");
    solstone_core_indexer_store::db::prune_by_paths(journal, &[rel.as_str()])
        .map(|_| ())
        .map_err(|error| ProjectionError::Index {
            path: document_path(journal, day),
            detail: error.to_string(),
        })
}

/// Rebuild one day's document from the segments it holds now.
pub fn rebuild_day(
    journal: &Path,
    day: &str,
    tier: &AreaTier,
    directory: &dyn DeviceDirectory,
    owner: Tz,
) -> Result<Rebuilt, ProjectionError> {
    // Read the marker before the segments: a removal that lands while this
    // build reads advances it, and the next pass rebuilds again.
    let source = source_token(journal, day);
    let view = read_day(journal, day, tier, directory, owner);
    let document = document_path(journal, day);
    let record = record_path(journal, day);
    if view.is_empty() {
        let had_document = remove(&document)?;
        let had_record = remove(&record)?;
        if had_document {
            forget_document(journal, day)?;
        }
        return Ok(if had_document || had_record {
            Rebuilt::Removed
        } else {
            Rebuilt::Nothing
        });
    }
    let text = render_day_document(&view);
    let built = BuildRecord {
        version: RECORD_VERSION,
        source,
        area_tier: AREA_TIER_SHA256.to_owned(),
        document: DOCUMENT_VERSION,
        readings: view.readings() as u64,
        visits: view.visits() as u64,
        devices: view.devices.len() as u64,
    };
    let unchanged = fs::read(&document).is_ok_and(|existing| existing == text.as_bytes());
    if !unchanged {
        write_file(&document, text.as_bytes())?;
    }
    if read_build_record(journal, day).as_ref() != Some(&built) {
        let mut bytes = serde_json::to_vec_pretty(&built).unwrap_or_default();
        bytes.push(b'\n');
        write_file(&record, &bytes)?;
    }
    if unchanged {
        return Ok(Rebuilt::Unchanged);
    }
    index_document(journal, &document)?;
    Ok(Rebuilt::Written)
}

fn holds_location(journal: &Path, day: &str) -> bool {
    iter_segments(journal, PathOrDay::Day(day))
        .unwrap_or_default()
        .iter()
        .any(|segment| segment.path().join(LOCATION_FILE).is_file())
}

/// What one reconcile pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    pub checked: usize,
    pub written: usize,
    pub removed: usize,
    pub failures: Vec<String>,
}

/// Remembers, across passes, which days are already current.
#[derive(Debug, Default)]
pub struct Reconciler {
    seen: HashMap<String, String>,
}

impl Reconciler {
    /// Bring every day's document up to date with its segments. A day whose
    /// marker has not moved since the last pass costs one small read.
    pub fn reconcile(
        &mut self,
        journal: &Path,
        tier: &AreaTier,
        directory: &dyn DeviceDirectory,
        owner: Tz,
    ) -> ReconcileReport {
        let mut report = ReconcileReport::default();
        let mut days: Vec<String> = day_dirs(journal)
            .map(|days| days.into_keys().collect())
            .unwrap_or_default();
        days.sort_unstable();
        self.seen.retain(|day, _| days.binary_search(day).is_ok());
        for day in days {
            report.checked += 1;
            let token = source_token(journal, &day);
            if self.seen.get(&day) == Some(&token) {
                continue;
            }
            let current = read_build_record(journal, &day).is_some_and(|record| {
                record.source == token
                    && record.area_tier == AREA_TIER_SHA256
                    && record.document == DOCUMENT_VERSION
            });
            if current && document_path(journal, &day).is_file() {
                self.seen.insert(day, token);
                continue;
            }
            let has_files =
                document_path(journal, &day).exists() || record_path(journal, &day).exists();
            if !has_files && !holds_location(journal, &day) {
                self.seen.insert(day, token);
                continue;
            }
            match rebuild_day(journal, &day, tier, directory, owner) {
                Ok(Rebuilt::Written) => report.written += 1,
                Ok(Rebuilt::Removed) => report.removed += 1,
                Ok(Rebuilt::Unchanged | Rebuilt::Nothing) => {}
                Err(error) => {
                    report.failures.push(format!("{day}: {error}"));
                    continue;
                }
            }
            self.seen.insert(day, token);
        }
        report
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::day::tests::{directory, fix, iphone_header, tier, write};

    const DAY: &str = "20261001";

    fn seed(root: &Path) {
        let header = iphone_header("full", "full");
        write(
            root,
            DAY,
            "phone_mobile-segment",
            "140000_300",
            &format!(
                "{header}\n{}\n{}\n",
                fix("2026-10-01T20:00:10Z", 39.7555, -105.2211, 10.0),
                fix("2026-10-01T20:04:10Z", 39.7556, -105.2212, 10.0)
            ),
        );
        write(
            root,
            DAY,
            "phone_mobile-segment",
            "150000_300",
            &format!(
                "{header}\n{}\n",
                r#"{"arrival":"2026-10-01T21:02:00Z","departure":"2026-10-01T22:40:00Z","h_acc":40,"lat":39.7527,"lon":-105.0001,"schema":"solstone.location.visit/1"}"#
            ),
        );
    }

    fn rebuild(root: &Path) -> Rebuilt {
        rebuild_day(root, DAY, tier(), &directory(), chrono_tz::America::Denver).unwrap()
    }

    #[test]
    fn the_document_holds_area_words_and_times_and_no_coordinates() {
        let root = tempfile::tempdir().unwrap();
        seed(root.path());
        assert_eq!(rebuild(root.path()), Rebuilt::Written);
        let text = fs::read_to_string(document_path(root.path(), DAY)).unwrap();
        assert!(text.starts_with("# location on Thursday, October 1, 2026\n"));
        assert!(text.contains(
            "- readings near Golden, Colorado, United States, 14:00 to 14:04: 2 readings from your iphone."
        ));
        assert!(text.contains(
            "- a visit your iphone reported near Denver, Colorado, United States: arrived 15:02, left 16:40."
        ));
        for coordinate in ["39.75", "105.2", "105.0"] {
            assert!(
                !text.contains(coordinate),
                "{coordinate} leaked into {text}"
            );
        }
        let record = read_build_record(root.path(), DAY).unwrap();
        assert_eq!((record.readings, record.visits, record.devices), (2, 1, 1));
        assert!(matches!(
            solstone_core_indexer_store::scan::rescan_file(
                root.path(),
                &document_path(root.path(), DAY)
            ),
            Ok(solstone_core_indexer_store::scan::RescanFileStatus::Indexed { .. })
        ));
    }

    #[test]
    fn a_reading_far_from_any_town_reads_as_a_sentence_without_in() {
        let root = tempfile::tempdir().unwrap();
        let header = iphone_header("full", "full");
        write(
            root.path(),
            DAY,
            "phone_mobile-segment",
            "120000_300",
            &format!(
                "{header}\n{}\n",
                fix("2026-10-01T18:00:10Z", 43.0, -107.5, 10.0)
            ),
        );
        rebuild(root.path());
        let text = fs::read_to_string(document_path(root.path(), DAY)).unwrap();
        assert!(
            text.contains("- readings with no named town nearby (Wyoming, United States), at 12:00: 1 reading from your iphone."),
            "{text}"
        );
    }

    #[test]
    fn an_unchanged_day_writes_nothing_on_replay() {
        let root = tempfile::tempdir().unwrap();
        seed(root.path());
        rebuild(root.path());
        let before = fs::metadata(document_path(root.path(), DAY))
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(rebuild(root.path()), Rebuilt::Unchanged);
        let after = fs::metadata(document_path(root.path(), DAY))
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn deleting_a_segment_rebuilds_from_what_survives_and_an_empty_day_loses_its_files() {
        let root = tempfile::tempdir().unwrap();
        seed(root.path());
        rebuild(root.path());
        let segments = root
            .path()
            .join("chronicle")
            .join(DAY)
            .join("phone_mobile-segment");
        fs::remove_dir_all(segments.join("150000_300")).unwrap();
        assert_eq!(rebuild(root.path()), Rebuilt::Written);
        let text = fs::read_to_string(document_path(root.path(), DAY)).unwrap();
        assert!(
            !text.contains("Denver"),
            "the deleted visit is gone: {text}"
        );
        fs::remove_dir_all(segments.join("140000_300")).unwrap();
        assert_eq!(rebuild(root.path()), Rebuilt::Removed);
        assert!(!document_path(root.path(), DAY).exists());
        assert!(!record_path(root.path(), DAY).exists());
    }

    #[test]
    fn a_reconcile_rebuilds_a_day_only_when_its_marker_moves() {
        let root = tempfile::tempdir().unwrap();
        seed(root.path());
        fs::create_dir_all(root.path().join("chronicle/20261002/laptop/090000_300")).unwrap();
        let mut reconciler = Reconciler::default();
        let pass = |reconciler: &mut Reconciler| {
            reconciler.reconcile(
                root.path(),
                tier(),
                &directory(),
                chrono_tz::America::Denver,
            )
        };
        let first = pass(&mut reconciler);
        assert_eq!((first.checked, first.written, first.removed), (2, 1, 0));
        assert!(first.failures.is_empty(), "{:?}", first.failures);
        assert!(!document_path(root.path(), "20261002").exists());
        assert_eq!(pass(&mut reconciler).written, 0);

        fs::remove_dir_all(
            root.path()
                .join("chronicle")
                .join(DAY)
                .join("phone_mobile-segment/150000_300"),
        )
        .unwrap();
        assert_eq!(
            pass(&mut reconciler).written,
            0,
            "the marker has not moved yet"
        );
        solstone_core_journal_io::bump_stream_marker(root.path(), DAY).unwrap();
        assert_eq!(pass(&mut reconciler).written, 1);
        let mut fresh = Reconciler::default();
        assert_eq!(
            pass(&mut fresh).written,
            0,
            "a restart trusts a current record"
        );
    }
}
