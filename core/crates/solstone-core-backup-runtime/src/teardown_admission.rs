// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Retained teardown admission. Fresh checks do not authorize deletion.

use std::sync::Arc;

use solstone_core_backup::teardown_configuration::{
    TeardownConfiguration, TeardownConfigurationRefusal,
};
use solstone_core_journal_config_write::{
    BoundConfigOutcome, BoundConfigPublication, BoundConfigRefusal,
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

    /// Clear through the original owner and retain the exact confirmed transition.
    pub fn clear_configuration(
        self,
        reader: &mut TeardownReader<'_>,
    ) -> Result<ClearedTeardownAdmission, TeardownClearFailure> {
        if let Err(refusal) = self.check(reader) {
            return Err(TeardownClearFailure::new(
                self,
                false,
                TeardownClearRefusal::Admission(refusal),
                None,
            ));
        }
        run_clear_hook(ClearCheckpoint::BeforeClear);
        let attempted = true;
        let outcome = self.configuration.clear_configuration();
        let outcome = match outcome {
            Err(refusal) => {
                return Err(TeardownClearFailure::new(
                    self,
                    attempted,
                    TeardownClearRefusal::Clear(Box::new(refusal)),
                    None,
                ));
            }
            Ok(outcome) => outcome,
        };
        if let Some(refusal) = outcome_refusal(&outcome) {
            let publication = match outcome {
                BoundConfigOutcome::Published(publication) => Some(publication),
                BoundConfigOutcome::Unchanged => None,
            };
            return Err(TeardownClearFailure::new(
                self,
                attempted,
                refusal,
                publication,
            ));
        }
        let BoundConfigOutcome::Published(publication) = outcome else {
            unreachable!("unchanged refused above")
        };
        run_clear_hook(ClearCheckpoint::AfterClear);
        if let Err(refusal) = self.revalidate_publication(&publication) {
            return Err(TeardownClearFailure::new(
                self,
                attempted,
                TeardownClearRefusal::Admission(refusal),
                Some(publication),
            ));
        }
        Ok(ClearedTeardownAdmission {
            admission: self,
            publication,
        })
    }

    fn revalidate_publication(
        &self,
        publication: &BoundConfigPublication,
    ) -> Result<(), TeardownAdmissionRefusal> {
        self.lease.revalidate().map_err(activity_refusal)?;
        self.configuration
            .revalidate_published_clear(publication)
            .map_err(TeardownAdmissionRefusal::Configuration)?;
        self.lease.revalidate().map_err(activity_refusal)
    }
}

/// Refusal facts, separate from whether the owned clear published anything.
pub enum TeardownClearRefusal {
    Admission(TeardownAdmissionRefusal),
    Clear(Box<BoundConfigRefusal>),
    Unchanged,
    PublicationUncertain,
}

impl std::fmt::Debug for TeardownClearRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Admission(refusal) => f.debug_tuple("Admission").field(refusal).finish(),
            Self::Clear(_) => f.write_str("Clear"),
            Self::Unchanged => f.write_str("Unchanged"),
            Self::PublicationUncertain => f.write_str("PublicationUncertain"),
        }
    }
}

/// Keeps exclusion and all observed effect facts until explicitly dropped.
pub struct TeardownClearFailure {
    admission: Box<TeardownAdmission>,
    attempted: bool,
    refusal: TeardownClearRefusal,
    publication: Option<Box<BoundConfigPublication>>,
}

impl TeardownClearFailure {
    fn new(
        admission: TeardownAdmission,
        attempted: bool,
        refusal: TeardownClearRefusal,
        publication: Option<BoundConfigPublication>,
    ) -> Self {
        Self {
            admission: Box::new(admission),
            attempted,
            refusal,
            publication: publication.map(Box::new),
        }
    }

    pub fn journal_root(&self) -> &Arc<JournalRoot> {
        self.admission.journal_root()
    }
    pub fn attempted(&self) -> bool {
        self.attempted
    }
    pub fn refusal(&self) -> &TeardownClearRefusal {
        &self.refusal
    }
    pub fn publication(&self) -> Option<&BoundConfigPublication> {
        self.publication.as_deref()
    }
}

impl std::fmt::Debug for TeardownClearFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TeardownClearFailure")
            .field("attempted", &self.attempted)
            .field("refusal", &self.refusal)
            .field("published", &self.publication.is_some())
            .finish()
    }
}

/// Original admission plus its own exact confirmed publication. No public factory.
pub struct ClearedTeardownAdmission {
    admission: TeardownAdmission,
    publication: BoundConfigPublication,
}

impl ClearedTeardownAdmission {
    pub fn journal_root(&self) -> &Arc<JournalRoot> {
        self.admission.journal_root()
    }
    pub fn configuration(&self) -> &TeardownConfiguration {
        self.admission.configuration()
    }
    pub fn publication(&self) -> &BoundConfigPublication {
        &self.publication
    }
    pub fn revalidate(&self) -> Result<(), TeardownAdmissionRefusal> {
        self.admission.revalidate_publication(&self.publication)
    }

    /// Fresh callback under the exact owned transition; drift takes precedence.
    pub fn check(&self, reader: &mut TeardownReader<'_>) -> Result<(), TeardownAdmissionRefusal> {
        self.revalidate()?;
        let result = reader(self.journal_root(), self.configuration());
        self.revalidate()?;
        result.map_err(TeardownAdmissionRefusal::Reader)
    }
}

fn outcome_refusal(outcome: &BoundConfigOutcome) -> Option<TeardownClearRefusal> {
    let BoundConfigOutcome::Published(publication) = outcome else {
        return Some(TeardownClearRefusal::Unchanged);
    };
    if publication.durability_uncertainty.is_some()
        || publication.observation_uncertainty.is_some()
        || publication.confirmation_refusal.is_some()
    {
        Some(TeardownClearRefusal::PublicationUncertain)
    } else {
        None
    }
}

#[derive(Clone, Copy, PartialEq)]
enum ClearCheckpoint {
    BeforeClear,
    AfterClear,
}

#[cfg(all(test, feature = "full-tests"))]
type ClearHook = (ClearCheckpoint, Box<dyn FnOnce()>);
#[cfg(all(test, feature = "full-tests"))]
thread_local! { static CLEAR_HOOK: std::cell::RefCell<Option<ClearHook>> = const { std::cell::RefCell::new(None) }; }

fn run_clear_hook(point: ClearCheckpoint) {
    #[cfg(all(test, feature = "full-tests"))]
    {
        let hook = CLEAR_HOOK.with(|slot| {
            let mut slot = slot.borrow_mut();
            if slot.as_ref().is_some_and(|(at, _)| *at == point) {
                slot.take()
            } else {
                None
            }
        });
        if let Some((_, hook)) = hook {
            hook();
        }
    }
    let _ = point;
}

fn activity_refusal(error: ActivityAdmissionError) -> TeardownAdmissionRefusal {
    match error {
        ActivityAdmissionError::Busy => TeardownAdmissionRefusal::ActivityBusy,
        ActivityAdmissionError::Unavailable(_) => TeardownAdmissionRefusal::ActivityUnavailable,
    }
}

#[cfg(all(test, feature = "full-tests"))]
mod clear_tests;
#[cfg(all(test, feature = "full-tests"))]
mod tests;
