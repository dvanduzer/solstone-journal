// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! The same source-derived prerequisites for discovery and daily-only execution.

use std::collections::BTreeMap;
use std::path::Path;

use crate::{DaySegment, SegmentIdentity, SegmentProgress};

fn directory(journal: &Path, day: &str, segment: &DaySegment) -> std::path::PathBuf {
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

pub fn segment_work_ready(
    journal: &Path,
    day: &str,
    segment: &DaySegment,
    progress: &BTreeMap<SegmentIdentity, SegmentProgress>,
) -> Result<bool, String> {
    if segment.stream == "mcp.agent" {
        return Ok(true);
    }
    if !crate::segment_requires_processing(&segment.clone().into()) {
        return Ok(true);
    }
    let directory = directory(journal, day, segment);
    let newest = crate::newest_segment_input_ms(&directory).map_err(|error| error.to_string())?;
    Ok(crate::segment_fully_sensed(&segment.data_state)
        && crate::segment_thinking_is_current(
            crate::lookup_segment_progress(progress, &segment.stream, &segment.key),
            newest,
        )
        && solstone_core_system::segment_publication::is_current(journal, &directory, newest)?)
}

pub fn day_segments_ready(journal: &Path, day: &str) -> Result<bool, String> {
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
    for segment in segments {
        if !segment_work_ready(journal, day, &segment, &progress.value)? {
            return Ok(false);
        }
    }
    Ok(true)
}
