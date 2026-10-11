// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Fresh coverage evidence for the retained teardown admission.

use std::sync::Arc;

use solstone_core_backup::teardown_configuration::TeardownConfiguration;
use solstone_core_backup_runtime::TeardownReaderRefusal;
use solstone_core_journal_io::JournalRoot;
use solstone_core_retention::archive_catalog::read_complete_catalog;

/// Read both owning sources afresh inside `TeardownAdmission::check`.
/// A successful observation grants no deletion, cleanup or crash-safety authority.
/// Call again at each destructive boundary; config and lease checks stay in admission.
pub fn read_teardown_coverage(
    root: &Arc<JournalRoot>,
    _configuration: &TeardownConfiguration,
) -> Result<(), TeardownReaderRefusal> {
    read_complete_catalog(root).map_err(|_| TeardownReaderRefusal::CatalogUnavailable)?;
    let obligations = crate::recovery::observe_obligations(root)
        .map_err(|_| TeardownReaderRefusal::RecoveryUnavailable)?;
    if !obligations.entries().is_empty() {
        return Err(TeardownReaderRefusal::RecoveryHeld);
    }
    Ok(())
}

#[cfg(all(test, feature = "full-tests"))]
mod tests;
