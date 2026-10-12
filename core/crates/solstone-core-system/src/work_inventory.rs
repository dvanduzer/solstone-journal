// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! A reporting snapshot. Never consulted as evidence that work is complete.

use serde::{Deserialize, Serialize};
use solstone_core_journal_config::work_priority::WorkBand;
use std::path::Path;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BandProgress {
    pub owner_requests: u64,
    pub segments_complete: u64,
    pub segments_owed: u64,
    pub daily_units_complete: u64,
    pub daily_units_owed: u64,
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
        self.eligible += other.eligible;
        self.permission_held += other.permission_held;
        self.prerequisite_held += other.prerequisite_held;
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkHold {
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
}

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
        }
    }
}

impl WorkInventory {
    pub fn band(&mut self, band: WorkBand) -> &mut BandProgress {
        &mut self.bands[band as usize - 1]
    }
    pub fn publish(&self, journal: &Path) -> Result<(), String> {
        solstone_core_journal_io::write_json(
            journal.join("health/work-inventory.json"),
            self,
            solstone_core_journal_io::JsonWriteOptions::default(),
        )
        .map_err(|error| error.to_string())
    }
}

pub fn read(journal: &Path) -> Result<Option<WorkInventory>, String> {
    let bytes = match std::fs::read(journal.join("health/work-inventory.json")) {
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
