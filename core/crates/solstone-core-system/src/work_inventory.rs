// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! A reporting snapshot. Never consulted as evidence that work is complete.

use serde::{Deserialize, Serialize};
use solstone_core_journal_config::work_priority::WorkBand;
use solstone_core_journal_io::durability::{ArtifactId, artifact};
use std::path::Path;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BandProgress {
    pub owner_requests: u64,
    pub segments_complete: u64,
    pub segments_owed: u64,
    pub daily_units_complete: u64,
    pub daily_units_owed: u64,
    #[serde(default)]
    pub activities_owed: u64,
    pub eligible: u64,
    pub permission_held: u64,
    pub prerequisite_held: u64,
}

impl BandProgress {
    pub fn add(&mut self, other: &Self) {
        self.owner_requests += other.owner_requests;
        self.segments_complete += other.segments_complete;
        self.segments_owed += other.segments_owed;
        self.daily_units_complete += other.daily_units_complete;
        self.daily_units_owed += other.daily_units_owed;
        self.activities_owed += other.activities_owed;
        self.eligible += other.eligible;
        self.permission_held += other.permission_held;
        self.prerequisite_held += other.prerequisite_held;
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkHold {
    #[serde(default)]
    pub retry_unit: Option<String>,
    pub day: String,
    pub segment: Option<String>,
    pub stream: Option<String>,
    pub band: WorkBand,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkInventory {
    pub version: u32,
    pub as_of_ms: i64,
    pub complete: bool,
    pub days_total: usize,
    pub days_inspected: usize,
    pub oldest_day: Option<String>,
    pub newest_day: Option<String>,
    pub bands: [BandProgress; 6],
    pub holds: Vec<WorkHold>,
    pub unreadable: Vec<String>,
    /// When discovery will look again. A finished pass stays current until then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_pass_ms: Option<i64>,
}

/// How long a snapshot stays current past its pass or its promised next one.
pub const CURRENT_FOR_MS: i64 = 180_000;

impl Default for WorkInventory {
    fn default() -> Self {
        Self {
            version: 1,
            as_of_ms: chrono::Utc::now().timestamp_millis(),
            complete: false,
            days_total: 0,
            days_inspected: 0,
            oldest_day: None,
            newest_day: None,
            bands: std::array::from_fn(|_| BandProgress::default()),
            holds: Vec::new(),
            unreadable: Vec::new(),
            next_pass_ms: None,
        }
    }
}

impl WorkInventory {
    pub fn band(&mut self, band: WorkBand) -> &mut BandProgress {
        &mut self.bands[band as usize - 1]
    }
    /// Whether a reader may treat this snapshot as discovery's current view.
    pub fn current_at(&self, now_ms: i64) -> bool {
        let until = self
            .next_pass_ms
            .unwrap_or(self.as_of_ms)
            .max(self.as_of_ms);
        now_ms >= self.as_of_ms && now_ms - until < CURRENT_FOR_MS
    }
    pub fn publish(&self, journal: &Path) -> Result<(), String> {
        solstone_core_journal_io::write_json(
            journal.join(artifact(ArtifactId::WorkInventory).path),
            self,
            solstone_core_journal_io::JsonWriteOptions::default(),
        )
        .map_err(|error| error.to_string())
    }
}

pub fn read(journal: &Path) -> Result<Option<WorkInventory>, String> {
    let bytes = match std::fs::read(journal.join(artifact(ArtifactId::WorkInventory).path)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    let report: WorkInventory =
        serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
    if report.version != 1 {
        return Err("unrecognized work inventory".into());
    }
    Ok(Some(report))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_finished_pass_stays_current_until_the_next_one_is_due() {
        let mut report = WorkInventory {
            as_of_ms: 1_000_000,
            ..WorkInventory::default()
        };
        assert!(report.current_at(1_000_000 + CURRENT_FOR_MS - 1));
        assert!(!report.current_at(1_000_000 + CURRENT_FOR_MS));
        assert!(!report.current_at(999_999));
        report.next_pass_ms = Some(1_000_000 + 3_600_000);
        assert!(report.current_at(1_000_000 + 3_600_000 + CURRENT_FOR_MS - 1));
        assert!(!report.current_at(1_000_000 + 3_600_000 + CURRENT_FOR_MS));
    }
}
