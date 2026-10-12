// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Source-derived import size. Coordinates are plans, never reservations or consent.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub struct Recording {
    pub day: String,
    pub stream: String,
    pub segment: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ImportSize {
    pub recordings: BTreeSet<Recording>,
    pub skipped: usize,
    /// Sources without chronicle recordings (for example body data) name their own unit.
    pub other_units: Option<(String, usize)>,
}

impl ImportSize {
    pub fn days(&self) -> BTreeSet<&str> {
        self.recordings
            .iter()
            .map(|recording| recording.day.as_str())
            .collect()
    }

    pub fn render(&self, json: bool) -> String {
        if json {
            format!(
                "{}\n",
                serde_json::json!({
                "schema": "solstone.import.size.v1",
                "recordings": self.recordings.len(),
                "days": self.days().len(),
                "skipped": self.skipped,
                "other_units": self.other_units,
                })
            )
        } else {
            let mut result = format!(
                "{} {} across {} {}\n",
                self.recordings.len(),
                if self.recordings.len() == 1 {
                    "recording"
                } else {
                    "recordings"
                },
                self.days().len(),
                if self.days().len() == 1 {
                    "day"
                } else {
                    "days"
                }
            );
            if let Some((unit, count)) = &self.other_units {
                result.push_str(&format!("{count} {unit}\n"));
            }
            if self.skipped > 0 {
                result.push_str(&format!("{} skipped\n", self.skipped));
            }
            result
        }
    }
}
