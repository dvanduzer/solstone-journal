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
//!
//! Each of the two rows is judged on its own bytes ([`read_leading_rows`]): a
//! character cut by the bound, or anything past the second row, never refuses
//! or hides them. Bytes in those two rows that are not UTF-8 still refuse a
//! checked observation, and still yield no evidence to a path scan.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, BufReader, Read};
use std::path::Path;

use solstone_core_journal_io::journal_root::JournalEntryKind;
use solstone_core_journal_io::media::{MediaDirectory, MediaEntry};
use solstone_core_journal_io::paths::{DirEntryKind, list_dir_entries};
use solstone_core_processing_record::{LeadingRow, analysis_row_key, read_leading_rows, vocab};

use crate::content::{ContentName, HandlerRegistry, MediaClassifier};
use crate::eligibility::{FoundContent, SidecarFacts};

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

/// The first two nonblank rows of a file, within a fixed byte budget.
///
/// `None` when the file cannot be read, or those rows are not UTF-8.
fn first_two_rows(path: &Path, marker_key: Option<&str>) -> Option<Vec<LeadingRow>> {
    let file = File::open(path).ok()?;
    let rows = read_leading_rows(BufReader::new(file.take(READ_BOUND)), marker_key).ok()?;
    (!rows.contains(&LeadingRow::NotUtf8)).then_some(rows)
}

/// Read one sidecar into the facts the predicate needs.
///
/// `marker_key` is the row key that proves this modality's analysis rows exist;
/// `None` means no handler claims the content, so no row can prove anything.
pub fn read_sidecar(path: &Path, marker_key: Option<&str>) -> SidecarFacts {
    first_two_rows(path, marker_key)
        .map(|rows| sidecar_facts(&rows, marker_key))
        .unwrap_or_default()
}

fn sidecar_facts(rows: &[LeadingRow], marker_key: Option<&str>) -> SidecarFacts {
    let record = match rows.first() {
        Some(LeadingRow::Object { record, .. }) => record.clone(),
        _ => None,
    };

    // ⛔ Row 1 is the header and is never consulted for the marker key. See the
    // module note: a stray key there would make a header-only file look
    // chunk-bearing.
    let has_analysis_row = marker_key.is_some()
        && matches!(rows.get(1), Some(LeadingRow::Object { has_key: true, .. }));

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
    pub(crate) bound_entries: BTreeMap<ContentName, MediaEntry>,
}

impl CheckedSegment {
    pub fn files(&self) -> &[FoundContent] {
        &self.found
    }

    /// The exact bound raw entry, including its native change stamp. Path scans
    /// return None. This is read evidence, without hash or release authority.
    pub fn bound_entry(&self, name: &ContentName) -> Option<&MediaEntry> {
        self.bound_entries.get(name)
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
    checked_sidecar_facts(&buffer, marker)
}

fn checked_sidecar_facts(buffer: &[u8], marker: Option<&str>) -> io::Result<SidecarFacts> {
    let rows = read_leading_rows(buffer, marker)?;
    if rows.contains(&LeadingRow::NotUtf8) {
        return Err(invalid_observation());
    }
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
    Ok(CheckedSegment {
        found,
        bound_entries: BTreeMap::new(),
    })
}

/// Read every owner-media sibling through an already admitted native directory.
///
/// The caller supplies the complete-listing limit and holds any required writer
/// and segment exclusion. Overflow, source changes and binding uncertainty
/// refuse the whole observation. Long sidecars need only a stable READ_BOUND
/// prefix; raw files are opened with a zero-byte prefix and are never hashed.
///
/// This point-in-time evidence does not grant P or release authority. A caller
/// joining a separate raw digest must compare fresh zero-prefix entries before
/// and after hashing with `bound_entry`, including the private native stamp.
pub fn scan_segment_bound(
    segment: &MediaDirectory,
    registry: &dyn HandlerRegistry,
    classifier: &dyn MediaClassifier,
    maximum_entries: usize,
) -> io::Result<CheckedSegment> {
    let entries = segment
        .entries(maximum_entries)?
        .ok_or_else(invalid_observation)?;
    let by_name = entries
        .iter()
        .map(|entry| (entry.name.as_os_str(), entry))
        .collect::<BTreeMap<_, _>>();
    let mut found = Vec::new();
    let mut bound_entries = BTreeMap::new();
    for entry in &entries {
        let name = entry.name.to_str().ok_or_else(invalid_observation)?;
        let name = ContentName::new(name).ok_or_else(invalid_observation)?;
        if !classifier.is_owner_media(&name) {
            continue;
        }
        if entry.kind != JournalEntryKind::RegularFile {
            return Err(invalid_observation());
        }
        let raw = segment
            .read_prefix(&entry.name, 0)?
            .ok_or_else(invalid_observation)?;
        if raw.entry != *entry {
            return Err(invalid_observation());
        }
        let marker = registry.expected_handler(&name).and_then(analysis_row_key);
        let sidecar = match sidecar_name(&name) {
            Some(sidecar) => match by_name.get(OsStr::new(&sidecar)) {
                Some(listed) => {
                    if listed.kind != JournalEntryKind::RegularFile {
                        return Err(invalid_observation());
                    }
                    let prefix = segment
                        .read_prefix(
                            OsStr::new(&sidecar),
                            usize::try_from(READ_BOUND).map_err(|_| invalid_observation())?,
                        )?
                        .ok_or_else(invalid_observation)?;
                    if prefix.entry != **listed {
                        return Err(invalid_observation());
                    }
                    checked_sidecar_facts(&prefix.bytes, marker)?
                }
                None => SidecarFacts::default(),
            },
            None => SidecarFacts::default(),
        };
        bound_entries.insert(name.clone(), raw.entry);
        found.push(FoundContent {
            name,
            size: entry.size,
            sidecar,
        });
    }
    let final_entries = segment
        .entries(maximum_entries)?
        .ok_or_else(invalid_observation)?;
    if final_entries != entries {
        return Err(invalid_observation());
    }
    segment.revalidate()?;
    found.sort_by(|left, right| left.name.as_str().cmp(right.name.as_str()));
    Ok(CheckedSegment {
        found,
        bound_entries,
    })
}
