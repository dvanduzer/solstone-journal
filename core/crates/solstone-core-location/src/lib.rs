// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Location in the journal: reading what the phones send, naming it at area
//! level from the bundled area tier, and the one writer of location-derived
//! files.
//!
//! Everything here runs on the journal. Naming is a lookup in a file that
//! ships byte-for-byte the same to every journal, so no service is asked
//! where anyone was.
//!
//! This crate owns two derived files per day (see [`projection`]):
//! `chronicle/<day>/location.md` and `chronicle/<day>/health/location.json`.
//! The captured `location.jsonl` in each segment is never written here.

pub mod accuracy;
pub mod area;
pub mod asset;
pub mod day;
pub mod install;
pub mod pack;
pub mod projection;
pub mod record;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub use area::{AreaName, AreaTier, Boundary, Bounds, TownPoint};
pub use asset::area_tier;
pub use day::{DayLocation, DeviceDirectory, Entry, SegmentContext, SourceRef, read_day};
pub use projection::{Reconciler, rebuild_day};

/// Device names from the journal itself: each segment's upload record, and the
/// description a paired client gave of itself.
pub struct JournalDirectory {
    descriptions: BTreeMap<String, (Option<String>, Option<String>)>,
}

impl JournalDirectory {
    pub fn read(journal: &Path) -> Self {
        let descriptions = solstone_core_sol_link::read_descriptions(journal)
            .unwrap_or_default()
            .into_iter()
            .map(|(cid, description)| {
                let reported = description.reported.as_ref();
                let name = description
                    .owner_label
                    .clone()
                    .or_else(|| reported.and_then(|reported| reported.name.clone()))
                    .filter(|name| !name.trim().is_empty());
                let device_type = reported.and_then(|reported| reported.device_type.clone());
                (cid, (name, device_type))
            })
            .collect();
        Self { descriptions }
    }
}

impl DeviceDirectory for JournalDirectory {
    fn segment_context(&self, segment_dir: &Path) -> SegmentContext {
        let Ok(report) = solstone_core_callosum::read_device_ingest_events(segment_dir) else {
            return SegmentContext::default();
        };
        let Some(record) = report.records.last() else {
            return SegmentContext::default();
        };
        let zone = report
            .records
            .iter()
            .rev()
            .find_map(solstone_core_callosum::DeviceIngestEvent::reported_zone);
        SegmentContext {
            cid: Some(record.cid.clone()).filter(|cid| !cid.is_empty()),
            platform: record
                .meta
                .get("platform")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            tz: zone.as_ref().and_then(|zone| zone.tz.clone()),
            utc_offset_seconds: zone.and_then(|zone| zone.utc_offset_seconds),
        }
    }

    fn describe(&self, cid: &str) -> (Option<String>, Option<String>) {
        self.descriptions.get(cid).cloned().unwrap_or((None, None))
    }
}

/// Read one day with the journal's own device names and the owner's zone.
pub fn read_journal_day(journal: &Path, day: &str) -> Result<DayLocation, area::AreaTierError> {
    let tier = area_tier()?;
    let packs = accuracy::installed_packs(journal);
    Ok(read_day(
        journal,
        day,
        tier,
        packs.as_deref(),
        &JournalDirectory::read(journal),
        solstone_core_journal_config::owner_zone(journal),
    ))
}

/// One reconcile pass over the whole journal with its own device names.
pub fn reconcile_journal(
    journal: &Path,
    reconciler: &mut Reconciler,
) -> Result<projection::ReconcileReport, area::AreaTierError> {
    let tier = area_tier()?;
    let packs = accuracy::installed_packs(journal);
    Ok(reconciler.reconcile(
        journal,
        tier,
        packs.as_deref(),
        &JournalDirectory::read(journal),
        solstone_core_journal_config::owner_zone(journal),
    ))
}

/// Days that hold location, from their build records, with reading counts.
pub fn list_location_days(journal: &Path) -> Vec<(String, u64)> {
    let mut days: Vec<(String, u64)> = solstone_core_journal_io::day_dirs(journal)
        .unwrap_or_default()
        .into_keys()
        .filter_map(|day| {
            let record = projection::read_build_record(journal, &day)?;
            Some((day, (record.readings + record.visits).max(1)))
        })
        .collect();
    days.sort();
    days
}

/// The installed or checkout path of the area tier, for diagnostics.
pub fn area_tier_path() -> Result<PathBuf, asset::AssetError> {
    asset::resolve_area_tier_path()
}
