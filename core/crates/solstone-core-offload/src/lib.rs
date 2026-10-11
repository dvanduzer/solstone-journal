// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Native media-offload runtime authorities.

pub mod ledger;
pub mod marks;
pub mod measurement;
pub mod pruning_audit;
pub mod recovery;
pub mod restore;
// Private, uncalled coordination while durable intent publication is held.
#[allow(dead_code)]
mod restore_intent;
#[allow(dead_code)]
mod restore_intent_store;
#[allow(dead_code)]
mod restore_prepared;
pub mod run;
pub mod status;
pub mod teardown;

pub use ledger::{
    append_offload_event, append_restore_event, ledger_path_for_day, summarize_day,
    summarize_journal, summarize_segment,
};
pub use marks::OffloadMarkIndex;
pub use measurement::suggest_offload_defaults;
pub use restore::{RestoreResult, restore_all_offload, restore_offload_day};
pub use run::{OffloadResult, format_offload_result, run_offload};
pub use status::OffloadStatus;
