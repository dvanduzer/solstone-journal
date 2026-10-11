// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Fresh coverage evidence for the retained teardown admission.

use std::sync::Arc;

use solstone_core_backup::teardown_configuration::TeardownConfiguration;
use solstone_core_backup_runtime::TeardownReaderRefusal;
use solstone_core_journal_io::JournalRoot;
use solstone_core_journal_io::activity_obligations::{observe_obligations, observe_with_carrier};
use solstone_core_journal_io::media::RootCarrierObservation;
use solstone_core_retention::archive_catalog::read_complete_catalog;

/// Read both owning sources afresh inside `TeardownAdmission::check`.
/// A successful observation grants no deletion, cleanup or crash-safety authority.
/// Call again at each destructive boundary; config and lease checks stay in admission.
pub fn read_teardown_coverage(
    root: &Arc<JournalRoot>,
    _configuration: &TeardownConfiguration,
    carrier: Option<RootCarrierObservation<'_>>,
) -> Result<(), TeardownReaderRefusal> {
    read_complete_catalog(root).map_err(|_| TeardownReaderRefusal::CatalogUnavailable)?;
    let held = match carrier {
        Some(carrier) => observe_with_carrier(root, carrier)
            .map_err(|_| TeardownReaderRefusal::RecoveryUnavailable)?
            .unresolved_entries()
            .next()
            .is_some(),
        None => !observe_obligations(root)
            .map_err(|_| TeardownReaderRefusal::RecoveryUnavailable)?
            .entries()
            .is_empty(),
    };
    if held {
        return Err(TeardownReaderRefusal::RecoveryHeld);
    }
    Ok(())
}

#[cfg(all(test, feature = "full-tests"))]
mod tests;
