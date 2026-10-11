// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Retained teardown admission. Fresh checks do not authorize deletion.

use std::sync::Arc;

use solstone_core_backup::teardown_configuration::{
    TeardownConfiguration, TeardownConfigurationRefusal,
};
use solstone_core_journal_io::JournalRoot;
use solstone_core_journal_io::activity::{ActivityAdmissionError, TeardownLease};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TeardownReaderRefusal {
    CatalogUnavailable,
    RecoveryUnavailable,
    RecoveryHeld,
}

pub type TeardownReader<'a> =
    dyn FnMut(&Arc<JournalRoot>, &TeardownConfiguration) -> Result<(), TeardownReaderRefusal> + 'a;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TeardownAdmissionRefusal {
    Unsupported,
    ActivityBusy,
    ActivityUnavailable,
    Configuration(TeardownConfigurationRefusal),
    Reader(TeardownReaderRefusal),
}

/// The original activity lease and immutable configuration observation.
/// No serialized or cached successful check can construct this admission.
pub struct TeardownAdmission {
    lease: Arc<TeardownLease>,
    configuration: TeardownConfiguration,
}

/// Admit observation only. Destructive execution requires additional boundaries.
pub fn admit_teardown(
    root: Arc<JournalRoot>,
) -> Result<TeardownAdmission, TeardownAdmissionRefusal> {
    #[cfg(unix)]
    {
        let lease = TeardownLease::try_acquire(root).map_err(activity_refusal)?;
        let configuration = TeardownConfiguration::capture(lease.journal_root())
            .map_err(TeardownAdmissionRefusal::Configuration)?;
        let admitted = TeardownAdmission {
            lease,
            configuration,
        };
        admitted.revalidate()?;
        Ok(admitted)
    }
    #[cfg(not(unix))]
    {
        let _ = root;
        Err(TeardownAdmissionRefusal::Unsupported)
    }
}

impl TeardownAdmission {
    pub fn journal_root(&self) -> &Arc<JournalRoot> {
        self.lease.journal_root()
    }

    pub fn configuration(&self) -> &TeardownConfiguration {
        &self.configuration
    }

    pub fn revalidate(&self) -> Result<(), TeardownAdmissionRefusal> {
        self.lease.revalidate().map_err(activity_refusal)?;
        self.configuration
            .revalidate()
            .map_err(TeardownAdmissionRefusal::Configuration)?;
        self.lease.revalidate().map_err(activity_refusal)
    }

    /// Invoke a fresh reader once under the original admission, with no cached verdict.
    /// Postcheck admission refusal takes precedence over a simultaneous reader refusal.
    pub fn check(&self, reader: &mut TeardownReader<'_>) -> Result<(), TeardownAdmissionRefusal> {
        self.revalidate()?;
        let result = reader(self.journal_root(), self.configuration());
        self.revalidate()?;
        result.map_err(TeardownAdmissionRefusal::Reader)
    }
}

fn activity_refusal(error: ActivityAdmissionError) -> TeardownAdmissionRefusal {
    match error {
        ActivityAdmissionError::Busy => TeardownAdmissionRefusal::ActivityBusy,
        ActivityAdmissionError::Unavailable(_) => TeardownAdmissionRefusal::ActivityUnavailable,
    }
}

#[cfg(all(test, feature = "full-tests"))]
mod tests;
