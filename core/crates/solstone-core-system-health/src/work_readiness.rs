// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! The same source-derived prerequisites for discovery and daily-only execution.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::{DaySegment, SegmentIdentity, SegmentProgress};

fn directory(journal: &Path, day: &str, segment: &DaySegment) -> PathBuf {
    match segment.stream_layout {
        solstone_core_journal_io::SegmentLayout::Direct => {
            journal.join("chronicle").join(day).join(&segment.key)
        }
        solstone_core_journal_io::SegmentLayout::Named => journal
            .join("chronicle")
            .join(day)
            .join(&segment.stream)
            .join(&segment.key),
    }
}

/// How far a segment's work has come.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentReadiness {
    /// Its thinking covers its input and its saved outputs are published.
    Ready,
    /// Its thinking covers its input; only the publication of its saved
    /// outputs is owed, which needs no model call.
    Unpublished,
    /// Its input is still being sensed or its thinking is not current.
    Owed,
}

pub fn segment_readiness(
    journal: &Path,
    day: &str,
    segment: &DaySegment,
    progress: &BTreeMap<SegmentIdentity, SegmentProgress>,
) -> Result<SegmentReadiness, String> {
    if segment.stream == "mcp.agent" {
        return Ok(SegmentReadiness::Ready);
    }
    if !crate::segment_requires_processing(&segment.clone().into()) {
        return Ok(SegmentReadiness::Ready);
    }
    let directory = directory(journal, day, segment);
    let newest = crate::newest_segment_input_ms(&directory).map_err(|error| error.to_string())?;
    if !crate::segment_fully_sensed(&segment.data_state)
        || !crate::segment_thinking_is_current(
            crate::lookup_segment_progress(progress, &segment.stream, &segment.key),
            newest,
        )
    {
        return Ok(SegmentReadiness::Owed);
    }
    Ok(
        if solstone_core_system::segment_publication::is_current(journal, &directory, newest)? {
            SegmentReadiness::Ready
        } else {
            SegmentReadiness::Unpublished
        },
    )
}

pub fn segment_work_ready(
    journal: &Path,
    day: &str,
    segment: &DaySegment,
    progress: &BTreeMap<SegmentIdentity, SegmentProgress>,
) -> Result<bool, String> {
    Ok(segment_readiness(journal, day, segment, progress)? == SegmentReadiness::Ready)
}

/// The directories of a day's thought segments whose saved outputs are not
/// yet published, or `None` while any of its segments still owes thinking.
/// Daily work publishes these itself rather than waiting for them.
pub fn day_unpublished_segments(journal: &Path, day: &str) -> Result<Option<Vec<PathBuf>>, String> {
    let (_, _, segments) = crate::scan_day(
        &crate::FilesystemSegmentSource,
        journal,
        day,
        chrono::Utc::now(),
    )
    .map_err(|error| error.to_string())?;
    let progress =
        crate::read_segment_progress(&crate::FilesystemHealthLogSource::new(journal), day)
            .map_err(|error| error.to_string())?;
    if progress.malformed_line_count != 0 {
        return Err("segment progress contains unreadable records".into());
    }
    let mut unpublished = Vec::new();
    for segment in segments {
        match segment_readiness(journal, day, &segment, &progress.value)? {
            SegmentReadiness::Ready => {}
            SegmentReadiness::Unpublished => unpublished.push(directory(journal, day, &segment)),
            SegmentReadiness::Owed => return Ok(None),
        }
    }
    Ok(Some(unpublished))
}

pub fn day_segments_ready(journal: &Path, day: &str) -> Result<bool, String> {
    Ok(day_unpublished_segments(journal, day)?.is_some_and(|unpublished| unpublished.is_empty()))
}
