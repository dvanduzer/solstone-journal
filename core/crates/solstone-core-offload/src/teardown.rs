// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Fresh coverage evidence for the retained teardown admission.

use std::sync::Arc;

use solstone_core_backup::teardown_configuration::TeardownConfiguration;
use solstone_core_backup_runtime::TeardownReaderRefusal;
use solstone_core_journal_io::JournalRoot;
use solstone_core_journal_io::activity_obligations::{observe_obligations, observe_with_carrier};
use solstone_core_journal_io::media::RootCarrierObservation;
use solstone_core_journal_io::paths::SegmentLayout;
use solstone_core_retention::archive_catalog::read_complete_catalog;
use solstone_core_segment::{OwnerDeletionState, owner_deletion_state};

use crate::measurement::measure_local_media;
use crate::status::{CatalogLocalPresence, CatalogMediaObservation, observe_catalog_media};

mod owned;
pub use owned::{
    OwnedTeardownOutcome, SettledTeardownReport, TeardownAttemptJournal, TeardownRunPhase,
    TeardownRunRefusal, run_owned_teardown,
};

/// Read both owning sources afresh inside `TeardownAdmission::check`.
/// A successful observation grants no deletion, cleanup or crash-safety authority.
/// Call again at each destructive boundary; config and lease checks stay in admission.
///
/// Refuses while any active archive witness lacks a same-size local file, so the
/// backup is never deleted while it may hold the only copy of owner media. The one
/// exception is media in a segment the owner deleted: restore never brings it back.
pub fn read_teardown_coverage(
    root: &Arc<JournalRoot>,
    _configuration: &TeardownConfiguration,
    carrier: Option<RootCarrierObservation<'_>>,
) -> Result<(), TeardownReaderRefusal> {
    let catalog =
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
    let observed = observe_catalog_media(&catalog, &measure_local_media(root.canonical_path()));
    // A degraded catalog may omit witnesses, so its absence of backup-only media proves nothing.
    if observed.catalog_degraded {
        return Err(TeardownReaderRefusal::CatalogUnavailable);
    }
    let covered = observed
        .files
        .iter()
        .all(|file| locally_covered(root, file));
    // The scan above is by path; it only describes this root if the path still names it.
    root.revalidate_canonical_binding()
        .map_err(|_| TeardownReaderRefusal::CatalogUnavailable)?;
    if covered {
        Ok(())
    } else {
        Err(TeardownReaderRefusal::BackupOnlyHeld)
    }
}

/// Unbound and unknown presence stay uncovered; only an exact size match counts.
fn locally_covered(root: &JournalRoot, file: &CatalogMediaObservation) -> bool {
    match file.presence {
        CatalogLocalPresence::Present { local_bytes } => local_bytes == file.witness.file.bytes,
        CatalogLocalPresence::Absent => owner_deleted_segment(root, file),
        CatalogLocalPresence::Unknown { .. } | CatalogLocalPresence::Unbound => false,
    }
}

/// Only a recorded owner deletion counts; staged, occupied or unreadable markers do not.
fn owner_deleted_segment(root: &JournalRoot, file: &CatalogMediaObservation) -> bool {
    let (Some(layout), Some(segment_dir)) = (file.witness.layout, &file.witness.segment_dir) else {
        return false;
    };
    let day = root.canonical_path().join("chronicle").join(&file.day);
    let path = match layout {
        SegmentLayout::Direct => day.join(segment_dir),
        SegmentLayout::Named => day.join(&file.stream).join(segment_dir),
    };
    matches!(owner_deletion_state(&path), Ok(OwnerDeletionState::Deleted))
}

#[cfg(all(test, feature = "full-tests"))]
mod tests;
