// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Reading a segment directory into the facts the release predicate decides on.
//!
//! [`eligibility`](crate::eligibility) deliberately takes what the caller found
//! rather than reading disk itself, so the decision is testable without a
//! filesystem. This is the caller: the one place that turns a directory into
//! [`FoundContent`].
//!
//! # ⛔ The marker key is read from the second row only
//!
//! A sidecar's first row is always the metadata header. Segment-wide metadata is
//! merged into that header, so a stray key there — `start` on an audio sidecar, say
//! — would make a **header-only** file look as though it carried transcript rows,
//! and the raw would be released with nothing standing in for it.
//!
//! The two reference implementations disagree about this. The general-purpose row
//! probe accepts the key on *either* of the first two rows; the retention path
//! narrows it to the second, with a comment naming exactly this hazard. This follows
//! retention, because this is the caller that deletes.
//!
//! # ⚠ Reads are bounded, which narrows against the reference
//!
//! The reference's retention path streams lines with no byte limit, so a sidecar
//! whose header is larger than the bound here would yield it a record and yields
//! none here. That holds the raw instead of releasing it — the same direction as
//! every other narrowing in this crate, and the reason a bound is affordable.
//!
//! ⚠ A truncated final line cannot parse, so exceeding the bound can only lose
//! evidence, never manufacture it.

use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

use serde_json::Value;
use solstone_core_journal_io::paths::{DirEntryKind, list_dir_entries};
use solstone_core_processing_record::{analysis_row_key, vocab};

use crate::content::{ContentName, HandlerRegistry, MediaClassifier};
use crate::eligibility::{FoundContent, SidecarFacts};

/// The record key a sidecar header carries.
const RECORD_KEY: &str = "_solstone_processing";

/// The extension every sidecar wears.
const SIDECAR_EXTENSION: &str = "jsonl";

/// How much of a sidecar may be read to find its first two rows.
pub const READ_BOUND: u64 = 2 * vocab::MAX_FIRST_ROW_BYTES as u64;

/// The sidecar filename for a content file.
///
/// ⛔ Replaces the final extension rather than appending, matching both reference
/// implementations: `chunk_audio.flac` is described by `chunk_audio.jsonl`.
pub fn sidecar_name(content: &ContentName) -> Option<String> {
    let (stem, _) = content.as_str().rsplit_once('.')?;
    if stem.is_empty() {
        return None;
    }
    Some(format!("{stem}.{SIDECAR_EXTENSION}"))
}

/// The first two non-blank lines of a file, within a fixed byte budget.
fn first_two_rows(path: &Path) -> Vec<String> {
    let Ok(mut file) = File::open(path) else {
        return Vec::new();
    };
    let mut buffer = Vec::new();
    if file
        .by_ref()
        .take(READ_BOUND)
        .read_to_end(&mut buffer)
        .is_err()
    {
        return Vec::new();
    }
    let Ok(text) = std::str::from_utf8(&buffer) else {
        return Vec::new();
    };
    text.split('\n')
        .filter(|line| !line.trim().is_empty())
        .take(2)
        .map(str::to_owned)
        .collect()
}

/// Read one sidecar into the facts the predicate needs.
///
/// `marker_key` is the row key that proves this modality's analysis rows exist;
/// `None` means no handler claims the content, so no row can prove anything.
pub fn read_sidecar(path: &Path, marker_key: Option<&str>) -> SidecarFacts {
    sidecar_facts(&first_two_rows(path), marker_key)
}

fn sidecar_facts(rows: &[String], marker_key: Option<&str>) -> SidecarFacts {
    let header = rows
        .first()
        .and_then(|line| serde_json::from_str::<Value>(line).ok());
    let record = header
        .as_ref()
        .and_then(|row| row.get(RECORD_KEY))
        .filter(|value| value.is_object())
        .cloned();

    // ⛔ Row 1 is the header and is never consulted for the marker key. See the
    // module note: a stray key there would make a header-only file look
    // chunk-bearing.
    let has_analysis_row = match (marker_key, rows.get(1)) {
        (Some(key), Some(line)) => serde_json::from_str::<Value>(line)
            .ok()
            .and_then(|row| row.as_object().map(|row| row.contains_key(key)))
            .unwrap_or(false),
        _ => false,
    };

    SidecarFacts {
        record,
        has_analysis_row,
    }
}

/// Read every media file in a segment directory, with its sidecar's facts.
///
/// Emits one entry per file the classifier calls owner media. ⛔ Symlinks and
/// directories are not content: `list_dir_entries` reports kinds without following
/// links, and anything that is not a plain file is skipped, so a symlink named like
/// media can never be released.
pub fn scan_segment(
    segment: &Path,
    registry: &dyn HandlerRegistry,
    classifier: &dyn MediaClassifier,
) -> Vec<FoundContent> {
    let Ok(entries) = list_dir_entries(segment) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for entry in entries {
        if entry.kind != DirEntryKind::File {
            continue;
        }
        let Some(name) = entry.name.to_str().and_then(ContentName::new) else {
            continue;
        };
        if !classifier.is_owner_media(&name) {
            continue;
        }
        // ⛔ The size on disk, read from the entry this loop is about to propose
        // deleting -- not a size recorded anywhere else.
        let Ok(size) = entry.path.metadata().map(|meta| meta.len()) else {
            continue;
        };
        let marker = registry
            .expected_handler(&name)
            .and_then(analysis_row_key)
            .map(str::to_owned);
        let sidecar = match sidecar_name(&name) {
            Some(sidecar) => read_sidecar(&segment.join(sidecar), marker.as_deref()),
            None => SidecarFacts::default(),
        };
        found.push(FoundContent {
            name,
            size,
            sidecar,
        });
    }
    found
}

/// A complete read observation, without release or archive authority.
/// Callers needing a stable decision must also hold writer and segment exclusion.
pub struct CheckedSegment {
    pub(crate) found: Vec<FoundContent>,
}

impl CheckedSegment {
    pub fn files(&self) -> &[FoundContent] {
        &self.found
    }
}

fn invalid_observation() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "segment observation is incomplete",
    )
}

fn checked_sidecar(path: &Path, marker: Option<&str>) -> io::Result<SidecarFacts> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(SidecarFacts::default());
        }
        Err(error) => return Err(error),
    };
    if !metadata.is_file() {
        return Err(invalid_observation());
    }
    let mut file = File::open(path)?;
    if !file.metadata()?.is_file() {
        return Err(invalid_observation());
    }
    let mut buffer = Vec::new();
    file.by_ref().take(READ_BOUND).read_to_end(&mut buffer)?;
    let text = std::str::from_utf8(&buffer).map_err(|_| invalid_observation())?;
    let rows = text
        .split('\n')
        .filter(|line| !line.trim().is_empty())
        .take(2)
        .map(str::to_owned)
        .collect::<Vec<_>>();
    Ok(sidecar_facts(&rows, marker))
}

/// Read all owner-media siblings, refusing missing directories and read errors.
/// Unlike the soft scanner, an unreadable sibling cannot disappear from a group.
pub fn scan_segment_checked(
    segment: &Path,
    registry: &dyn HandlerRegistry,
    classifier: &dyn MediaClassifier,
) -> io::Result<CheckedSegment> {
    if !std::fs::symlink_metadata(segment)?.is_dir() {
        return Err(invalid_observation());
    }
    let mut found = Vec::new();
    for entry in std::fs::read_dir(segment)? {
        let entry = entry?;
        let filename = entry.file_name();
        let name = filename.to_str().ok_or_else(invalid_observation)?;
        let Some(name) = ContentName::new(name) else {
            return Err(invalid_observation());
        };
        if !classifier.is_owner_media(&name) {
            continue;
        }
        let metadata = std::fs::symlink_metadata(entry.path())?;
        if !metadata.is_file() {
            return Err(invalid_observation());
        }
        let marker = registry.expected_handler(&name).and_then(analysis_row_key);
        let sidecar = match sidecar_name(&name) {
            Some(sidecar) => checked_sidecar(&segment.join(sidecar), marker)?,
            None => SidecarFacts::default(),
        };
        found.push(FoundContent {
            name,
            size: metadata.len(),
            sidecar,
        });
    }
    found.sort_by(|left, right| left.name.as_str().cmp(right.name.as_str()));
    Ok(CheckedSegment { found })
}
