// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use solstone_core_format::segment::segment_key;
use solstone_core_journal_io::is_day_key;
use solstone_core_journal_io::journal_root::JournalRoot;
use solstone_core_journal_io::media::MediaDirectory;
use solstone_core_segment::{OwnerDeletionState, SegmentDir, owner_deletion_state};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MarkerState {
    Live,
    Deleted,
    Staged,
    Occupied,
    Unreadable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EntryObservation {
    Present,
    Absent,
    Unreadable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RequestedSource {
    DerivedFile,
    TranscriptSegment,
    ActivityFile,
}

pub(crate) fn allows_source_release(
    kind: RequestedSource,
    marker: MarkerState,
    segment_directory: EntryObservation,
    derived_entry: EntryObservation,
) -> bool {
    match kind {
        RequestedSource::ActivityFile => derived_entry == EntryObservation::Present,
        RequestedSource::DerivedFile | RequestedSource::TranscriptSegment => {
            if marker == MarkerState::Unreadable
                || segment_directory == EntryObservation::Unreadable
            {
                return false;
            }
            if matches!(
                marker,
                MarkerState::Deleted | MarkerState::Staged | MarkerState::Occupied
            ) {
                return false;
            }
            if segment_directory == EntryObservation::Absent {
                return false;
            }
            match kind {
                RequestedSource::TranscriptSegment => true,
                RequestedSource::DerivedFile => derived_entry == EntryObservation::Present,
                RequestedSource::ActivityFile => unreachable!(),
            }
        }
    }
}

pub(crate) fn check_indexed_source_release(journal_root: &Path, path: &str) -> bool {
    let rel_path = path.strip_prefix("chronicle/").unwrap_or(path);
    let parts: Vec<&str> = rel_path.split('/').collect();

    // 1. Segment file: day-rooted path with at least 3 components whose third contains segment_key
    if parts.len() >= 3 && is_day_key(parts[0]) && segment_key(parts[2]).is_some() {
        let day = parts[0];
        let stream = parts[1];
        let segment = parts[2];
        let rel_leaf_parts = &parts[3..];
        if rel_leaf_parts.is_empty() {
            return false;
        }
        for comp in rel_leaf_parts {
            if *comp == ".." || comp.is_empty() {
                return false;
            }
        }
        let Ok(segment_dir) = SegmentDir::resolve(journal_root, day, segment, stream) else {
            return false;
        };
        let marker = match owner_deletion_state(segment_dir.path()) {
            Ok(OwnerDeletionState::Live) => MarkerState::Live,
            Ok(OwnerDeletionState::Deleted) => MarkerState::Deleted,
            Ok(OwnerDeletionState::Staged) => MarkerState::Staged,
            Ok(OwnerDeletionState::Occupied) => MarkerState::Occupied,
            Err(_) => MarkerState::Unreadable,
        };
        let Ok(root) = JournalRoot::open(journal_root).map(Arc::new) else {
            return allows_source_release(
                RequestedSource::DerivedFile,
                marker,
                EntryObservation::Unreadable,
                EntryObservation::Unreadable,
            );
        };
        let rel_segment_dir = segment_rel_dir(day, stream, segment);
        let segment_directory = match MediaDirectory::open(Arc::clone(&root), &rel_segment_dir) {
            Ok(media_dir) => (EntryObservation::Present, Some(media_dir)),
            Err(err) if err.kind() == io::ErrorKind::NotFound => (EntryObservation::Absent, None),
            Err(_) => (EntryObservation::Unreadable, None),
        };
        let derived_entry = if let Some(media_dir) = segment_directory.1 {
            observe_relative_derived_leaf(&media_dir, rel_leaf_parts)
        } else if segment_directory.0 == EntryObservation::Absent {
            EntryObservation::Absent
        } else {
            EntryObservation::Unreadable
        };
        return allows_source_release(
            RequestedSource::DerivedFile,
            marker,
            segment_directory.0,
            derived_entry,
        );
    }

    // 2. Activity file: exactly facets/<facet>/activities/<YYYYMMDD>.jsonl (4 components)
    if parts.len() == 4 && parts[0] == "facets" && parts[2] == "activities" {
        let Some(day) = parts[3].strip_suffix(".jsonl") else {
            return true;
        };
        if !is_day_key(day) {
            return true;
        }
        let facet = parts[1];
        if facet.is_empty() || facet == ".." {
            return false;
        }
        let Ok(root) = JournalRoot::open(journal_root).map(Arc::new) else {
            return allows_source_release(
                RequestedSource::ActivityFile,
                MarkerState::Live,
                EntryObservation::Present,
                EntryObservation::Unreadable,
            );
        };
        let parent_rel = format!("facets/{facet}/activities");
        let derived_entry = match MediaDirectory::open(root, Path::new(&parent_rel)) {
            Ok(media_dir) => match media_dir.read_prefix(OsStr::new(parts[3]), 1) {
                Ok(Some(_)) => EntryObservation::Present,
                Ok(None) => EntryObservation::Absent,
                Err(_) => EntryObservation::Unreadable,
            },
            Err(err) if err.kind() == io::ErrorKind::NotFound => EntryObservation::Absent,
            Err(_) => EntryObservation::Unreadable,
        };
        return allows_source_release(
            RequestedSource::ActivityFile,
            MarkerState::Live,
            EntryObservation::Present,
            derived_entry,
        );
    }

    // Any other path returns true with no filesystem access
    true
}

pub(crate) fn check_segment_transcript_release(
    journal_root: &Path,
    day: &str,
    stream: &str,
    segment: &str,
) -> bool {
    let Ok(segment_dir) = SegmentDir::resolve(journal_root, day, segment, stream) else {
        return false;
    };
    let marker = match owner_deletion_state(segment_dir.path()) {
        Ok(OwnerDeletionState::Live) => MarkerState::Live,
        Ok(OwnerDeletionState::Deleted) => MarkerState::Deleted,
        Ok(OwnerDeletionState::Staged) => MarkerState::Staged,
        Ok(OwnerDeletionState::Occupied) => MarkerState::Occupied,
        Err(_) => MarkerState::Unreadable,
    };
    let Ok(root) = JournalRoot::open(journal_root).map(Arc::new) else {
        return allows_source_release(
            RequestedSource::TranscriptSegment,
            marker,
            EntryObservation::Unreadable,
            EntryObservation::Absent,
        );
    };
    let rel_segment_dir = segment_rel_dir(day, stream, segment);
    let segment_directory = match MediaDirectory::open(root, &rel_segment_dir) {
        Ok(_) => EntryObservation::Present,
        Err(err) if err.kind() == io::ErrorKind::NotFound => EntryObservation::Absent,
        Err(_) => EntryObservation::Unreadable,
    };
    allows_source_release(
        RequestedSource::TranscriptSegment,
        marker,
        segment_directory,
        EntryObservation::Absent,
    )
}

fn segment_rel_dir(day: &str, stream: &str, segment: &str) -> PathBuf {
    if stream == solstone_core_journal_io::DEFAULT_STREAM || stream.is_empty() {
        PathBuf::from(format!("chronicle/{day}/{segment}"))
    } else {
        PathBuf::from(format!("chronicle/{day}/{stream}/{segment}"))
    }
}

fn observe_relative_derived_leaf(
    media_dir: &Arc<MediaDirectory>,
    rel_leaf_parts: &[&str],
) -> EntryObservation {
    let (leaf, ancestors) = match rel_leaf_parts.split_last() {
        Some(split) => split,
        None => return EntryObservation::Absent,
    };
    let mut current = Arc::clone(media_dir);
    for dir_name in ancestors {
        match current.open_child(OsStr::new(dir_name)) {
            Ok(Some(child)) => {
                current = child;
            }
            Ok(None) => return EntryObservation::Absent,
            Err(_) => return EntryObservation::Unreadable,
        }
    }
    match current.read_prefix(OsStr::new(leaf), 1) {
        Ok(Some(_)) => EntryObservation::Present,
        Ok(None) => EntryObservation::Absent,
        Err(_) => EntryObservation::Unreadable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pure_source_release_policy_table() {
        let cases = [
            // Row 1: Live derived entry with present file releases
            (
                RequestedSource::DerivedFile,
                MarkerState::Live,
                EntryObservation::Present,
                EntryObservation::Present,
                true,
            ),
            // Row 2: Live derived entry with absent file refuses
            (
                RequestedSource::DerivedFile,
                MarkerState::Live,
                EntryObservation::Present,
                EntryObservation::Absent,
                false,
            ),
            // Row 3: Live marker but absent segment directory refuses
            (
                RequestedSource::DerivedFile,
                MarkerState::Live,
                EntryObservation::Absent,
                EntryObservation::Absent,
                false,
            ),
            // Row 4: Deleted marker refuses derived file even if file present
            (
                RequestedSource::DerivedFile,
                MarkerState::Deleted,
                EntryObservation::Present,
                EntryObservation::Present,
                false,
            ),
            // Row 5: Staged marker refuses derived file
            (
                RequestedSource::DerivedFile,
                MarkerState::Staged,
                EntryObservation::Present,
                EntryObservation::Present,
                false,
            ),
            // Row 6: Occupied marker refuses derived file
            (
                RequestedSource::DerivedFile,
                MarkerState::Occupied,
                EntryObservation::Present,
                EntryObservation::Present,
                false,
            ),
            // Row 7: Unreadable marker refuses derived file
            (
                RequestedSource::DerivedFile,
                MarkerState::Unreadable,
                EntryObservation::Present,
                EntryObservation::Present,
                false,
            ),
            // Row 8: Unreadable segment directory refuses derived file
            (
                RequestedSource::DerivedFile,
                MarkerState::Live,
                EntryObservation::Unreadable,
                EntryObservation::Present,
                false,
            ),
            // Row 9: Unreadable derived file refuses derived file
            (
                RequestedSource::DerivedFile,
                MarkerState::Live,
                EntryObservation::Present,
                EntryObservation::Unreadable,
                false,
            ),
            // Row 10: Live transcript segment releases even if derived entry is absent
            (
                RequestedSource::TranscriptSegment,
                MarkerState::Live,
                EntryObservation::Present,
                EntryObservation::Absent,
                true,
            ),
            // Row 11: Live marker but absent segment directory refuses transcript segment
            (
                RequestedSource::TranscriptSegment,
                MarkerState::Live,
                EntryObservation::Absent,
                EntryObservation::Absent,
                false,
            ),
            // Row 12: Deleted marker refuses transcript segment
            (
                RequestedSource::TranscriptSegment,
                MarkerState::Deleted,
                EntryObservation::Present,
                EntryObservation::Present,
                false,
            ),
            // Row 13: Staged marker refuses transcript segment
            (
                RequestedSource::TranscriptSegment,
                MarkerState::Staged,
                EntryObservation::Present,
                EntryObservation::Present,
                false,
            ),
            // Row 14: Occupied marker refuses transcript segment
            (
                RequestedSource::TranscriptSegment,
                MarkerState::Occupied,
                EntryObservation::Present,
                EntryObservation::Present,
                false,
            ),
            // Row 15: Unreadable marker refuses transcript segment
            (
                RequestedSource::TranscriptSegment,
                MarkerState::Unreadable,
                EntryObservation::Present,
                EntryObservation::Present,
                false,
            ),
            // Row 16: Unreadable segment directory refuses transcript segment
            (
                RequestedSource::TranscriptSegment,
                MarkerState::Live,
                EntryObservation::Unreadable,
                EntryObservation::Present,
                false,
            ),
            // Row 17: Unreadable derived entry is ignored for transcript segment
            (
                RequestedSource::TranscriptSegment,
                MarkerState::Live,
                EntryObservation::Present,
                EntryObservation::Unreadable,
                true,
            ),
            // Row 18: Activity file ignores marker and segment directory; present releases
            (
                RequestedSource::ActivityFile,
                MarkerState::Deleted,
                EntryObservation::Absent,
                EntryObservation::Present,
                true,
            ),
            // Row 19: Activity file absent refuses
            (
                RequestedSource::ActivityFile,
                MarkerState::Live,
                EntryObservation::Present,
                EntryObservation::Absent,
                false,
            ),
            // Row 20: Activity file unreadable refuses
            (
                RequestedSource::ActivityFile,
                MarkerState::Live,
                EntryObservation::Present,
                EntryObservation::Unreadable,
                false,
            ),
        ];

        for (i, (kind, marker, segment_dir, derived_entry, expected)) in cases.iter().enumerate() {
            let actual = allows_source_release(*kind, *marker, *segment_dir, *derived_entry);
            assert_eq!(
                actual,
                *expected,
                "Case {}: allows_source_release({:?}, {:?}, {:?}, {:?}) = {}, expected {}",
                i + 1,
                kind,
                marker,
                segment_dir,
                derived_entry,
                actual,
                expected
            );
        }
    }
}
